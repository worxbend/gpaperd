//! One style's background key: what the daemon wrote to it, and what the
//! daemon does once something else changes it.
//!
//! See "External changes" and "Locked or missing keys" in the plan's
//! "Applying wallpapers" section.

use std::time::{Duration, SystemTime};

use crate::event::Pause;

/// What the daemon knows of one style's background key.
#[derive(Debug, Clone, Default)]
pub(crate) struct KeyState {
    /// The URI that the daemon put in the key, as far as this run knows, or
    /// tries again after a failed write. `None` while the key is still the
    /// user's.
    pub(crate) written: Option<String>,
    /// What `written` was before the last write, or before the failed write
    /// that a retry repeats: what the key still holds when GNOME refuses it.
    pub(crate) before_write: Option<String>,
    /// The daemon wrote `written`, and GNOME has reported no new value of
    /// the key since. A report sent before the write still carries what
    /// GNOME reported before, which is no news.
    pub(crate) awaits_report: bool,
    /// Why the style idles.
    pub(crate) pause: Option<Pause>,
    /// Something else changed the key, and the daemon is to write it back.
    pub(crate) reassert: Option<Reassert>,
    /// When `reassert-now` saw the key change, oldest first. Only the last
    /// minute counts.
    pub(crate) flips: Vec<SystemTime>,
    /// An external-change pause held when the lock pause replaced it. It
    /// holds again once the lock is lifted, and it is saved meanwhile.
    pub(crate) paused_under_lock: bool,
}

/// When a key that something else changed is written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reassert {
    /// With the next key write.
    Now,
    /// At this instant.
    At(SystemTime),
    /// At this instant, because the last key write did not land. `written`
    /// stays on record. The key holds what GNOME reported before meanwhile:
    /// another value of it is an external change.
    Retry(SystemTime),
    /// With the next key write, because the wait of a `Retry` is over. The
    /// write is the daemon's own, not an answer to an external change: the
    /// key still holds what GNOME reported before, and another value is one.
    RetryNow,
    /// When the next rotation of any style is due.
    AtNextTick,
}

/// What a new value of a key means to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyChange {
    /// Something else changed the key that the daemon wrote.
    Taken,
    /// The key holds the daemon's URI again.
    Returned,
}

impl KeyState {
    /// Whether something else changed the key after the daemon wrote it,
    /// and the daemon has not written it back.
    pub(crate) fn is_foreign(&self) -> bool {
        let reasserts = matches!(
            self.reassert,
            Some(Reassert::Now | Reassert::At(_) | Reassert::AtNextTick)
        );
        self.pause == Some(Pause::ExternalChange) || reasserts
    }

    /// Whether the key cannot show the style in this run. The daemon then
    /// leaves the style alone.
    pub(crate) fn is_disabled(&self) -> bool {
        matches!(self.pause, Some(Pause::LockedKey | Pause::NoKey))
    }

    /// Whether an external-change pause holds, or waits under a lock: what
    /// the state file saves as `paused`.
    pub(crate) fn is_paused_by_change(&self) -> bool {
        match self.pause {
            Some(Pause::ExternalChange) => true,
            Some(Pause::LockedKey) => self.paused_under_lock,
            _ => false,
        }
    }

    /// What it means that the key holds `value` now. `None` when that
    /// changes nothing: the key was never the daemon's in this run, or it is
    /// as the daemon knew it. During a retry, or before GNOME reports the
    /// last write, the key may still hold what GNOME `reported` before,
    /// which is no news. During a retry the daemon's URI is `Returned`: an
    /// unconfirmed write landed late, so the retry is called off.
    pub(crate) fn change(&self, value: &str, reported: Option<&str>) -> Option<KeyChange> {
        let written = self.written.as_deref()?;
        let is_retry = self.waits_for_retry();
        let is_written = value == written;
        let may_hold_reported = is_retry || self.awaits_report;
        if may_hold_reported && !is_written && reported == Some(value) {
            return None;
        }
        match (self.is_foreign(), is_written) {
            (false, false) => Some(KeyChange::Taken),
            (true, true) => Some(KeyChange::Returned),
            (false, true) if is_retry => Some(KeyChange::Returned),
            (false, true) | (true, false) => None,
        }
    }

    /// The daemon's URI that the key holds, as far as this run knows: during
    /// a retry, the one on record before the write that failed.
    pub(crate) fn landed_uri(&self) -> Option<&str> {
        if self.waits_for_retry() {
            self.before_write.as_deref()
        } else {
            self.written.as_deref()
        }
    }

    fn waits_for_retry(&self) -> bool {
        matches!(self.reassert, Some(Reassert::Retry(_) | Reassert::RetryNow))
    }

    /// GNOME reported `value` for the key, after `reported` before. A new
    /// value ends the wait for the report of the last write.
    pub(crate) fn hear(&mut self, value: &str, reported: Option<&str>) {
        if reported != Some(value) {
            self.awaits_report = false;
        }
    }

    /// Plans to write the key back, in place of a retry that waits: that
    /// write never landed.
    pub(crate) fn plan_write_back(&mut self, reassert: Reassert) {
        self.drop_retry();
        self.reassert = Some(reassert);
    }

    /// Calls off a retry that waits. The failed write never landed, so the
    /// key holds the URI on record before it.
    fn drop_retry(&mut self) {
        if self.waits_for_retry() {
            self.written = self.before_write.clone();
        }
    }

    /// Whether the daemon must write `wanted` to the key now.
    pub(crate) fn owes(&self, wanted: &str) -> bool {
        match (self.pause, self.reassert) {
            (Some(_), _) => false,
            (None, Some(Reassert::Now | Reassert::RetryNow)) => true,
            (None, Some(Reassert::At(_) | Reassert::Retry(_) | Reassert::AtNextTick)) => false,
            (None, None) => self.written.as_deref() != Some(wanted),
        }
    }

    /// Records that the daemon wrote `uri` to the key. A retry keeps
    /// `before_write`: the write that failed never replaced it.
    pub(crate) fn record_write(&mut self, uri: String) {
        let previous = self.written.replace(uri);
        if self.reassert != Some(Reassert::RetryNow) {
            self.before_write = previous;
        }
        self.reassert = None;
        self.awaits_report = true;
    }

    /// Lets a key that waits for the next tick be written back with the next
    /// key write.
    pub(crate) fn end_tick_wait(&mut self) {
        if self.reassert == Some(Reassert::AtNextTick) {
            self.reassert = Some(Reassert::Now);
        }
    }

    /// The key is the daemon's again: an external change is over.
    pub(crate) fn settle(&mut self) {
        if self.pause == Some(Pause::ExternalChange) {
            self.pause = None;
        }
        self.reassert = None;
    }

    /// Stops the style for as long as an administrator keeps its key locked
    /// on a value that the daemon cannot use. That replaces an external
    /// change, which waits until the lock is lifted: the key is not the
    /// daemon's to write back meanwhile. Once the lock is lifted, the value
    /// GNOME reported is compared with the URI on record.
    pub(crate) fn lock_out(&mut self) {
        self.paused_under_lock = self.is_paused_by_change();
        self.pause = Some(Pause::LockedKey);
        self.drop_retry();
        self.reassert = None;
        self.awaits_report = false;
    }

    /// The lock pause is over. The external-change pause that it replaced
    /// holds again.
    pub(crate) fn unlock(&mut self) {
        let was_paused = std::mem::take(&mut self.paused_under_lock);
        self.pause = was_paused.then_some(Pause::ExternalChange);
    }

    /// Stops the style because something else changed its key.
    pub(crate) fn pause(&mut self) {
        self.pause = Some(Pause::ExternalChange);
        self.drop_retry();
        self.reassert = None;
    }

    /// Ends what an external change began, because the config was loaded
    /// again: the key is written back at once.
    pub(crate) fn resume(&mut self) {
        let waits_under_lock = std::mem::take(&mut self.paused_under_lock);
        if self.is_foreign() || waits_under_lock {
            self.settle();
            self.reassert = Some(Reassert::Now);
        }
        self.flips.clear();
    }

    /// The key is the user's again: `gpaperctl restore` wrote the original
    /// value back. Until the daemon writes it once more, no value of it is
    /// an external change.
    pub(crate) fn hand_back(&mut self) {
        self.written = None;
        self.paused_under_lock = false;
        self.settle();
        self.flips.clear();
    }

    /// Counts an external change at `now`, and forgets those that are
    /// `window` old.
    pub(crate) fn note_flip(&mut self, now: SystemTime, window: Duration) {
        // A flip from after `now` is from before a clock step: it counts no
        // more.
        let is_recent = |at: &SystemTime| now.duration_since(*at).is_ok_and(|age| age < window);
        self.flips.retain(is_recent);
        self.flips.push(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs)
    }

    fn written(uri: &str) -> KeyState {
        KeyState {
            written: Some(uri.to_owned()),
            ..KeyState::default()
        }
    }

    #[test]
    fn a_key_the_daemon_never_wrote_cannot_be_taken() {
        let key = KeyState::default();

        assert_eq!(key.change("file:///picked.jpg", None), None);
    }

    #[test]
    fn another_value_than_the_written_one_takes_the_key() {
        let key = written("file:///slot");

        assert_eq!(
            key.change("file:///picked.jpg", None),
            Some(KeyChange::Taken)
        );
        assert_eq!(key.change("file:///slot", None), None);
    }

    #[test]
    fn a_taken_key_is_taken_once_and_returns_once() {
        let mut key = written("file:///slot");
        key.pause();

        assert_eq!(key.change("file:///other.jpg", None), None);
        assert_eq!(key.change("file:///slot", None), Some(KeyChange::Returned));
    }

    #[test]
    fn a_key_waiting_for_a_retry_can_be_taken_and_landed() {
        let mut key = written("file:///slot");
        key.reassert = Some(Reassert::Retry(at(60)));
        let reported = Some("file:///before");

        assert_eq!(
            key.change("file:///picked.jpg", reported),
            Some(KeyChange::Taken)
        );
        assert_eq!(
            key.change("file:///slot", reported),
            Some(KeyChange::Returned)
        );
        assert!(!key.owes("file:///slot"));
    }

    #[test]
    fn a_key_waiting_for_a_retry_still_holds_what_gnome_reported_before() {
        let mut key = written("file:///before");
        key.record_write("file:///refused".to_owned());
        key.reassert = Some(Reassert::Retry(at(60)));
        let reported = Some("file:///before");

        assert_eq!(key.change("file:///before", reported), None);
        assert_eq!(
            key.change("file:///picked.jpg", reported),
            Some(KeyChange::Taken)
        );
        assert_eq!(key.landed_uri(), Some("file:///before"));
    }

    #[test]
    fn a_due_retry_is_the_daemons_own_write_and_no_external_change() {
        let mut key = written("file:///refused");
        key.reassert = Some(Reassert::RetryNow);
        let reported = Some("file:///before");

        assert!(!key.is_foreign());
        assert!(key.owes("file:///refused"));
        assert_eq!(key.change("file:///before", reported), None);
        assert_eq!(
            key.change("file:///picked.jpg", reported),
            Some(KeyChange::Taken)
        );
    }

    #[test]
    fn a_report_sent_before_the_write_is_no_news_until_gnome_reports_anew() {
        let mut key = written("file:///before");
        key.record_write("file:///written".to_owned());
        let reported = Some("file:///before");

        assert_eq!(key.change("file:///before", reported), None);
        key.hear("file:///before", reported);
        key.hear("file:///written", reported);

        assert_eq!(
            key.change("file:///before", Some("file:///written")),
            Some(KeyChange::Taken)
        );
    }

    #[test]
    fn a_retry_called_off_puts_the_uri_before_the_failed_write_back_on_record() {
        let mut locked = written("file:///before");
        locked.record_write("file:///refused".to_owned());
        locked.reassert = Some(Reassert::Retry(at(60)));
        let mut taken = locked.clone();

        locked.lock_out();
        locked.unlock();
        taken.pause();

        assert_eq!(
            locked.change("file:///before", Some("file:///before")),
            None
        );
        assert_eq!(locked.landed_uri(), Some("file:///before"));
        assert_eq!(
            taken.change("file:///before", Some("file:///other")),
            Some(KeyChange::Returned)
        );
    }

    #[test]
    fn only_a_wait_for_the_next_tick_ends_with_end_tick_wait() {
        let mut ticking = written("file:///slot");
        ticking.reassert = Some(Reassert::AtNextTick);
        let mut retrying = written("file:///slot");
        retrying.reassert = Some(Reassert::Retry(at(60)));

        ticking.end_tick_wait();
        retrying.end_tick_wait();

        assert_eq!(ticking.reassert, Some(Reassert::Now));
        assert_eq!(retrying.reassert, Some(Reassert::Retry(at(60))));
    }

    #[test]
    fn a_key_is_owed_until_it_holds_the_wanted_uri() {
        let mut key = KeyState::default();
        assert!(key.owes("file:///slot"));

        key.record_write("file:///slot".to_owned());

        assert!(!key.owes("file:///slot"));
        assert!(key.owes("file:///image.jpg"));
    }

    #[test]
    fn a_key_that_was_written_back_is_not_owed_again() {
        let mut key = written("file:///slot");
        key.reassert = Some(Reassert::Now);

        key.record_write("file:///slot".to_owned());

        assert!(!key.owes("file:///slot"));
    }

    #[test]
    fn a_paused_or_waiting_key_is_not_owed() {
        let mut paused = written("file:///slot");
        paused.pause();
        let mut waiting = written("file:///slot");
        waiting.reassert = Some(Reassert::AtNextTick);

        assert!(!paused.owes("file:///image.jpg"));
        assert!(!waiting.owes("file:///image.jpg"));
    }

    #[test]
    fn a_resumed_key_is_written_back_even_with_the_same_uri() {
        let mut key = written("file:///slot");
        key.pause();

        key.resume();

        assert!(key.owes("file:///slot"));
        assert_eq!(key.pause, None);
    }

    #[test]
    fn a_reload_does_not_touch_a_key_that_is_the_daemons() {
        let mut key = written("file:///slot");

        key.resume();

        assert!(!key.owes("file:///slot"));
    }

    #[test]
    fn a_resumed_key_starts_its_count_of_flips_over() {
        let mut key = written("file:///slot");
        key.note_flip(at(0), WINDOW);

        key.resume();

        assert!(key.flips.is_empty());
    }

    #[test]
    fn a_disabled_key_stays_disabled() {
        let mut key = KeyState {
            pause: Some(Pause::LockedKey),
            ..KeyState::default()
        };

        key.resume();
        key.settle();

        assert!(key.is_disabled());
        assert!(!key.owes("file:///slot"));
    }

    #[test]
    fn only_flips_within_the_window_count() {
        let mut key = KeyState::default();

        key.note_flip(at(0), WINDOW);
        key.note_flip(at(59), WINDOW);
        key.note_flip(at(60), WINDOW);

        assert_eq!(key.flips, [at(59), at(60)]);
    }

    #[test]
    fn flips_from_before_a_clock_step_back_are_forgotten() {
        let mut key = KeyState::default();
        key.note_flip(at(500), WINDOW);

        key.note_flip(at(100), WINDOW);

        assert_eq!(key.flips, [at(100)]);
    }
}
