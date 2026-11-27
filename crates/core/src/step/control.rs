//! Control commands from `gpaperctl`, through the daemon's D-Bus server.
//! Every command gets exactly one `Effect::Reply` with its id.

use std::mem;

use super::{Turn, keys};
use crate::catalog::{CatalogDelta, ItemId};
use crate::config::{ApplyMode, Config, ConfigError};
use crate::control::{
    Command, Direction, PartialMove, Reply, ReplyId, Request, RequestStage, StyleTarget,
};
use crate::event::{Effect, Event, Favorite, Pause};
use crate::model::Unshowable;
use crate::style::Style;

const STARTING: &str = "gpaperd is still starting";
const STOPPING: &str = "gpaperd is stopping";
const SUPERSEDED: &str = "a newer reload replaced this one";
const CACHE_NOT_LOADED: &str = "the remote cache is not loaded yet";

impl Turn<'_> {
    /// [`Event::Control`] before `Started`: nothing runs yet, but the caller
    /// still gets its reply.
    pub(super) fn control_before_start(&mut self, id: ReplyId) {
        self.reply(id, Reply::Failed(STARTING.to_owned()));
    }

    /// Any event after `Shutdown`. Only a control request is answered.
    pub(super) fn after_stop(&mut self, ev: Event) {
        if let Event::Control { id, .. } = ev {
            self.reply(id, Reply::Failed(STOPPING.to_owned()));
        }
    }

    /// [`Event::Control`] while running.
    pub(super) fn on_control(&mut self, id: ReplyId, command: Command) {
        match command {
            Command::Next(target) => self.queue_request(id, Direction::Forward, target),
            Command::Previous(target) => self.queue_request(id, Direction::Back, target),
            Command::Set(target, item) => self.queue_request(id, Direction::To(item), target),
            Command::Ban(target) => self.ban(id, target),
            Command::Unban(item) => self.unban(id, item),
            Command::Favorite(target) => self.save_favorites(id, target),
            Command::Pause(target) => self.pause_by_user(id, target),
            Command::Resume(target) => self.resume_by_user(id, target),
            Command::Reload => {
                self.model.reloads.push(id);
                self.effects.push(Effect::ReadConfig(id));
            }
            Command::RefreshRemote => {
                if self.model.remote.is_loaded() {
                    self.model.remote.refresh_all();
                    self.reply(id, Reply::Done);
                } else {
                    self.reply(id, Reply::Failed(CACHE_NOT_LOADED.to_owned()));
                }
            }
            Command::Restore => self.restore(id),
            Command::Status => {
                let report = self.model.status_report();
                self.reply(id, Reply::Status(report));
            }
        }
    }

    /// [`Event::ConfigRead`]: applied as
    /// `ConfigChanged` is, then answered with `Reloaded`. A read that no
    /// `Reload` waits for is ignored.
    pub(super) fn on_config_read(&mut self, id: ReplyId, result: Result<Config, ConfigError>) {
        let Some(index) = self.model.reloads.iter().position(|&asked| asked == id) else {
            return;
        };
        let is_newest = index + 1 == self.model.reloads.len();
        self.model.reloads.remove(index);
        if !is_newest {
            self.reply(id, Reply::Reloaded(Err(SUPERSEDED.to_owned())));
            return;
        }
        match result {
            Ok(config) => {
                self.reload(config);
                self.reply(id, Reply::Reloaded(Ok(())));
            }
            Err(error) => {
                // A read still listed is older than this one.
                self.fail_listed_reloads();
                self.reply(id, Reply::Reloaded(Err(error.to_string())));
            }
        }
    }

    /// A newer config, from the watcher or from the newest read, replaces
    /// every `Reload` that is still waiting on its file.
    pub(super) fn fail_listed_reloads(&mut self) {
        let listed = mem::take(&mut self.model.reloads);
        for id in listed {
            self.reply(id, Reply::Reloaded(Err(SUPERSEDED.to_owned())));
        }
    }

    /// [`Event::KeysRestored`]: answers the `Restore` that waits for it. An
    /// answer that no `Restore` waits for is ignored.
    pub(super) fn on_keys_restored(&mut self, id: ReplyId, result: Result<(), String>) {
        let Some(index) = self.model.restores.iter().position(|&asked| asked == id) else {
            return;
        };
        self.model.restores.remove(index);
        let reply = match result {
            Ok(()) => Reply::Done,
            Err(error) => Reply::Failed(format!("not every original key was restored: {error}")),
        };
        self.reply(id, reply);
    }

    /// [`Event::FavoritesSaved`]: answers the `Favorite` that waits for it.
    /// An answer that no `Favorite` waits for is ignored.
    pub(super) fn on_favorites_saved(&mut self, id: ReplyId, result: Result<(), String>) {
        let Some(index) = self.model.favorites.iter().position(|&asked| asked == id) else {
            return;
        };
        self.model.favorites.remove(index);
        let reply = match result {
            Ok(()) => Reply::Done,
            Err(error) => Reply::Failed(format!("the favorite was not saved: {error}")),
        };
        self.reply(id, reply);
    }

    /// `Shutdown`: every request that still waits fails.
    pub(super) fn refuse_waiting_requests(&mut self) {
        let waiting = mem::take(&mut self.model.requests);
        let reloads = mem::take(&mut self.model.reloads);
        let restores = mem::take(&mut self.model.restores);
        let favorites = mem::take(&mut self.model.favorites);
        let ids = waiting.iter().map(|request| request.id);
        for id in ids.chain(reloads).chain(restores).chain(favorites) {
            self.reply(id, Reply::Failed(STOPPING.to_owned()));
        }
    }

    /// Ends the requests whose rotations are over, starts those whose style
    /// is free, and answers every request that is finished.
    pub(super) fn serve_requests(&mut self) {
        let mut requests = mem::take(&mut self.model.requests);
        for request in &mut requests {
            for part in &mut request.styles {
                if let RequestStage::Running { before } = &part.stage {
                    part.stage =
                        self.follow_rotation(part.style, before.clone(), &request.direction);
                }
            }
        }
        for index in 0..requests.len() {
            self.start_free_styles(&mut requests, index);
        }
        for request in requests {
            match request.reply() {
                Some(reply) => {
                    self.note_partial_move(&request);
                    self.reply(request.id, reply);
                }
                None => self.model.requests.push(request),
            }
        }
    }

    /// Starts the queued styles of `requests[index]` that nothing is in
    /// flight for, and that no earlier request still runs for.
    fn start_free_styles(&mut self, requests: &mut [Request], index: usize) {
        let (earlier, rest) = requests.split_at_mut(index);
        let request = &mut rest[0];
        for part in &mut request.styles {
            let is_taken = earlier.iter().any(|other| other.runs_for(part.style));
            let is_free = !is_taken && !self.model.styles[part.style].is_busy();
            if part.stage == RequestStage::Queued && is_free {
                part.stage = self.start_request(part.style, &request.direction);
            }
        }
    }

    /// Where a request's rotation of `style` stands. `before` is what the
    /// style showed when it started. A `Set` is over once its image is on
    /// screen; a `Next` or `Previous` once any other image is.
    fn follow_rotation(
        &self,
        style: Style,
        before: Option<ItemId>,
        direction: &Direction,
    ) -> RequestStage {
        let state = &self.model.styles[style];
        let is_uri_mode = self.model.config().general.apply_mode == ApplyMode::Uri;
        let owes_uri = is_uri_mode && self.model.owed_uri(style).is_some();
        // A write held for a later instant or tick may come long after the
        // caller stopped waiting.
        let writes_later = is_uri_mode && state.key.reassert.is_some();
        let has_arrived = match direction {
            Direction::To(item) => state.current.as_ref() == Some(item),
            Direction::Forward | Direction::Back => state.current != before,
        };
        if state.is_busy() || owes_uri {
            RequestStage::Running { before }
        } else if writes_later && has_arrived {
            RequestStage::Finished(Err(format!("the {style} background key is written later")))
        } else if has_arrived {
            RequestStage::Finished(Ok(()))
        } else if let Direction::To(_) = direction {
            RequestStage::Finished(Err(format!("the {style} image could not be shown")))
        } else {
            RequestStage::Finished(Err(format!("no other {style} image could be shown")))
        }
    }

    /// Starts a requested rotation of `style`: with the next image of its
    /// order, the newest one of its history that can still be shown, or the
    /// image a `Set` names. A key that waits for the next tick is written
    /// back with it: the clock that would bring that tick may be stopped.
    fn start_request(&mut self, style: Style, direction: &Direction) -> RequestStage {
        if let Some(why) = self.why_style_cannot_change(style) {
            return RequestStage::Finished(Err(why));
        }
        let before = self.model.styles[style].current.clone();
        let candidate = match direction {
            // A pick from a round with nothing else to show would start a
            // new round, and forget what this one showed.
            Direction::Forward if !self.model.has_other_candidate(style) => {
                Err(format!("no other {style} image to show"))
            }
            Direction::Forward => self
                .model
                .pick(style, &[])
                .ok_or_else(|| format!("no other {style} image to show")),
            Direction::Back => self
                .model
                .step_back(style)
                .ok_or_else(|| format!("no earlier {style} image to show")),
            // A banned image on screen stays only until the next change.
            Direction::To(item) if self.model.is_banned(item) => {
                Err(unshowable_text(style, &Unshowable::Banned))
            }
            Direction::To(item) if self.model.shows(style, item) => {
                return self.keep_set_image(style);
            }
            Direction::To(item) => self
                .model
                .chosen_candidate(style, item)
                .map_err(|why| unshowable_text(style, &why)),
        };
        match candidate {
            Ok(candidate) => {
                self.model.styles[style].key.end_tick_wait();
                self.probe(style, candidate, Vec::new());
                RequestStage::Running { before }
            }
            Err(why) => RequestStage::Finished(Err(why)),
        }
    }

    /// A `Set` of the image on screen: nothing to publish, but the image
    /// gets a whole interval from now, as a shown one does.
    fn keep_set_image(&mut self, style: Style) -> RequestStage {
        self.model.restart_clock(style, self.now);
        self.persist();
        RequestStage::Finished(Ok(()))
    }

    /// A `Set` candidate failed. No other image is tried: the request ends
    /// once nothing is in flight for the style. As when a rotation gives up,
    /// a fresh interval starts, so that a tick that fell due meanwhile does
    /// not show another image in its place. A slot that was lost meanwhile
    /// is filled again.
    pub(super) fn end_set(&mut self, style: Style) {
        self.model.restart_clock(style, self.now);
        self.persist();
        if !self.model.has_wallpaper(style) {
            self.refill(style);
        }
    }

    fn why_style_cannot_change(&self, style: Style) -> Option<String> {
        if self.model.awaits_config() {
            return Some("no config file is loaded".to_owned());
        }
        match self.model.styles[style].key.pause {
            Some(Pause::NoKey) => Some(format!("GNOME has no {style} background key")),
            Some(Pause::LockedKey) => Some(format!("the {style} background key is locked")),
            _ => self.why_key_shows_other_wallpaper(style),
        }
    }

    /// Why the key of `style` does not name the image the rotation last
    /// showed: an outside change, or a key handed back to the user.
    fn why_key_shows_other_wallpaper(&self, style: Style) -> Option<String> {
        let state = &self.model.styles[style];
        if state.key.pause == Some(Pause::ExternalChange) {
            return Some(format!(
                "the {style} background key was changed outside gpaperd; run gpaperctl resume"
            ));
        }
        state.is_handed_back().then(|| {
            format!("the {style} background key holds your own wallpaper; run gpaperctl resume")
        })
    }

    /// A `Previous` candidate failed: the next older image of the history is
    /// tried. With none left the request fails and the slot stays, unless it
    /// was lost meanwhile.
    pub(super) fn step_back_further(&mut self, style: Style) {
        if let Some(candidate) = self.model.step_back(style) {
            return self.probe(style, candidate, Vec::new());
        }
        let why = format!("no earlier {style} image could be shown");
        for request in &mut self.model.requests {
            for part in &mut request.styles {
                if part.style == style && matches!(part.stage, RequestStage::Running { .. }) {
                    part.stage = RequestStage::Finished(Err(why.clone()));
                }
            }
        }
        if !self.model.has_wallpaper(style) {
            self.refill(style);
        }
    }

    fn queue_request(&mut self, id: ReplyId, direction: Direction, target: StyleTarget) {
        let styles = target.styles(self.model.visible);
        if let Direction::To(item) = &direction
            && let Some(unlisted) = self.style_without(&styles, item)
        {
            return self.reply(id, Reply::Rejected(not_in_sources(unlisted)));
        }
        let mut request = Request::new(id, direction, styles);
        if let Some(partial) = self.model.partial_move.take() {
            self.carry_partial_move(&mut request, &partial);
        }
        self.model.requests.push(request);
    }

    /// The first of `styles` whose sources do not hold `item`, as far as a
    /// scan tells. A `Set` naming such an item changes nothing in any style.
    /// Without a config every style lacks it: the request then fails as a
    /// `Next` would.
    fn style_without(&self, styles: &[Style], item: &ItemId) -> Option<Style> {
        if self.model.awaits_config() {
            return None;
        }
        let lacks = |style: &&Style| {
            self.model.chosen_candidate(**style, item).err() == Some(Unshowable::NotListed)
        };
        styles.iter().find(lacks).copied()
    }

    /// Bans what each meant style shows, then rotates them as `Next` does.
    /// Nothing is banned when a style shows nothing, or when `Next` would
    /// fail at once for a reason other than a lack of images: no config, or
    /// a key that is missing, locked or not the daemon's.
    fn ban(&mut self, id: ReplyId, target: StyleTarget) {
        let styles = target.styles(self.model.visible);
        let mut shown = Vec::new();
        for &style in &styles {
            if let Some(why) = self.why_style_cannot_change(style) {
                return self.reply(id, Reply::Failed(why));
            }
            match &self.model.styles[style].current {
                Some(item) => shown.push(item.clone()),
                None => return self.reply(id, Reply::Failed(nothing_shown(style))),
            }
        }
        for item in shown {
            self.model.ban(item);
        }
        self.persist();
        self.queue_request(id, Direction::Forward, target);
    }

    /// Lifts the ban of `item`, which then counts as a new file in every
    /// catalog that holds it.
    fn unban(&mut self, id: ReplyId, item: ItemId) {
        if !self.model.unban(&item) {
            return self.reply(id, Reply::Rejected("the image is not banned".to_owned()));
        }
        self.persist();
        for style in Style::ALL {
            if self.model.holds(style, &item) {
                let delta = CatalogDelta {
                    added: vec![item.clone()],
                    modified: Vec::new(),
                };
                self.follow_catalog(style, &delta);
            }
        }
        self.reply(id, Reply::Done);
    }

    /// Asks for a copy of what each meant style shows, once per file, and
    /// replies once [`Event::FavoritesSaved`] says how it went.
    fn save_favorites(&mut self, id: ReplyId, target: StyleTarget) {
        let Some(dir) = self.model.config().general.favorites_dir.clone() else {
            let why = "favorites_dir is not set in the config".to_owned();
            return self.reply(id, Reply::Failed(why));
        };
        let mut images: Vec<Favorite> = Vec::new();
        for style in target.styles(self.model.visible) {
            if self.model.styles[style].current.is_none() {
                return self.reply(id, Reply::Failed(nothing_shown(style)));
            }
            if let Some(why) = self.why_key_shows_other_wallpaper(style) {
                return self.reply(id, Reply::Failed(why));
            }
            let Some(image) = self.model.favorite(style) else {
                let why = format!("the {style} image is no longer available");
                return self.reply(id, Reply::Failed(why));
            };
            if !images.iter().any(|saved| saved.source == image.source) {
                images.push(image);
            }
        }
        self.model.favorites.push(id);
        self.effects.push(Effect::SaveFavorites { id, dir, images });
    }

    /// A both-style retry of the same direction does not pick again for a
    /// style that the last one moved, while that style still shows what it
    /// moved to. A banned image is never kept that way.
    fn carry_partial_move(&self, request: &mut Request, partial: &PartialMove) {
        if request.styles.len() < 2 || partial.direction != request.direction {
            return;
        }
        for part in &mut request.styles {
            let current = &self.model.styles[part.style].current;
            let still_shown = partial
                .moved
                .iter()
                .any(|(style, shown)| *style == part.style && shown == current);
            let is_banned = current
                .as_ref()
                .is_some_and(|item| self.model.is_banned(item));
            if still_shown && !is_banned {
                part.stage = RequestStage::Carried;
            }
        }
    }

    /// Remembers what a mixed both-style request rotated itself, so that
    /// one retry of the same direction does not move it again.
    fn note_partial_move(&mut self, request: &Request) {
        if request.styles.len() < 2 {
            return;
        }
        let failed = request
            .styles
            .iter()
            .any(|part| matches!(part.stage, RequestStage::Finished(Err(_))));
        let moved: Vec<(Style, Option<ItemId>)> = request
            .styles
            .iter()
            .filter(|part| part.stage == RequestStage::Finished(Ok(())))
            .map(|part| (part.style, self.model.styles[part.style].current.clone()))
            .collect();
        self.model.partial_move = (failed && !moved.is_empty()).then(|| PartialMove {
            direction: request.direction.clone(),
            moved,
        });
    }

    /// The user pause holds until `Resume`. The style's clock runs on.
    fn pause_by_user(&mut self, id: ReplyId, target: StyleTarget) {
        for style in target.styles(self.model.visible) {
            self.model.styles[style].user_paused = true;
        }
        self.persist();
        self.reply(id, Reply::Done);
    }

    /// Ends the user pause, and an external-change pause with it: that key
    /// is written back when the step settles. An overdue style rotates then,
    /// any other keeps its deadline.
    fn resume_by_user(&mut self, id: ReplyId, target: StyleTarget) {
        for style in target.styles(self.model.visible) {
            let state = &mut self.model.styles[style];
            state.user_paused = false;
            state.key.resume();
        }
        self.persist();
        self.reply(id, Reply::Done);
    }

    /// Writes the user's original keys back and pauses both styles, and
    /// replies once the write is over. The keys are the user's until
    /// `Resume`: no value of theirs is an external change, and none of them
    /// is written.
    fn restore(&mut self, id: ReplyId) {
        let Some(original) = self.model.original_keys.clone() else {
            return self.reply(id, Reply::Failed("no original keys are saved".to_owned()));
        };
        for style in Style::ALL {
            let state = &mut self.model.styles[style];
            state.user_paused = true;
            state.key.hand_back();
        }
        self.model.picture_options_owed =
            keys::picture_options_to_write(self.model.config(), &original);
        self.persist();
        self.model.restores.push(id);
        self.effects
            .push(Effect::RestoreKeys { id, keys: original });
    }

    fn reply(&mut self, id: ReplyId, reply: Reply) {
        self.effects.push(Effect::Reply { id, reply });
    }
}

fn nothing_shown(style: Style) -> String {
    format!("no {style} image is shown")
}

fn not_in_sources(style: Style) -> String {
    format!("the image is not in the sources of {style}")
}

/// Why `style` cannot show the image a `Set` names. The text leaves the
/// image out: a URL may hold credentials.
fn unshowable_text(style: Style, why: &Unshowable) -> String {
    match why {
        Unshowable::Unscanned => format!("the {style} sources are still being scanned"),
        Unshowable::NotListed => not_in_sources(style),
        Unshowable::NotDownloaded => format!("the {style} image is not downloaded yet"),
        Unshowable::Bad(reason) => format!("the {style} image cannot be shown: {reason}"),
        Unshowable::Banned => format!("the {style} image is banned"),
    }
}
