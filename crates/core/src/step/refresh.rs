//! The refresh of a slot that was written while its style was hidden. The
//! shell watches only the slot it shows, so of any other it may keep a
//! texture from before the write.

use std::mem;

use super::{HIDDEN_REFRESH_DELAY, Turn};
use crate::event::{Effect, SlotWrite};
use crate::style::Style;

impl Turn<'_> {
    /// Plans the refresh of the slot on screen when it is dirty, for
    /// [`HIDDEN_REFRESH_DELAY`] from now. A hidden slot's refresh is put off
    /// until its style is shown.
    pub(super) fn plan_refresh(&mut self) {
        let due = self.now.checked_add(HIDDEN_REFRESH_DELAY);
        for style in Style::ALL {
            let state = &mut self.model.styles[style];
            let is_stale_on_screen = style == self.model.visible && state.dirty_while_hidden;
            state.refresh_due = due.filter(|_| is_stale_on_screen);
        }
    }

    /// Publishes every dirty slot again whose refresh is due: the same
    /// content on a new inode, which makes the shell drop its stale texture.
    /// The mark stays until that inode is stored, or the refresh fails.
    pub(super) fn refresh_stale_slots(&mut self) {
        for style in Style::ALL {
            let deadline = self.model.refresh_deadline(style);
            if deadline.is_some_and(|due| due <= self.now) {
                let state = &mut self.model.styles[style];
                state.refresh_due = None;
                state.is_refreshing = true;
                self.effects.push(Effect::Republish(style));
            }
        }
    }

    pub(super) fn on_republished(&mut self, style: Style, result: Result<SlotWrite, String>) {
        let state = &mut self.model.styles[style];
        let was_awaited = mem::take(&mut state.is_refreshing);
        if !was_awaited {
            return;
        }
        // After a failure the slot is as it was. The interpreter logged the
        // error text, and the mark is spent: the refresh is not tried again.
        // A success while the style is hidden leaves the shell's texture
        // stale, so the mark stays for the next time the style is shown.
        if let Ok(write) = result {
            state.slot = Some(write.slot);
            state.dirty_while_hidden = style != self.model.visible;
            self.persist();
        } else {
            state.dirty_while_hidden = false;
        }
        // A slot that was lost meanwhile had to wait for this answer.
        self.fill_empty_slots();
    }
}
