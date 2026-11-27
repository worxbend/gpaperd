//! Control commands from `gpaperctl`, through the daemon's D-Bus server.
//! Every command gets exactly one `Effect::Reply` with its id.

use std::mem;

use super::{Turn, keys};
use crate::catalog::ItemId;
use crate::config::{ApplyMode, Config, ConfigError};
use crate::control::{
    Command, Direction, PartialMove, Reply, ReplyId, Request, RequestStage, StyleTarget,
};
use crate::event::{Effect, Event, Pause};
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

    /// `Shutdown`: every request that still waits fails.
    pub(super) fn refuse_waiting_requests(&mut self) {
        let waiting = mem::take(&mut self.model.requests);
        let reloads = mem::take(&mut self.model.reloads);
        let restores = mem::take(&mut self.model.restores);
        let ids = waiting.iter().map(|request| request.id);
        for id in ids.chain(reloads).chain(restores) {
            self.reply(id, Reply::Failed(STOPPING.to_owned()));
        }
    }

    /// Ends the requests whose rotations are over, starts those whose style
    /// is free, and answers every request that is finished.
    pub(super) fn serve_requests(&mut self) {
        let mut requests = mem::take(&mut self.model.requests);
        for part in requests.iter_mut().flat_map(|request| &mut request.styles) {
            if let RequestStage::Running { before } = &part.stage {
                part.stage = self.follow_rotation(part.style, before.clone());
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
                part.stage = self.start_request(part.style, request.direction);
            }
        }
    }

    /// Where a request's rotation of `style` stands. `before` is what the
    /// style showed when it started.
    fn follow_rotation(&self, style: Style, before: Option<ItemId>) -> RequestStage {
        let state = &self.model.styles[style];
        let is_uri_mode = self.model.config().general.apply_mode == ApplyMode::Uri;
        let owes_uri = is_uri_mode && self.model.owed_uri(style).is_some();
        // A write held for a later instant or tick may come long after the
        // caller stopped waiting.
        let writes_later = is_uri_mode && state.key.reassert.is_some();
        if state.is_busy() || owes_uri {
            RequestStage::Running { before }
        } else if writes_later && state.current != before {
            RequestStage::Finished(Err(format!("the {style} background key is written later")))
        } else if state.current != before {
            RequestStage::Finished(Ok(()))
        } else {
            RequestStage::Finished(Err(format!("no other {style} image could be shown")))
        }
    }

    /// Starts a requested rotation of `style`: with the next image of its
    /// order, or the newest one of its history that can still be shown. A
    /// key that waits for the next tick is written back with it: the clock
    /// that would bring that tick may be stopped.
    fn start_request(&mut self, style: Style, direction: Direction) -> RequestStage {
        if let Some(why) = self.why_style_cannot_change(style) {
            return RequestStage::Finished(Err(why));
        }
        let before = self.model.styles[style].current.clone();
        let candidate = match direction {
            // A pick from a round with nothing else to show would start a
            // new round, and forget what this one showed.
            Direction::Forward if !self.model.has_other_candidate(style) => None,
            Direction::Forward => self.model.pick(style, &[]),
            Direction::Back => self.model.step_back(style),
        };
        match (candidate, direction) {
            (Some(candidate), _) => {
                self.model.styles[style].key.end_tick_wait();
                self.probe(style, candidate, Vec::new());
                RequestStage::Running { before }
            }
            (None, Direction::Forward) => {
                RequestStage::Finished(Err(format!("no other {style} image to show")))
            }
            (None, Direction::Back) => {
                RequestStage::Finished(Err(format!("no earlier {style} image to show")))
            }
        }
    }

    fn why_style_cannot_change(&self, style: Style) -> Option<String> {
        if self.model.awaits_config() {
            return Some("no config file is loaded".to_owned());
        }
        let state = &self.model.styles[style];
        match state.key.pause {
            Some(Pause::NoKey) => Some(format!("GNOME has no {style} background key")),
            Some(Pause::LockedKey) => Some(format!("the {style} background key is locked")),
            // The key would not name what the rotation shows.
            Some(Pause::ExternalChange) => Some(format!(
                "the {style} background key was changed outside gpaperd; run gpaperctl resume"
            )),
            Some(Pause::User | Pause::ScreenLocked) | None if state.is_handed_back() => {
                Some(format!(
                    "the {style} background key holds your own wallpaper; run gpaperctl resume"
                ))
            }
            // A key pause is never one of these two; a request goes on under
            // either.
            Some(Pause::User | Pause::ScreenLocked) | None => None,
        }
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
        let mut request = Request::new(id, direction, target.styles(self.model.visible));
        if let Some(partial) = self.model.partial_move.take() {
            self.carry_partial_move(&mut request, &partial);
        }
        self.model.requests.push(request);
    }

    /// A both-style retry of the same direction does not pick again for a
    /// style that the last one moved, while that style still shows what it
    /// moved to.
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
            if still_shown {
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
        self.model.partial_move = (failed && !moved.is_empty()).then_some(PartialMove {
            direction: request.direction,
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
