//! Config reloads: the new config replaces the running one, and the reducer
//! applies what differs.

use super::Turn;
use crate::config::{Config, General, StyleConfig};
use crate::event::Effect;
use crate::style::Style;

impl Turn<'_> {
    /// Makes `new` the running config and applies what it changes. The
    /// rotations that an interval makes due start when the step settles.
    pub(super) fn reload(&mut self, new: Config) {
        self.model.partial_move = None;
        self.fail_listed_reloads();
        let old = self.model.replace_config(new);
        // Before `apply_mode` clears the wallpapers and refills: an empty
        // style must not borrow the other catalog for sources still to scan.
        for style in Style::ALL {
            if self.model.config().styles[style].sources != old.styles[style].sources {
                self.model.await_rescan(style);
            }
        }
        self.reload_general(&old.general);
        for style in Style::ALL {
            self.reload_style(style, &old.styles[style]);
            self.resume(style);
            // `rotate_hidden` decides which clocks run.
            self.model.follow_visibility(style, self.now);
        }
        self.reload_remotes();
        // A new `empty_style` or `apply_mode` leaves a style to be filled.
        self.fill_empty_slots();
    }

    pub(super) fn reload_general(&mut self, old: &General) {
        let new = self.model.config().general.clone();
        // A value that stays leaves owed alone: that may be the `zoom` that
        // replaces `none`.
        if new.picture_options != old.picture_options {
            self.model.picture_options_owed = new.picture_options;
        }
        if new.apply_mode != old.apply_mode {
            // Each mode shows its own thing: nothing is there yet for the
            // keys to name.
            for style in Style::ALL {
                self.model.clear_wallpaper(style);
            }
            // A key locked on its slot URI is fine in slots mode only.
            self.follow_locks();
        }
        if new.max_pixels > old.max_pixels {
            let fit_now = self.model.forget_size_verdicts();
            self.follow_dropped_checks(fit_now);
        } else if new.max_pixels < old.max_pixels {
            for style in Style::ALL {
                self.model.styles[style].reprobe_owed = true;
            }
        }
    }

    pub(super) fn reload_style(&mut self, style: Style, old: &StyleConfig) {
        let new = &self.model.config().styles[style];
        let has_new_order = new.order != old.order;
        if new.sources != old.sources {
            let sources = new.sources.clone();
            self.effects.push(Effect::Watch { style, sources });
        }
        if has_new_order {
            self.model.reset_round(style);
        }
    }

    /// A reload ends what an external change began: `style` rotates again
    /// and its key is written back.
    pub(super) fn resume(&mut self, style: Style) {
        let key = &mut self.model.styles[style].key;
        let was_paused = key.is_paused_by_change();
        key.resume();
        if was_paused {
            self.persist();
        }
    }
}
