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
    /// The URI that the daemon put in the key, as far as this run knows.
    /// `None` while the key is still the user's.
    pub(crate) written: Option<String>,
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
        self.pause == Some(Pause::ExternalChange) || self.reassert.is_some()
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
    /// as the daemon knew it.
    pub(crate) fn change(&self, value: &str) -> Option<KeyChange> {
        let written = self.written.as_deref()?;
        match (self.is_foreign(), value == written) {
            (false, false) => Some(KeyChange::Taken),
            (true, true) => Some(KeyChange::Returned),
            (false, true) | (true, false) => None,
        }
    }

    /// Whether the daemon must write `wanted` to the key now.
    pub(crate) fn owes(&self, wanted: &str) -> bool {
        match (self.pause, self.reassert) {
            (Some(_), _) => false,
            (None, Some(Reassert::Now)) => true,
            (None, Some(Reassert::At(_) | Reassert::AtNextTick)) => false,
            (None, None) => self.written.as_deref() != Some(wanted),
        }
    }

    /// Records that the daemon wrote `uri` to the key.
    pub(crate) fn record_write(&mut self, uri: String) {
        self.written = Some(uri);
        self.reassert = None;
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
    /// daemon's to write back meanwhile.
    pub(crate) fn lock_out(&mut self) {
        self.paused_under_lock = self.is_paused_by_change();
        self.pause = Some(Pause::LockedKey);
        self.reassert = None;
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

        assert_eq!(key.change("file:///picked.jpg"), None);
    }

    #[test]
    fn another_value_than_the_written_one_takes_the_key() {
        let key = written("file:///slot");

        assert_eq!(key.change("file:///picked.jpg"), Some(KeyChange::Taken));
        assert_eq!(key.change("file:///slot"), None);
    }

    #[test]
    fn a_taken_key_is_taken_once_and_returns_once() {
        let mut key = written("file:///slot");
        key.pause();

        assert_eq!(key.change("file:///other.jpg"), None);
        assert_eq!(key.change("file:///slot"), Some(KeyChange::Returned));
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
