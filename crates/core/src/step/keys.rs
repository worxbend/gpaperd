//! The reducer's side of the background keys: which keys a run can use,
//! what it does when something else changes one, and the key writes.

use std::time::Duration;

use super::{MAX_KEY_FLIPS_PER_MINUTE, REASSERT_DELAY, Turn};
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
        self.follow_keys(&keys);
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
        // A style with nothing to rotate to has no tick. It gets one, a
        // whole interval from now.
        if !self.model.has_other_candidate(style) {
            self.model.restart_clock(style, self.now);
        }
        self.model.styles[style].key.reassert = Some(Reassert::AtNextTick);
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
            if let Some(Reassert::At(due)) = key.reassert
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
        // `picture-options` alone is written only next to a key that is the
        // daemon's: without one, the background is none of its business.
        let picture_options = if writes_a_uri || self.model.holds_a_key() {
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
        let written: Vec<Style> = Style::ALL
            .into_iter()
            .filter(|&style| uris[style].is_some())
            .collect();
        self.effects.push(Effect::SetKeys {
            uris,
            picture_options,
        });
        self.announce_key_writes(&written);
    }

    /// Uri mode: an image is on screen once its key names it, so the key
    /// write is what `Changed` announces. In slots mode the publish was.
    fn announce_key_writes(&mut self, written: &[Style]) {
        if self.model.config().general.apply_mode != ApplyMode::Uri {
            return;
        }
        for &style in written {
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
