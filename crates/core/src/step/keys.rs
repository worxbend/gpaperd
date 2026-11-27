//! The reducer's side of the background keys: which keys a run can use,
//! what it does when something else changes one, and the key writes.

use std::time::Duration;

use super::{KEY_WRITE_RETRY, MAX_KEY_FLIPS_PER_MINUTE, REASSERT_DELAY, Turn};
use crate::config::{ApplyMode, Config, ExternalChangePolicy, PictureOptions};
use crate::event::{BackgroundKeys, Effect, GnomeSnapshot};
use crate::key::{KeyChange, Reassert};
use crate::style::{PerStyle, Style};

/// The minute that [`MAX_KEY_FLIPS_PER_MINUTE`] counts over.
const KEY_FLIP_WINDOW: Duration = Duration::from_secs(60);

impl Turn<'_> {
    /// Start-up: finds out what this run can do with each key, and whether
    /// a style that the last run left paused has its key back.
    pub(super) fn adopt_keys(&mut self, gnome: &GnomeSnapshot) {
        self.model.locks = gnome.locked;
        self.model.gnome_keys = Some(gnome.keys.clone());
        for style in Style::ALL {
            self.model.adopt_key(style, gnome.keys.uri(style));
        }
        self.follow_keys(&gnome.keys);
    }

    /// GNOME's report that the background keys changed: first what the
    /// daemon wrote to them, then what their locks allow.
    pub(super) fn on_keys_changed(&mut self, keys: BackgroundKeys) {
        let previous = self.model.gnome_keys.clone();
        self.follow_keys(&keys);
        self.announce_landed_writes(&keys, previous.as_ref());
        self.model.gnome_keys = Some(keys);
        self.follow_locks();
    }

    /// GNOME's report that an administrator locked or unlocked a key.
    pub(super) fn on_locks_changed(&mut self, locks: PerStyle<bool>) {
        self.model.locks = locks;
        self.follow_locks();
    }

    /// Starts or ends the lock pause of each style whose key is locked on a
    /// value it cannot use, or no longer is. A style that goes on rotates
    /// at once when it is overdue, and fills its slot when that is empty. A
    /// key that is free again may hold someone else's value by now: that is
    /// an external change.
    pub(super) fn follow_locks(&mut self) {
        let Some(keys) = self.model.gnome_keys.clone() else {
            return;
        };
        let mut is_changed = false;
        for style in Style::ALL {
            if let Some(value) = keys.uri(style) {
                is_changed |= self.model.follow_lock(style, value);
            }
        }
        if !is_changed {
            return;
        }
        self.follow_keys(&keys);
        for style in Style::ALL {
            self.model.follow_visibility(style, self.now);
        }
        // A lock replaced an external change, which is saved.
        self.persist();
        self.fill_empty_slots();
    }

    /// Compares what the background keys hold with what the daemon wrote to
    /// them. A key that this run cannot use is left out.
    pub(super) fn follow_keys(&mut self, keys: &BackgroundKeys) {
        for style in Style::ALL {
            let key = &self.model.styles[style].key;
            if key.is_disabled() {
                continue;
            }
            let change = keys.uri(style).and_then(|value| key.change(value));
            match change {
                Some(KeyChange::Taken) => self.on_external_change(style),
                Some(KeyChange::Returned) => {
                    self.model.styles[style].key.settle();
                    self.persist();
                }
                None => {}
            }
        }
    }

    /// Something else changed `style`'s key: `on_external_change` says what
    /// to do about it.
    pub(super) fn on_external_change(&mut self, style: Style) {
        match self.model.config().general.on_external_change {
            ExternalChangePolicy::Pause => self.pause(style),
            ExternalChangePolicy::ReassertNextTick => self.reassert_at_next_tick(style),
            ExternalChangePolicy::ReassertNow => self.reassert_soon(style),
        }
    }

    pub(super) fn pause(&mut self, style: Style) {
        self.model.styles[style].key.pause();
        self.persist();
    }

    pub(super) fn reassert_at_next_tick(&mut self, style: Style) {
        // A style with nothing to rotate to has no tick. A running one gets
        // a fresh interval. A hidden one cannot keep its clock running.
        if !self.model.has_other_candidate(style) && self.model.is_running(style) {
            self.model.restart_clock(style, self.now);
        }
        // `AtNextTick` only fires from a clock that is due. A hidden style
        // whose clock is stopped never gets that, so the write waits one
        // interval of its own unless some other style will tick.
        let reassert = if self.waits_on_its_own(style) {
            let due = self
                .now
                .checked_add(self.model.config().styles[style].interval);
            due.map_or(Reassert::Now, Reassert::At)
        } else {
            Reassert::AtNextTick
        };
        self.model.styles[style].key.reassert = Some(reassert);
    }

    /// A hidden style has no clock of its own. It needs a deadline when it
    /// has nothing to rotate to, and when no other style will tick either.
    fn waits_on_its_own(&self, style: Style) -> bool {
        if self.model.is_running(style) {
            return false;
        }
        !self.model.has_other_candidate(style) || !self.another_style_ticks(style)
    }

    fn another_style_ticks(&self, style: Style) -> bool {
        let other = style.other();
        let state = &self.model.styles[other];
        self.model.is_running(other)
            && self.model.has_other_candidate(other)
            && state.key.pause.is_none()
            && !state.user_paused
            && !self.model.is_held_by_lock()
    }

    /// The GNOME write failed. The keys still hold what they held. Each style
    /// whose recorded URI is still the one that failed goes back to the URI
    /// on record before it, and is tried again after [`KEY_WRITE_RETRY`]. An
    /// `unconfirmed` write keeps that URI on record instead: when it lands
    /// late, the retry is called off. A style that already moved on is left
    /// alone. A failure that names only `picture-options` arms the same
    /// retry on every key this run still holds. The failed options are owed
    /// again only while the running config still asks for them.
    pub(super) fn on_key_write_failed(
        &mut self,
        uris: PerStyle<Option<String>>,
        picture_options: Option<PictureOptions>,
        unconfirmed: bool,
    ) {
        let due = self.now.checked_add(KEY_WRITE_RETRY);
        let reassert = due.map_or(Reassert::Now, Reassert::Retry);
        let mut matched = false;
        for style in Style::ALL {
            let Some(uri) = uris[style].as_deref() else {
                continue;
            };
            let key = &mut self.model.styles[style].key;
            if key.written.as_deref() == Some(uri) {
                // An unconfirmed write may still land. Its report then settles
                // the key, and a later change of it stays an external change.
                if !unconfirmed {
                    key.written = key.before_write.clone();
                }
                key.reassert = Some(reassert);
                matched = true;
            }
        }
        let options_only =
            picture_options.is_some() && Style::ALL.into_iter().all(|style| uris[style].is_none());
        if options_only {
            self.arm_held_keys(reassert);
            matched = true;
        }
        let still_owed = picture_options.is_some_and(|options| self.config_owes(options));
        if matched && still_owed && self.model.picture_options_owed.is_none() {
            self.model.picture_options_owed = picture_options;
        }
    }

    /// Whether the running config still asks for `options`: a reload may
    /// have dropped it while the failed write was out. Without a value of its
    /// own the config still owes the `zoom` that replaces `none`.
    fn config_owes(&self, options: PictureOptions) -> bool {
        let configured = self.model.config().general.picture_options;
        configured.map_or(options == PictureOptions::Zoom, |wanted| wanted == options)
    }

    /// Arms [`KEY_WRITE_RETRY`] on every key this run still holds and is not
    /// already waiting. `written` stays: the key holds that URI, so a user
    /// change during the wait is still an external change.
    fn arm_held_keys(&mut self, reassert: Reassert) {
        for style in Style::ALL {
            let key = &mut self.model.styles[style].key;
            let already_waits = matches!(key.reassert, Some(Reassert::At(_) | Reassert::Retry(_)));
            if key.written.is_some() && key.pause.is_none() && !already_waits {
                key.before_write = key.written.clone();
                key.reassert = Some(reassert);
            }
        }
    }

    /// Plans the write-back of `style`'s key, [`REASSERT_DELAY`] from now.
    /// A key that changed more than [`MAX_KEY_FLIPS_PER_MINUTE`] times
    /// within a minute is fought over by another tool: its style is paused
    /// instead.
    pub(super) fn reassert_soon(&mut self, style: Style) {
        let key = &mut self.model.styles[style].key;
        key.note_flip(self.now, KEY_FLIP_WINDOW);
        if key.flips.len() > MAX_KEY_FLIPS_PER_MINUTE as usize {
            return self.pause(style);
        }
        let due = self.now.checked_add(REASSERT_DELAY);
        key.reassert = Some(due.map_or(Reassert::Now, Reassert::At));
    }

    /// Lets every key whose wait under `reassert-now` is over be written
    /// back.
    pub(super) fn end_reassert_waits(&mut self) {
        for style in Style::ALL {
            let key = &mut self.model.styles[style].key;
            if let Some(Reassert::At(due) | Reassert::Retry(due)) = key.reassert
                && due <= self.now
            {
                key.reassert = Some(Reassert::Now);
            }
        }
    }

    /// Writes every key that does not hold what it must, in one write. A
    /// key that names a missing file blanks the desktop, so the key of a
    /// style with nothing to show stays as it is.
    pub(super) fn write_owed_keys(&mut self) {
        // Every key write is a fade on screen: wait for what is in flight,
        // so that the keys go out together, after the images they name.
        // Without a config file the keys stay the user's.
        if self.model.is_busy() || self.model.awaits_config() {
            return;
        }
        let uris = PerStyle::from_fn(|style| self.model.owed_uri(style).map(str::to_owned));
        let writes_a_uri = Style::ALL.iter().any(|&style| uris[style].is_some());
        // A retry already armed must not send `picture-options` again in
        // this step: that is the loop a refused options write would start.
        let retries_later = Style::ALL.iter().any(|&style| {
            matches!(
                self.model.styles[style].key.reassert,
                Some(Reassert::At(_) | Reassert::Retry(_))
            )
        });
        // `picture-options` alone is written only next to a key that is the
        // daemon's: without one, the background is none of its business.
        let picture_options = if writes_a_uri || (self.model.holds_a_key() && !retries_later) {
            self.model.picture_options_owed.take()
        } else {
            None
        };
        if !writes_a_uri && picture_options.is_none() {
            return;
        }
        // GNOME reports this write like any other change. A key that is on
        // record as written is not owed again, so the report is not answered
        // with another write.
        for style in Style::ALL {
            if let Some(uri) = &uris[style] {
                self.model.styles[style].key.record_write(uri.clone());
            }
        }
        // The user's original keys must be on disk before they are replaced.
        self.persist();
        self.effects.push(Effect::SetKeys {
            uris,
            picture_options,
        });
    }

    /// Uri mode: `Changed` names an image once the shell reports the URI
    /// this run wrote, and that URI is not the one the shell already had.
    /// The plan to write is not that report. Slots mode announces the publish.
    fn announce_landed_writes(&mut self, keys: &BackgroundKeys, previous: Option<&BackgroundKeys>) {
        if self.model.config().general.apply_mode != ApplyMode::Uri {
            return;
        }
        for style in Style::ALL {
            let Some(reported) = keys.uri(style) else {
                continue;
            };
            let landed = self.model.styles[style].key.written.as_deref() == Some(reported);
            let previous_uri = previous.and_then(|held| held.uri(style));
            if !landed || previous_uri == Some(reported) {
                continue;
            }
            if let Some(item) = self.model.styles[style].current.clone() {
                self.effects.push(Effect::Changed { style, item });
            }
        }
    }
}

/// The `picture-options` value to write with the slot URIs. `none` makes the
/// shell load no file at all, so it is never left in place.
pub(super) fn picture_options_to_write(
    config: &Config,
    keys: &BackgroundKeys,
) -> Option<PictureOptions> {
    let shell_loads_no_file = keys.picture_options == "none";
    config
        .general
        .picture_options
        .or(shell_loads_no_file.then_some(PictureOptions::Zoom))
}
