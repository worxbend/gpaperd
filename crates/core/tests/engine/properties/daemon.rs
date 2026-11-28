//! The interpreter and the world around it, with the invariants that every
//! effect must keep.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::{BadReason, FileId, FsChange, ItemId, LocalFile};
use gpaperd_core::config::{
    Config, ConfigError, EmptyStylePolicy, ExternalChangePolicy, Order, Source,
};
use gpaperd_core::control::{Command, HISTORY_LEN, Reply, ReplyId};
use gpaperd_core::event::{
    BackgroundKeys, Effect, Event, Fired, GnomeEvent, HookEvent, HookRun, Method, Pause,
    ScreenLock, SlotWrite,
};
use gpaperd_core::model::{Model, ModelInit, Persisted, STATE_VERSION, StylePersisted};
use gpaperd_core::step::{
    HIDDEN_REFRESH_DELAY, MAX_PROBE_RETRIES, MAX_PUBLISH_ATTEMPTS, PROBE_RETRY_DELAY,
    REASSERT_DELAY, step,
};
use gpaperd_core::style::{PerStyle, Style};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::scenario::{Boot, Folders, Op, Reload, Scenario};
use crate::support::{
    REMOTE_CACHE_DIR, at, id, is_set_keys, keys_changed, slot_uris, started, started_with_locks,
    user_keys, version,
};

/// Answers the daemon may still owe after the last generated op. Each style
/// can have three rotations in a row: the fill of its slot, a rotation that
/// is overdue, and one that publishes a changed image again. Each tries up
/// to [`MAX_PUBLISH_ATTEMPTS`] candidates, with a probe and a publish each.
/// The refresh of a dirty slot is one more answer.
const MAX_ANSWERS_OWED: u32 = 2 * (3 * 2 * MAX_PUBLISH_ATTEMPTS + 1);

/// How the daemon knows of an image that it may show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sighting {
    /// The state file names it as the image in the slot, or in the style's
    /// history, and no scan of the style came back yet.
    SavedState,
    /// A scan or the watcher reported the file, saved this many times.
    File(u64),
}

/// One version of an image: its id and how often its file was saved.
type Version = (ItemId, u64);

/// A probed image, and how the daemon knew of it when the probe started.
type Probed = (ItemId, Vec<Sighting>);

/// An image that was truncated when it was last probed.
#[derive(Debug, Clone, Copy)]
struct StillCopied {
    /// Truncated verdicts in a row.
    verdicts: u32,
    /// The earliest time of its next probe. `None` once the daemon saw that
    /// time pass: a clock that goes back later does not bring the wait back.
    waits_until: Option<SystemTime>,
}

/// The interpreter and the world around it, as far as the reducer can tell.
pub struct Daemon<'a> {
    scenario: &'a Scenario,
    /// The config file as it was last loaded: the running config.
    pub config: Config,
    pub model: Model,
    folders: Folders,
    /// What the sources of a reload cover, until its `Watch` effects are in.
    reloaded_folders: Option<Folders>,
    /// The images on disk.
    on_disk: BTreeSet<&'static str>,
    /// How often each image was saved. A deleted image keeps its count, so
    /// that it comes back as it left.
    saves: BTreeMap<&'static str, u64>,
    /// What the daemon was told of each style's files: every image and the
    /// version of it. `None` before the style's first scan result.
    told: PerStyle<Option<BTreeMap<ItemId, u64>>>,
    /// The styles whose scan is under way. Their files are not watched yet.
    scanning: PerStyle<bool>,
    /// The image the state file names for each slot.
    saved_current: PerStyle<Option<ItemId>>,
    /// The images of each style's history in the state file: what a
    /// `Previous` before the first scan steps back to.
    saved_history: PerStyle<Vec<ItemId>>,
    /// Seconds into the test timeline.
    now_secs: u64,
    visible: Style,
    /// Inode of each slot file on disk.
    slots: PerStyle<Option<FileId>>,
    last_slot_inode: u64,
    armed: Option<SystemTime>,
    /// GNOME's background keys.
    keys: BackgroundKeys,
    /// Whether this run's GNOME has a dark key.
    has_dark_key: bool,
    /// The keys that an administrator locked for this run.
    locked: PerStyle<bool>,
    /// The styles that this run must leave alone: their key is missing, or
    /// locked on something else than the slot URI.
    disabled: PerStyle<bool>,
    /// The styles whose key the daemon knows to hold the slot URI through a
    /// write of its own, or took over as such when the run started.
    holds_key: PerStyle<bool>,
    /// The styles that the `pause` policy stopped, because something else
    /// took their key. Saved state: a restart does not end it.
    taken: PerStyle<bool>,
    /// When `reassert-now` may write a taken key back, at the earliest.
    /// `None` once the daemon saw that time pass: a clock that goes back
    /// later does not bring the wait back.
    reassert_after: PerStyle<Option<SystemTime>>,
    /// The taken keys that `reassert-now` must write back as soon as it can.
    write_back_due: PerStyle<bool>,
    /// When the shell has had its time with the style on screen: from then
    /// on its slot may be republished.
    refresh_from: SystemTime,
    /// The style left the screen while its republish was unanswered: the
    /// write may have landed unwatched.
    hidden_while_refreshing: PerStyle<bool>,
    /// Foreign wallpapers written to the keys so far.
    foreign_wallpapers: u64,
    /// The saved state from before the step in progress.
    before: Persisted,
    /// `Probe`, `Publish` and `Republish` effects that have no answer yet,
    /// oldest first.
    unanswered: VecDeque<Effect>,
    /// The candidates each style's latest rotation probed, oldest first,
    /// and how the daemon knew of each one then. A candidate leaves the list
    /// when its file changes.
    tried: PerStyle<Vec<Probed>>,
    /// What each style probed last.
    probing: PerStyle<Option<Probed>>,
    /// Versions whose probe failed for good in this run of the daemon, per
    /// catalog. A catalog that learns the file later knows no verdict.
    known_bad: PerStyle<BTreeSet<Version>>,
    /// Versions each catalog holds back while their truncated wait runs.
    held_back: PerStyle<BTreeSet<Version>>,
    /// Versions that wait for another probe.
    still_copied: BTreeMap<Version, StillCopied>,
    pub is_ready: bool,
    /// `Shutdown` was sent: the daemon waits for nothing any more.
    is_stopped: bool,
    /// The control requests without a reply yet, by id, with the styles
    /// that each `Next` or `Previous` rotates.
    waiting: BTreeMap<u64, Vec<Style>>,
    /// The ids among `waiting` of the `Previous` requests.
    stepping_back: BTreeSet<u64>,
    /// The ids among `waiting` of the `Set` requests.
    setting: BTreeSet<u64>,
    /// The requests that the step in progress replies to.
    replied_in_step: BTreeSet<u64>,
    /// Whether the step in progress saves the state.
    saved_in_step: bool,
    /// What the last [`Effect::Persist`] wrote. A restart loads this, not the
    /// live model: a step that never saved is gone with the process. `None`
    /// until the first save.
    state_file: Option<Persisted>,
    /// The styles whose `Previous` the step in progress replies to: the walk
    /// is over, so any probe of the step starts a rotation of its own. The
    /// refill of a lost slot comes before the reply.
    walks_ended: PerStyle<bool>,
    /// The id of the next control request.
    next_request: u64,
    /// Whether the screen is locked.
    screen_locked: bool,
    /// Whether this run sent `Started`.
    has_started: bool,
    /// The keys from before the first run: what `Restore` writes back.
    original_keys: Option<BackgroundKeys>,
    /// The `Restore` writes that the step in progress asked for.
    restores_owed: Vec<(ReplyId, BackgroundKeys)>,
    /// The effects of every step, in order.
    pub trace: Vec<Vec<Effect>>,
}

impl<'a> Daemon<'a> {
    pub fn boot(scenario: &'a Scenario) -> Result<Daemon<'a>, TestCaseError> {
        let first = &scenario.first_boot;
        let model = new_model(&scenario.config, PerStyle::default(), first.seed, None);
        let mut daemon = Daemon {
            scenario,
            config: scenario.config.clone(),
            before: model.persisted(),
            model,
            folders: first.folders.clone(),
            reloaded_folders: None,
            on_disk: BTreeSet::new(),
            saves: BTreeMap::new(),
            told: PerStyle::default(),
            scanning: PerStyle::default(),
            saved_current: PerStyle::default(),
            saved_history: PerStyle::default(),
            now_secs: 0,
            visible: first.visible,
            slots: PerStyle::default(),
            last_slot_inode: 0,
            armed: None,
            keys: user_keys(),
            has_dark_key: true,
            locked: PerStyle::default(),
            disabled: PerStyle::default(),
            holds_key: PerStyle::default(),
            taken: PerStyle::default(),
            reassert_after: PerStyle::default(),
            write_back_due: PerStyle::default(),
            refresh_from: at(0),
            hidden_while_refreshing: PerStyle::default(),
            foreign_wallpapers: 0,
            unanswered: VecDeque::new(),
            tried: PerStyle::default(),
            probing: PerStyle::default(),
            known_bad: PerStyle::default(),
            held_back: PerStyle::default(),
            still_copied: BTreeMap::new(),
            is_ready: false,
            is_stopped: false,
            waiting: BTreeMap::new(),
            stepping_back: BTreeSet::new(),
            setting: BTreeSet::new(),
            replied_in_step: BTreeSet::new(),
            saved_in_step: false,
            state_file: None,
            walks_ended: PerStyle::default(),
            next_request: 0,
            screen_locked: false,
            has_started: false,
            original_keys: None,
            restores_owed: Vec::new(),
            trace: Vec::new(),
        };
        daemon.start_run(first, None)?;
        Ok(daemon)
    }

    fn now(&self) -> SystemTime {
        at(self.now_secs)
    }

    /// Starts a run of the daemon process over `boot`'s folders.
    fn start_run(
        &mut self,
        boot: &Boot,
        state_file: Option<Persisted>,
    ) -> Result<(), TestCaseError> {
        self.folders = boot.folders.clone();
        self.on_disk
            .extend(boot.folders.light.iter().chain(&boot.folders.dark));
        self.told = PerStyle::from_fn(|style| boot.scanned_up_front.then(|| self.scan(style)));
        self.scanning = PerStyle::default();
        self.saved_current = PerStyle::from_fn(|style| {
            let saved = state_file.as_ref()?;
            saved.styles[style].current.clone()
        });
        self.saved_history = PerStyle::from_fn(|style| {
            let saved = state_file
                .as_ref()
                .map(|saved| &saved.styles[style].history);
            saved
                .into_iter()
                .flat_map(|history| history.iter().cloned())
                .collect()
        });
        // Original keys that no save wrote are gone with the killed process.
        self.original_keys = state_file
            .as_ref()
            .and_then(|saved| saved.original_keys.clone());
        let files = PerStyle::from_fn(|style| self.told[style].as_ref().map(local_files));
        self.model = new_model(&self.config, files, boot.seed, state_file);
        self.visible = boot.visible;
        self.armed = None;
        self.unanswered.clear();
        self.hidden_while_refreshing = PerStyle::default();
        self.tried = PerStyle::default();
        self.probing = PerStyle::default();
        self.known_bad = PerStyle::default();
        self.held_back = PerStyle::default();
        self.still_copied.clear();
        self.is_ready = false;
        // A request to the killed process gets no reply: its caller times
        // out.
        self.waiting.clear();
        self.stepping_back.clear();
        self.setting.clear();
        self.find_keys(boot);
        self.refresh_from = self.now() + HIDDEN_REFRESH_DELAY;
        let keys = self.gnome_keys();
        // Keys left on the slots are the daemon's, not the user's originals.
        let names_a_slot = Style::ALL
            .into_iter()
            .any(|style| keys.uri(style) == Some(slot_uris()[style].as_str()));
        if !names_a_slot {
            self.original_keys.get_or_insert_with(|| keys.clone());
        }
        // The screensaver's state at start may come before `Started`, when
        // the daemon has nothing to do yet.
        self.has_started = false;
        if self.screen_locked {
            self.send(Event::ScreenLock(ScreenLock::Locked))?;
        }
        self.has_started = true;
        self.send(started_with_locks(
            self.visible,
            keys,
            self.slots,
            self.locked,
        ))
    }

    /// A run starts: what it can do with each key, and what it knows of the
    /// key from the start.
    fn find_keys(&mut self, boot: &Boot) {
        self.has_dark_key = boot.has_dark_key;
        self.locked = boot.locked;
        self.forget_write_backs();
        for style in Style::ALL {
            let names_slot = self.key(style) == Some(slot_uris()[style].as_str());
            let is_locked_elsewhere = self.locked[style] && !names_slot;
            self.disabled[style] = self.key(style).is_none() || is_locked_elsewhere;
            let was_taken = mem::take(&mut self.taken[style]);
            // A missing key ends the pause, and so does a key that names the
            // slot again. A lock elsewhere only holds it: the pause waits
            // under the lock, is saved, and holds again once it is lifted.
            self.taken[style] = was_taken && self.key(style).is_some() && !names_slot;
            // The slot URI counts as the daemon's own write when the key is
            // locked on it, when the last run was paused by an external
            // change, and when the user paused it with the key on the slot.
            let kept_by_user = self.is_user_paused(style) && names_slot;
            self.holds_key[style] =
                !self.disabled[style] && (self.locked[style] || was_taken || kept_by_user);
        }
    }

    /// What `style`'s key holds. `None` when this run's GNOME has no such
    /// key.
    fn key(&self, style: Style) -> Option<&str> {
        match style {
            Style::Light => Some(&self.keys.picture_uri),
            Style::Dark => self
                .keys
                .picture_uri_dark
                .as_deref()
                .filter(|_| self.has_dark_key),
        }
    }

    /// Nothing waits to be written back any more: a new run or a reload
    /// writes the keys anyway.
    fn forget_write_backs(&mut self) {
        self.reassert_after = PerStyle::default();
        self.write_back_due = PerStyle::default();
    }

    /// Stores `uri` in `style`'s key.
    fn set_key(&mut self, style: Style, uri: String) {
        match style {
            Style::Light => self.keys.picture_uri = uri,
            Style::Dark => self.keys.picture_uri_dark = Some(uri),
        }
    }

    /// The keys as this run's GNOME reports them.
    fn gnome_keys(&self) -> BackgroundKeys {
        BackgroundKeys {
            picture_uri_dark: self.key(Style::Dark).map(str::to_owned),
            ..self.keys.clone()
        }
    }

    /// Whether something other than the daemon can write `style`'s key.
    fn is_key_free(&self, style: Style) -> bool {
        self.key(style).is_some() && !self.locked[style]
    }

    fn is_paused(&self, style: Style) -> bool {
        self.model.persisted().styles[style].paused
    }

    fn is_user_paused(&self, style: Style) -> bool {
        self.model.persisted().styles[style].user_paused
    }

    /// Whether a locked screen pauses every style.
    fn is_lock_held(&self) -> bool {
        self.screen_locked && self.config.general.pause_when_locked
    }

    /// Whether the user or a locked screen holds `style`'s ticks back.
    fn is_held(&self, style: Style) -> bool {
        self.is_user_paused(style) || self.is_lock_held()
    }

    /// Whether a `Previous` that waits for its reply steps `style` back to
    /// an image that the round may have shown.
    fn is_stepping_back(&self, style: Style) -> bool {
        self.waits_among(&self.stepping_back, style)
    }

    /// Whether a `Set` that waits for its reply puts the image it names on
    /// `style`, which the round may have shown.
    fn is_setting(&self, style: Style) -> bool {
        self.waits_among(&self.setting, style)
    }

    /// Whether one of the waiting requests `ids` rotates `style`.
    fn waits_among(&self, ids: &BTreeSet<u64>, style: Style) -> bool {
        let mut rotated = self.waiting.iter().filter(|(id, _)| ids.contains(id));
        rotated.any(|(_, styles)| styles.contains(&style))
    }

    /// Whether a `Next`, `Previous` or `Set` that waits for its reply
    /// rotates `style`.
    fn is_requested(&self, style: Style) -> bool {
        self.waiting.values().any(|styles| styles.contains(&style))
    }

    /// What a scan of `style`'s folder finds now.
    pub fn scan(&self, style: Style) -> BTreeMap<ItemId, u64> {
        let covered = self.folders.of(style).iter();
        covered
            .filter(|image| self.on_disk.contains(*image))
            .map(|image| (id(image), self.saves_of(image)))
            .collect()
    }

    fn saves_of(&self, image: &str) -> u64 {
        self.saves.get(image).copied().unwrap_or(0)
    }

    pub fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::Wait { secs } => {
                self.now_secs += secs;
                if self.armed.is_some_and(|deadline| deadline <= self.now()) {
                    self.fire()?;
                }
            }
            Op::FireEarly => self.fire()?,
            Op::ClockForward { secs } => {
                self.now_secs += secs;
                self.send(Event::Fired(Fired::ClockChanged))?;
            }
            Op::ClockBack { secs } => {
                self.now_secs = self.now_secs.saturating_sub(*secs);
                self.shorten_waits();
                self.send(Event::Fired(Fired::ClockChanged))?;
            }
            // Without a dark key GNOME has no style to switch to.
            Op::Switch(style) if self.has_dark_key => {
                if *style != self.visible {
                    self.refresh_from = self.now() + HIDDEN_REFRESH_DELAY;
                    let hidden = self.visible;
                    if self.unanswered.contains(&Effect::Republish(hidden)) {
                        self.hidden_while_refreshing[hidden] = true;
                    }
                }
                self.visible = *style;
                self.send(Event::Gnome(GnomeEvent::StyleChanged(*style)))?;
            }
            Op::Switch(_) => {}
            Op::Answer {
                publish_fails,
                still_copied,
            } => self.answer_oldest(*publish_fails, *still_copied)?,
            Op::ScanDone(style) => self.finish_scan(*style)?,
            Op::Create(image) => {
                if self.on_disk.insert(image) {
                    self.report_file(image)?;
                }
            }
            Op::Delete(image) => {
                if self.on_disk.remove(image) {
                    self.report_file(image)?;
                }
            }
            Op::Edit(image) => {
                if self.on_disk.contains(image) {
                    *self.saves.entry(image).or_insert(0) += 1;
                    self.report_file(image)?;
                }
            }
            Op::LoseSlot(style) => {
                if self.slots[*style].take().is_some() {
                    self.send(Event::SlotLost(*style))?;
                }
            }
            Op::TakeKey(style) => self.take_key(*style)?,
            Op::ReturnKey(style) => self.return_key(*style)?,
            Op::Reload(reload) => self.reload(reload)?,
            Op::ReloadFails => self.fail_reload()?,
            Op::Restart { boot, lost_slots } => self.restart(boot, *lost_slots)?,
            Op::Ask(command) => self.ask(command.clone())?,
            Op::Lock(lock) => {
                self.screen_locked = *lock == ScreenLock::Locked;
                self.send(Event::ScreenLock(*lock))?;
            }
        }
        Ok(())
    }

    /// `gpaperctl` sends `command`. A pause, a resume and a status request
    /// are answered at once; the others when their work is over.
    fn ask(&mut self, command: Command) -> Result<(), TestCaseError> {
        let id = self.next_request;
        self.next_request += 1;
        let rotated = match &command {
            Command::Next(target) | Command::Previous(target) | Command::Set(target, _) => {
                target.styles(self.visible)
            }
            _ => Vec::new(),
        };
        self.waiting.insert(id, rotated);
        match command {
            Command::Previous(_) => {
                self.stepping_back.insert(id);
            }
            Command::Set(..) => {
                self.setting.insert(id);
            }
            _ => {}
        }
        if let Command::Resume(target) = command {
            // A resume ends an external-change pause and writes that key
            // back at once.
            for style in target.styles(self.visible) {
                self.taken[style] = false;
                self.reassert_after[style] = None;
            }
        }
        self.send(Event::Control {
            id: ReplyId(id),
            command: command.clone(),
        })?;
        match command {
            Command::Pause(target) => {
                for style in target.styles(self.visible) {
                    prop_assert!(self.is_user_paused(style), "{style} was not paused");
                }
            }
            Command::Resume(target) => {
                for style in target.styles(self.visible) {
                    prop_assert!(
                        !self.is_user_paused(style) && !self.is_paused(style),
                        "{style} was not resumed"
                    );
                }
            }
            // With no original keys saved, restore fails and pauses nothing.
            Command::Restore if self.original_keys.is_some() => {
                for style in Style::ALL {
                    prop_assert!(self.is_user_paused(style), "restore did not pause {style}");
                }
            }
            _ => {}
        }
        let is_answered_at_once = matches!(
            command,
            Command::Pause(_) | Command::Resume(_) | Command::Status | Command::Restore
        );
        prop_assert!(
            !is_answered_at_once || !self.waiting.contains_key(&id),
            "{command:?} was not answered at once"
        );
        Ok(())
    }

    /// Writes the original keys back for `Restore`, as GNOME does: a locked
    /// key that holds another value fails the write, and only a change is
    /// reported. Then answers the request.
    fn restore(&mut self, id: ReplyId, original: &BackgroundKeys) -> Result<(), TestCaseError> {
        // A key that this GNOME does not have is left alone.
        let wanted = PerStyle::from_fn(|style| {
            let uri = original.uri(style).filter(|_| self.key(style).is_some());
            uri.map(str::to_owned)
        });
        let is_refused = Style::ALL.into_iter().any(|style| {
            let wanted = wanted[style].as_deref();
            wanted.is_some() && self.locked[style] && self.key(style) != wanted
        });
        if is_refused {
            let result = Err("locked keys".to_owned());
            return self.send(Event::KeysRestored { id, result });
        }
        let before = self.gnome_keys();
        for style in Style::ALL {
            if let Some(uri) = wanted[style].clone() {
                self.set_key(style, uri);
            }
        }
        self.keys.picture_options = original.picture_options.clone();
        if self.gnome_keys() != before {
            self.send(keys_changed(self.gnome_keys()))?;
        }
        self.send(Event::KeysRestored { id, result: Ok(()) })
    }

    /// Something else writes a wallpaper of its own to `style`'s key.
    fn take_key(&mut self, style: Style) -> Result<(), TestCaseError> {
        if !self.is_key_free(style) {
            return Ok(());
        }
        let was_the_daemons =
            self.holds_key[style] && self.key(style) == Some(slot_uris()[style].as_str());
        self.foreign_wallpapers += 1;
        let wallpaper = format!("file:///elsewhere/{}.jpg", self.foreign_wallpapers);
        self.set_key(style, wallpaper);
        if was_the_daemons {
            match self.config.general.on_external_change {
                ExternalChangePolicy::Pause => self.taken[style] = true,
                ExternalChangePolicy::ReassertNow => {
                    self.reassert_after[style] = Some(self.now() + REASSERT_DELAY);
                }
                ExternalChangePolicy::ReassertNextTick => {}
            }
        }
        self.send(keys_changed(self.gnome_keys()))?;
        prop_assert!(
            !self.taken[style] || self.is_paused(style),
            "{style} goes on although its key was taken"
        );
        Ok(())
    }

    /// Something else writes the slot URI back to `style`'s key.
    fn return_key(&mut self, style: Style) -> Result<(), TestCaseError> {
        let slot_uri = slot_uris()[style].clone();
        if !self.is_key_free(style) || self.key(style) == Some(slot_uri.as_str()) {
            return Ok(());
        }
        self.set_key(style, slot_uri);
        self.taken[style] = false;
        self.reassert_after[style] = None;
        self.write_back_due[style] = false;
        self.send(keys_changed(self.gnome_keys()))?;
        prop_assert!(
            !self.is_paused(style),
            "{style} stays paused although its key names the slot again"
        );
        Ok(())
    }

    /// The config file is saved again. The daemon scans the sources that
    /// changed, and every pause ends.
    fn reload(&mut self, reload: &Reload) -> Result<(), TestCaseError> {
        let mut config = reload.config.clone();
        for style in Style::ALL {
            config.styles[style].sources = if reload.keeps_sources {
                self.config.styles[style].sources.clone()
            } else {
                reload.folders.sources(style)
            };
        }
        let old = mem::replace(&mut self.config, config.clone());
        self.reloaded_folders = Some(reload.folders.clone());
        self.taken = PerStyle::default();
        self.forget_write_backs();
        // The report of a key write is a step of its own, after this one.
        let reload_step = self.trace.len();
        self.send(Event::ConfigChanged(Ok(config)))?;
        self.reloaded_folders = None;
        let effects = &self.trace[reload_step];
        for style in Style::ALL {
            let sources = self.config.styles[style].sources.clone();
            let has_new_sources = sources != old.styles[style].sources;
            let is_scanned_again = effects.contains(&Effect::Watch { style, sources });
            prop_assert_eq!(
                is_scanned_again,
                has_new_sources,
                "{} sources are scanned again exactly when they changed",
                style
            );
            prop_assert!(!self.is_paused(style), "a reload did not end a pause");
        }
        Ok(())
    }

    /// The config file is saved with a mistake in it: nothing changes. As
    /// any event may, this one can make the daemon arm its timer anew.
    fn fail_reload(&mut self) -> Result<(), TestCaseError> {
        let mistake = ConfigError {
            line: 2,
            column: 5,
            message: "unknown key".to_owned(),
        };
        self.send(Event::ConfigChanged(Err(mistake)))?;
        prop_assert_eq!(self.model.config(), &self.config);
        let effects = self.trace.last().expect("the step that was just sent");
        let does_something = |effect: &Effect| !matches!(effect, Effect::Arm(_));
        prop_assert!(
            !effects.iter().any(does_something),
            "an invalid config file had effects: {effects:?}"
        );
        Ok(())
    }

    /// A timerfd is one-shot: once it fired, nothing is armed.
    fn fire(&mut self) -> Result<(), TestCaseError> {
        self.armed = None;
        self.send(Event::Fired(Fired::Deadline))
    }

    /// The clock went back: nothing waits longer than its whole delay from
    /// now.
    fn shorten_waits(&mut self) {
        let now = self.now();
        for wait in self.still_copied.values_mut() {
            wait.waits_until = wait
                .waits_until
                .map(|until| until.min(now + PROBE_RETRY_DELAY));
        }
        self.refresh_from = self.refresh_from.min(now + HIDDEN_REFRESH_DELAY);
        for style in Style::ALL {
            self.reassert_after[style] =
                self.reassert_after[style].map(|after| after.min(now + REASSERT_DELAY));
        }
    }

    /// The daemon handles an event now, and sees which waits are over.
    fn end_waits(&mut self) {
        let now = self.now();
        for (version, wait) in &mut self.still_copied {
            wait.waits_until = wait.waits_until.filter(|until| now < *until);
            if wait.waits_until.is_none() {
                for style in Style::ALL {
                    self.held_back[style].remove(version);
                }
            }
        }
        for style in Style::ALL {
            if self.reassert_after[style].is_some_and(|after| after <= now) {
                self.reassert_after[style] = None;
                self.write_back_due[style] = true;
            }
        }
    }

    /// The scan of `style`'s folder comes back. From now on the watcher
    /// reports every change to it.
    fn finish_scan(&mut self, style: Style) -> Result<(), TestCaseError> {
        if !self.scanning[style] {
            return Ok(());
        }
        self.scanning[style] = false;
        let found = self.scan(style);
        let files = local_files(&found);
        // A version that left this catalog comes back with no verdict.
        let still_holds = |(item, saves): &Version| found.get(item) == Some(saves);
        self.known_bad[style].retain(still_holds);
        self.held_back[style].retain(still_holds);
        self.told[style] = Some(found);
        // The other style's catalog was not replaced, so its bad, truncated,
        // and tried sets stay. Clearing them hides a probe the reducer still
        // refuses.
        self.send(Event::Scanned { style, files })
    }

    /// The watcher tells every style whose folder covers `image` what became
    /// of it.
    fn report_file(&mut self, image: &'static str) -> Result<(), TestCaseError> {
        let item = id(image);
        // Whatever was known of the file is void.
        for style in Style::ALL {
            self.known_bad[style].retain(|(bad, _)| *bad != item);
            self.held_back[style].retain(|(held, _)| *held != item);
        }
        self.still_copied.retain(|(copied, _), _| *copied != item);
        for style in Style::ALL {
            self.tried[style].retain(|(tried, _)| *tried != item);
        }
        for style in Style::ALL {
            let is_watched = self.folders.of(style).contains(&image) && !self.scanning[style];
            let Some(told) = self.told[style].as_mut().filter(|_| is_watched) else {
                continue;
            };
            let change = if self.on_disk.contains(image) {
                let saves = self.saves.get(image).copied().unwrap_or(0);
                told.insert(item.clone(), saves);
                FsChange::Found(version(image, saves))
            } else {
                told.remove(&item);
                FsChange::Removed(PathBuf::from(image))
            };
            let changes = vec![change];
            self.send(Event::Fs { style, changes })?;
        }
        Ok(())
    }

    fn answer_oldest(
        &mut self,
        publish_fails: bool,
        still_copied: bool,
    ) -> Result<(), TestCaseError> {
        match self.unanswered.pop_front() {
            Some(Effect::Probe { style, item, .. }) => {
                let verdict = if self.scenario.broken.contains(&item) {
                    Err(BadReason::NotAnImage)
                } else if still_copied {
                    Err(BadReason::Truncated)
                } else {
                    Ok(())
                };
                self.note_verdict(style, &item, &verdict);
                self.send(Event::Probed {
                    style,
                    item,
                    verdict,
                })
            }
            Some(Effect::Publish { style, item, .. }) => {
                let before = self.model.persisted().styles[style].clone();
                // The reply may go out in the same step.
                let is_off_order = self.is_stepping_back(style) || self.is_setting(style);
                let result = self.write_slot(style, publish_fails);
                let is_written_while_hidden = result.is_ok() && style != self.visible;
                self.send(Event::Published {
                    style,
                    item,
                    result,
                })?;
                let is_dirty = self.model.persisted().styles[style].dirty_while_hidden;
                prop_assert!(
                    !is_written_while_hidden || is_dirty,
                    "the {style} slot was written while hidden and is not dirty"
                );
                // On screen, the mark can only go: its refresh may follow at
                // once.
                let got_dirty = is_dirty && !before.dirty_while_hidden;
                prop_assert!(
                    is_written_while_hidden || !got_dirty,
                    "the {style} slot got dirty on screen"
                );
                self.check_round(style, &before, is_off_order)
            }
            Some(Effect::Republish(style)) => {
                let result = match self.slots[style] {
                    Some(_) => self.write_slot(style, publish_fails),
                    None => Err("No such file or directory".to_owned()),
                };
                // A failure spends the mark, and so does a success that was
                // on screen all along. Otherwise the write may have landed
                // unwatched: the mark stays for the next refresh.
                let was_hidden = mem::take(&mut self.hidden_while_refreshing[style]);
                let keeps_mark = result.is_ok() && (style != self.visible || was_hidden);
                self.send(Event::Republished { style, result })?;
                let is_dirty = self.model.persisted().styles[style].dirty_while_hidden;
                prop_assert_eq!(is_dirty, keeps_mark, "the {} refresh answer", style);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Records what the daemon must remember of a verdict: a version that
    /// failed for good, or one that waits for another probe.
    fn note_verdict(&mut self, style: Style, item: &ItemId, verdict: &Result<(), BadReason>) {
        // The daemon sees which waits are over before it hears the verdict.
        self.end_waits();
        // A verdict counts only for a version that a catalog still holds.
        // Which version was probed is unknown while the two catalogs hold
        // different ones.
        let Some((_, sightings)) = &self.probing[style] else {
            return;
        };
        let [Sighting::File(saves)] = sightings[..] else {
            return;
        };
        let probed = (item.clone(), saves);
        // Only the catalogs that hold the version take the verdict.
        let holders: Vec<Style> = Style::ALL
            .into_iter()
            .filter(|&style| self.holds(style, &probed))
            .collect();
        if holders.is_empty() {
            return;
        }
        // The latest verdict about a version is the one that counts.
        for &style in &holders {
            self.known_bad[style].remove(&probed);
            self.held_back[style].remove(&probed);
        }
        let earlier = self.still_copied.remove(&probed);
        match verdict {
            Ok(()) => {}
            Err(BadReason::Truncated) => self.note_truncated(probed, earlier, &holders),
            Err(_) => self.mark_bad(&probed, &holders),
        }
    }

    /// Whether `style`'s catalog holds `version`.
    fn holds(&self, style: Style, (item, saves): &Version) -> bool {
        let told = self.told[style].as_ref();
        told.is_some_and(|told| told.get(item) == Some(saves))
    }

    fn mark_bad(&mut self, version: &Version, holders: &[Style]) {
        for &style in holders {
            self.known_bad[style].insert(version.clone());
        }
    }

    fn hold_back(&mut self, version: &Version, holders: &[Style]) {
        for &style in holders {
            self.held_back[style].insert(version.clone());
        }
    }

    /// A truncated version waits for its next probe, until it had all of
    /// them. `earlier` is what was known of the version before. `holders`
    /// are the catalogs that hold it.
    fn note_truncated(&mut self, probed: Version, earlier: Option<StillCopied>, holders: &[Style]) {
        // Both styles can probe one file at the same time. The second
        // verdict finds the version waiting: that is no second probe spent.
        if let Some(waiting) = earlier.filter(|wait| wait.waits_until.is_some()) {
            self.hold_back(&probed, holders);
            self.still_copied.insert(probed, waiting);
            return;
        }
        let verdicts = earlier.map_or(0, |wait| wait.verdicts) + 1;
        let has_probes_left = verdicts <= MAX_PROBE_RETRIES;
        if has_probes_left {
            self.hold_back(&probed, holders);
        } else {
            self.mark_bad(&probed, holders);
        }
        let wait = StillCopied {
            verdicts,
            waits_until: has_probes_left.then(|| self.now() + PROBE_RETRY_DELAY),
        };
        self.still_copied.insert(probed, wait);
    }

    /// Whether `style` may probe this version now.
    fn is_candidate(&self, style: Style, version: &Version) -> bool {
        let is_refused = |pool: Style| {
            self.known_bad[pool].contains(version) || self.held_back[pool].contains(version)
        };
        !self.is_refused_by_every_pool(style, version, is_refused)
    }

    /// Whether every catalog that `style` may pick from and that holds
    /// `version` knows it is bad. A catalog that learned the file after the
    /// verdict still probes it.
    fn is_known_bad(&self, style: Style, version: &Version) -> bool {
        self.is_refused_by_every_pool(style, version, |pool| {
            self.known_bad[pool].contains(version)
        })
    }

    /// Whether at least one catalog that `style` may pick from holds
    /// `version`, and each of them refuses it.
    fn is_refused_by_every_pool(
        &self,
        style: Style,
        version: &Version,
        refuses: impl Fn(Style) -> bool,
    ) -> bool {
        let may_borrow = self.config.general.empty_style == EmptyStylePolicy::UseOtherStyle;
        let mut holders = [Some(style), may_borrow.then(|| style.other())]
            .into_iter()
            .flatten()
            .filter(|&pool| self.holds(pool, version))
            .peekable();
        holders.peek().is_some() && holders.all(refuses)
    }

    fn write_slot(&mut self, style: Style, fails: bool) -> Result<SlotWrite, String> {
        if fails {
            return Err("No space left on device".to_owned());
        }
        self.last_slot_inode += 1;
        let slot = FileId {
            dev: 2,
            ino: self.last_slot_inode,
        };
        self.slots[style] = Some(slot);
        Ok(SlotWrite {
            method: Method::Reflink,
            slot,
        })
    }

    /// Checks that a shuffled style showed no image twice in one round.
    /// `before` is the style's saved state from before the publish answer,
    /// and `is_off_order` whether a `Previous` brought an image back or a
    /// `Set` chose it.
    fn check_round(
        &self,
        style: Style,
        before: &StylePersisted,
        is_off_order: bool,
    ) -> Result<(), TestCaseError> {
        let after = &self.model.persisted().styles[style];
        let is_shuffled = self.config.styles[style].order == Order::Random;
        let Some(shown) = after.current.as_ref().filter(|_| is_shuffled) else {
            return Ok(());
        };
        if before.current.as_ref() == Some(shown) || is_off_order {
            // The slot was written again with the image it had, got an
            // image back from its history, or the one a `Set` named.
            return Ok(());
        }
        // Every state file here is this model's own, which knows what its
        // rounds showed.
        let (shown_before, shown_after) = (
            before.shown.as_deref().unwrap_or_default(),
            after.shown.as_deref().unwrap_or_default(),
        );
        // A request that waited for this publish drew the first image of a
        // new round in the same step.
        if shown_after.is_empty() && self.is_requested(style) && !self.is_idle(style) {
            return Ok(());
        }
        let distinct: BTreeSet<&ItemId> = shown_after.iter().collect();
        prop_assert_eq!(
            distinct.len(),
            shown_after.len(),
            "a round lists an image twice"
        );
        prop_assert_eq!(shown_after.last(), Some(shown), "an image had no turn");
        let starts_a_round = shown_after.len() == 1;
        prop_assert!(
            starts_a_round || !shown_before.contains(shown),
            "{style} showed {shown} twice in one round"
        );
        Ok(())
    }

    fn restart(&mut self, boot: &Boot, lost_slots: PerStyle<bool>) -> Result<(), TestCaseError> {
        for style in Style::ALL {
            if lost_slots[style] {
                self.slots[style] = None;
            }
        }
        self.start_run(boot, self.state_file.clone())
    }

    /// Lets every scan come back and answers everything still owed, with no
    /// more failures.
    pub fn settle(&mut self) -> Result<(), TestCaseError> {
        for style in Style::ALL {
            self.finish_scan(style)?;
        }
        for _ in 0..MAX_ANSWERS_OWED {
            if self.unanswered.is_empty() {
                return Ok(());
            }
            self.answer_oldest(false, false)?;
        }
        prop_assert!(self.unanswered.is_empty(), "rotations never come to an end");
        Ok(())
    }

    pub fn shut_down(&mut self) -> Result<(), TestCaseError> {
        self.is_stopped = true;
        self.send(Event::Shutdown)?;
        let last = self.trace.last().expect("the shutdown step");
        let is_reply = |effect: &&Effect| matches!(effect, Effect::Reply { .. });
        let (replies, rest): (Vec<&Effect>, Vec<&Effect>) = last.iter().partition(is_reply);
        prop_assert_eq!(rest, vec![&Effect::Arm(None), &Effect::Persist]);
        let is_refusal = |effect: &&Effect| {
            matches!(
                effect,
                Effect::Reply {
                    reply: Reply::Failed(_),
                    ..
                }
            )
        };
        prop_assert!(
            replies.iter().all(is_refusal),
            "a request succeeded at shutdown"
        );
        prop_assert!(self.waiting.is_empty(), "requests left without a reply");
        self.fire()?;
        prop_assert_eq!(self.trace.last(), Some(&Vec::new()));
        Ok(())
    }

    /// Feeds `event` to the daemon. GNOME reports every change to the keys,
    /// the daemon's too, so a key write is followed by its report.
    fn send(&mut self, event: Event) -> Result<(), TestCaseError> {
        let wrote_keys = self.step_checked(event)?;
        if wrote_keys {
            let wrote_again = self.step_checked(keys_changed(self.gnome_keys()))?;
            // Against a real GNOME the two would never stop.
            prop_assert!(
                !wrote_again,
                "the report of the daemon's own key write was answered with another write"
            );
        }
        for (id, original) in mem::take(&mut self.restores_owed) {
            self.restore(id, &original)?;
        }
        Ok(())
    }

    /// One step of the reducer, with every effect checked. Returns whether
    /// the step wrote keys.
    fn step_checked(&mut self, event: Event) -> Result<bool, TestCaseError> {
        let now = self.now();
        self.end_waits();
        self.before = self.model.persisted();
        let effects = step(&mut self.model, now, event.clone());
        self.replied_in_step = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Reply { id, .. } => Some(id.0),
                _ => None,
            })
            .collect();
        self.walks_ended = PerStyle::from_fn(|style| {
            let ends_walk = |id: &u64| {
                self.stepping_back.contains(id)
                    && self
                        .waiting
                        .get(id)
                        .is_some_and(|styles| styles.contains(&style))
            };
            self.replied_in_step.iter().any(ends_walk)
        });
        self.saved_in_step = effects.contains(&Effect::Persist);
        if self.saved_in_step {
            // `persist` writes the model after `step` returns, not a later one.
            self.state_file = Some(self.model.persisted());
        }
        for effect in &effects {
            self.check(&event, effect)?;
        }
        let saves = effects
            .iter()
            .filter(|effect| **effect == Effect::Persist)
            .count();
        prop_assert!(saves <= 1, "the state was saved twice in one step");
        if matches!(event, Event::Published { result: Ok(_), .. }) {
            prop_assert_eq!(saves, 1, "a rotation was not saved");
        }
        let wrote_keys = effects.iter().any(is_set_keys);
        self.trace.push(effects);
        self.check_short_waits()?;
        Ok(wrote_keys)
    }

    /// After every step: what is due was done, and the timer is armed for
    /// what is not due yet.
    fn check_short_waits(&self) -> Result<(), TestCaseError> {
        if self.is_stopped || !self.has_started {
            return Ok(());
        }
        let now = self.now();
        let is_armed_by = |due: SystemTime| self.armed.is_some_and(|armed| armed <= due);
        if self.awaits_refresh() {
            let due = self.refresh_from;
            prop_assert!(now < due, "a dirty slot on screen was not republished");
            prop_assert!(
                is_armed_by(due),
                "the refresh of a dirty slot would be missed"
            );
        }
        let saved = self.model.persisted();
        for style in Style::ALL {
            // The guard against a tool that fights for the key took over.
            if saved.styles[style].paused {
                continue;
            }
            let can_write = self.unanswered.is_empty() && saved.styles[style].slot.is_some();
            prop_assert!(
                !self.write_back_due[style] || !can_write,
                "the {style} key was not written back"
            );
            if let Some(due) = self.reassert_after[style] {
                prop_assert!(is_armed_by(due), "a write-back would be missed");
            }
        }
        Ok(())
    }

    /// Whether the slot on screen is dirty and nothing stands in the way of
    /// its refresh.
    fn awaits_refresh(&self) -> bool {
        let style = self.visible;
        let saved = &self.model.persisted().styles[style];
        let is_slot_shown = saved.slot.is_some() && !saved.paused && !self.disabled[style];
        saved.dirty_while_hidden && is_slot_shown && self.is_idle(style)
    }

    /// Checks one effect against the invariants and records what it changes.
    fn check(&mut self, cause: &Event, effect: &Effect) -> Result<(), TestCaseError> {
        match effect {
            Effect::Probe {
                style,
                item,
                path,
                max_pixels,
            } => {
                prop_assert!(self.is_idle(*style), "two rotations of {style} in flight");
                prop_assert!(!self.disabled[*style], "{style} has no key to show images");
                prop_assert_eq!(path, &PathBuf::from(item.as_str()));
                prop_assert_eq!(*max_pixels, self.config.general.max_pixels);
                self.check_candidate(cause, *style, item)?;
                self.unanswered.push_back(effect.clone());
            }
            Effect::Publish {
                style, item, path, ..
            } => {
                let passed_probe = Event::Probed {
                    style: *style,
                    item: item.clone(),
                    verdict: Ok(()),
                };
                prop_assert_eq!(cause, &passed_probe, "published without a passed probe");
                prop_assert!(
                    !self.scenario.broken.contains(item),
                    "published a broken image"
                );
                prop_assert_eq!(path, &PathBuf::from(item.as_str()));
                self.check_published_version(*style, item)?;
                let is_requested = self.is_requested(*style);
                let is_repair =
                    self.shows(*style, item) || self.before.styles[*style].slot.is_none();
                prop_assert!(
                    self.is_running(*style) || self.shows(*style, item) || is_requested,
                    "a hidden slot that needed no repair was written"
                );
                prop_assert!(
                    !self.is_held(*style) || is_requested || is_repair,
                    "{style} rotated while the user or a locked screen paused it"
                );
                prop_assert!(self.is_idle(*style), "two rotations of {style} in flight");
                self.unanswered.push_back(effect.clone());
            }
            Effect::Republish(style) => self.check_refresh(*style)?,
            Effect::Watch { style, sources } => self.check_watch(cause, *style, sources)?,
            Effect::Arm(deadline) => {
                if let Some(deadline) = deadline {
                    let latest = self.now() + self.longest_wait();
                    prop_assert!(self.now() < *deadline, "armed for a time that has passed");
                    prop_assert!(*deadline <= latest, "armed further ahead than any wait");
                }
                self.armed = *deadline;
            }
            Effect::SetKeys {
                uris,
                picture_options,
            } => {
                let names_a_key = Style::ALL.iter().any(|&style| uris[style].is_some());
                let holds_a_key = Style::ALL.iter().any(|&style| self.holds_key[style]);
                prop_assert!(
                    names_a_key || picture_options.is_some(),
                    "a key write that names no key"
                );
                prop_assert!(
                    names_a_key || holds_a_key,
                    "picture-options written although no key is the daemon's"
                );
                prop_assert!(
                    self.unanswered.is_empty(),
                    "keys written while a slot write is in flight"
                );
                for style in Style::ALL {
                    if let Some(uri) = &uris[style] {
                        self.check_key_write(style, uri)?;
                    }
                }
                if let Some(options) = picture_options {
                    self.keys.picture_options = options.as_str().to_owned();
                }
            }
            Effect::Ready => {
                prop_assert!(!self.is_ready, "READY sent twice in one run");
                prop_assert!(
                    self.unanswered.is_empty(),
                    "READY before the first apply settled"
                );
                self.is_ready = true;
            }
            Effect::Report(status) => {
                prop_assert_eq!(status.visible, self.visible);
                self.check_next_change(status.next_change)?;
                for style in Style::ALL {
                    prop_assert!(
                        !self.is_held(style) || status.paused[style].is_some(),
                        "the status hides that {style} is paused"
                    );
                }
            }
            Effect::Persist | Effect::CollectGarbage { .. } => {}
            Effect::Fetch(url) | Effect::DropRemote(url) => {
                prop_assert!(false, "{url} is in no config, yet its fetch changed");
            }
            Effect::Changed { style, item } => {
                let is_its_publish = matches!(
                    cause,
                    Event::Published { style: published, item: shown, result: Ok(_) }
                        if published == style && shown == item
                );
                prop_assert!(is_its_publish, "{effect:?} without its publish");
            }
            Effect::Reply { id, .. } => {
                let waited = self.waiting.remove(&id.0);
                prop_assert!(waited.is_some(), "a second reply, or one nobody asked for");
                self.stepping_back.remove(&id.0);
                self.setting.remove(&id.0);
            }
            Effect::RestoreKeys { id, keys } => {
                let is_restore = matches!(
                    cause,
                    Event::Control {
                        command: Command::Restore,
                        ..
                    }
                );
                prop_assert!(is_restore, "{effect:?} without a restore");
                prop_assert_eq!(Some(keys), self.original_keys.as_ref());
                // The keys are the user's until `Resume`.
                for style in Style::ALL {
                    self.holds_key[style] = false;
                    self.taken[style] = false;
                    self.reassert_after[style] = None;
                    self.write_back_due[style] = false;
                }
                self.restores_owed.push((*id, keys.clone()));
            }
            Effect::ReadConfig(_) => {
                prop_assert!(false, "{effect:?} without a reload request");
            }
            Effect::SaveFavorites { .. } => {
                prop_assert!(false, "{effect:?} without a favorite request");
            }
            Effect::RunHook(run) => self.check_hook_run(cause, run)?,
        }
        Ok(())
    }

    /// `on_change` runs with what its style shows after the step, for a
    /// publish of another image or for a switch to that style.
    fn check_hook_run(&self, cause: &Event, run: &HookRun) -> Result<(), TestCaseError> {
        prop_assert_eq!(Some(&run.command), self.config.general.on_change.as_ref());
        let current = &self.model.persisted().styles[run.style].current;
        prop_assert_eq!(Some(&run.item), current.as_ref());
        prop_assert_eq!(run.path.as_ref(), Some(&PathBuf::from(run.item.as_str())));
        prop_assert_eq!(run.visible, run.style == self.visible);
        match run.event {
            HookEvent::Changed => {
                let is_its_publish = matches!(
                    cause,
                    Event::Published { style, item, result: Ok(_) }
                        if *style == run.style && *item == run.item
                );
                prop_assert!(is_its_publish, "{run:?} without its publish");
                let shown_before = self.before.styles[run.style].current.as_ref();
                prop_assert_ne!(
                    shown_before,
                    Some(&run.item),
                    "the image was on screen already"
                );
            }
            HookEvent::Switched => {
                let is_its_switch = matches!(
                    cause,
                    Event::Gnome(GnomeEvent::StyleChanged(style)) if *style == run.style
                );
                prop_assert!(is_its_switch, "{run:?} without its switch");
            }
        }
        Ok(())
    }

    /// A slot is republished only while it is dirty and on screen, once the
    /// shell has had its time with it, and never next to another write of
    /// the slot. That uses the mark up.
    fn check_refresh(&mut self, style: Style) -> Result<(), TestCaseError> {
        let saved = &self.model.persisted().styles[style];
        prop_assert!(
            self.is_idle(style),
            "two writes of the {style} slot in flight"
        );
        prop_assert_eq!(style, self.visible, "a hidden slot was republished");
        prop_assert!(
            self.slots[style].is_some(),
            "a missing slot was republished"
        );
        prop_assert!(
            self.before.styles[style].dirty_while_hidden,
            "a clean slot was republished"
        );
        prop_assert!(
            saved.dirty_while_hidden,
            "a refresh dropped its mark before its answer"
        );
        prop_assert!(
            self.refresh_from <= self.now(),
            "a slot was republished before the shell watches it"
        );
        prop_assert!(
            !self.disabled[style] && !saved.paused,
            "a slot that its key does not name was republished"
        );
        self.unanswered.push_back(Effect::Republish(style));
        Ok(())
    }

    /// The sources are scanned and watched when a run starts, and again
    /// when a reload changes them.
    fn check_watch(
        &mut self,
        cause: &Event,
        style: Style,
        sources: &[Source],
    ) -> Result<(), TestCaseError> {
        prop_assert_eq!(sources, self.config.styles[style].sources.as_slice());
        match cause {
            Event::Started { .. } => {
                prop_assert!(!self.scanning[style], "{style} sources watched twice");
            }
            // The watch of the new sources replaces the one before.
            Event::ConfigChanged(Ok(_)) => {
                if let Some(folders) = &self.reloaded_folders {
                    *self.folders.of_mut(style) = folders.of(style).to_vec();
                }
            }
            _ => prop_assert!(false, "the sources are watched anew without a reason"),
        }
        self.scanning[style] = true;
        Ok(())
    }

    /// A key is written only when it must be: it names an existing slot, it
    /// is the daemon's to write, and it does not hold the slot URI through
    /// an earlier write. `reassert-now` waits its delay first.
    fn check_key_write(&mut self, style: Style, uri: &str) -> Result<(), TestCaseError> {
        let slot_uri = slot_uris()[style].clone();
        prop_assert_eq!(uri, slot_uri.as_str());
        prop_assert!(
            self.slots[style].is_some(),
            "the {style} key points at a missing slot file"
        );
        prop_assert!(!self.disabled[style], "a locked or missing key was written");
        prop_assert!(
            !self.taken[style] && !self.is_paused(style),
            "the {style} key was written while its style is paused"
        );
        let holds_the_uri = self.holds_key[style] && self.key(style) == Some(uri);
        prop_assert!(!holds_the_uri, "the {style} key was written for nothing");
        let is_handed_back = self.is_user_paused(style) && !self.holds_key[style];
        prop_assert!(
            !is_handed_back,
            "the {style} key was written although it is the user's until a resume"
        );
        prop_assert!(
            self.reassert_after[style].is_none(),
            "the {style} key was written back too soon"
        );
        self.write_back_due[style] = false;
        self.set_key(style, uri.to_owned());
        self.holds_key[style] = true;
        Ok(())
    }

    /// Records `item` as the next candidate of `style`'s rotation. One
    /// rotation tries no version of an image twice and at most
    /// [`MAX_PUBLISH_ATTEMPTS`] images. No rotation probes a version that is
    /// known to be bad, or one that is still copied before its wait is over.
    /// The image that a lost slot showed is its repair, which starts anew.
    ///
    /// Which version is probed is unknown while the two catalogs hold
    /// different ones, so then an image may come twice.
    fn check_candidate(
        &mut self,
        cause: &Event,
        style: Style,
        item: &ItemId,
    ) -> Result<(), TestCaseError> {
        let sightings = self.sightings(style, item);
        prop_assert!(
            !sightings.is_empty(),
            "{style} probed {item}, which is not there"
        );
        let is_known_bad = |sighting: &Sighting| match sighting {
            Sighting::SavedState => false,
            Sighting::File(saves) => self.is_known_bad(style, &(item.clone(), *saves)),
        };
        let may_be_probed = |sighting: &Sighting| match sighting {
            Sighting::SavedState => true,
            Sighting::File(saves) => self.is_candidate(style, &(item.clone(), *saves)),
        };
        prop_assert!(
            !sightings.iter().all(is_known_bad),
            "probed a bad image again"
        );
        prop_assert!(
            sightings.iter().any(may_be_probed),
            "probed a truncated image before its wait was over"
        );
        if !self.continues_rotation(cause, style)
            || self.repairs_lost_slot(style, item)
            || self.serves_request_after_giving_up(style)
            || self.walks_ended[style]
        {
            self.tried[style].clear();
        }
        // A `Previous` walks the history, which may name an image twice and
        // ends with it.
        let steps_back = self.is_stepping_back(style);
        let tried = &mut self.tried[style];
        let candidate = (item.clone(), sightings);
        let is_one_version = candidate.1.len() == 1;
        prop_assert!(
            steps_back || !is_one_version || !tried.contains(&candidate),
            "tried an image twice in one rotation"
        );
        tried.push(candidate.clone());
        self.probing[style] = Some(candidate);
        let tried = &self.tried[style];
        let images: BTreeSet<&ItemId> = tried.iter().map(|(image, _)| image).collect();
        prop_assert!(
            steps_back || images.len() <= MAX_PUBLISH_ATTEMPTS as usize,
            "one rotation tried too many candidates"
        );
        prop_assert!(
            tried.len() <= HISTORY_LEN + MAX_PUBLISH_ATTEMPTS as usize,
            "a walk through the history did not end"
        );
        Ok(())
    }

    /// Only an image that is there is published, and only in the version
    /// that was probed.
    fn check_published_version(&self, style: Style, item: &ItemId) -> Result<(), TestCaseError> {
        let sightings = self.sightings(style, item);
        prop_assert!(
            !sightings.is_empty(),
            "{style} published {item}, which left or never was there"
        );
        let Some((probed, sightings_then)) = &self.probing[style] else {
            return Err(TestCaseError::fail("published without a probe"));
        };
        prop_assert_eq!(probed, item, "published another image than the probed one");
        // An image that only the state file named was probed by its path.
        let was_probed =
            |sighting: &Sighting| *sighting == Sighting::SavedState || sightings.contains(sighting);
        prop_assert!(
            sightings_then.iter().any(was_probed),
            "published a file that changed since its probe"
        );
        Ok(())
    }

    /// The timer is armed for the next change, or for something sooner: the
    /// next probe of a truncated image, or a key that waits on its own.
    fn check_next_change(&self, next_change: Option<SystemTime>) -> Result<(), TestCaseError> {
        match (self.armed, next_change) {
            (None, None) => {}
            (Some(armed), None) => {
                let longest = self
                    .own_reassert_waits()
                    .chain([PROBE_RETRY_DELAY])
                    .max()
                    .expect("there is the probe retry");
                prop_assert!(
                    armed <= self.now() + longest,
                    "armed although nothing is due"
                );
            }
            (armed, Some(next_change)) => {
                let is_armed_in_time = armed.is_some_and(|armed| armed <= next_change);
                prop_assert!(is_armed_in_time, "the next change would be missed");
            }
        }
        Ok(())
    }

    /// Whether the probe that `cause` leads to continues `style`'s rotation
    /// with its next candidate: `cause` is a failed probe or publish of that
    /// style. A rotation that only publishes again what its slot shows has
    /// no next candidate, so a probe after its failure starts a new one.
    fn continues_rotation(&self, cause: &Event, style: Style) -> bool {
        let (answered, failed) = match cause {
            Event::Probed {
                style,
                item,
                verdict: Err(_),
            }
            | Event::Published {
                style,
                item,
                result: Err(_),
            } => (*style, item),
            _ => return false,
        };
        answered == style && !self.shows(style, failed)
    }

    /// Whether the step in progress ended `style`'s rotation and then
    /// started the rotation of a request that waited for it: the rotation
    /// gave up, which saves the state, or it ran for a request that this
    /// step replies to.
    fn serves_request_after_giving_up(&self, style: Style) -> bool {
        // A rotation that moves on to its next candidate saves nothing.
        let gave_up = self.saved_in_step;
        let rotates = |(_, styles): &(&u64, &Vec<Style>)| styles.contains(&style);
        let (finished, next): (Vec<_>, Vec<_>) = self
            .waiting
            .iter()
            .filter(rotates)
            .partition(|(id, _)| self.replied_in_step.contains(id));
        (gave_up || !finished.is_empty()) && !next.is_empty()
    }

    /// Whether `style`'s slot file is gone and held `item`. Publishing that
    /// image again is the repair of the lost slot.
    fn repairs_lost_slot(&self, style: Style, item: &ItemId) -> bool {
        let saved = self.model.persisted();
        self.slots[style].is_none() && saved.styles[style].current.as_ref() == Some(item)
    }

    /// Whether `style`'s slot file exists and holds `item`, as far as the
    /// daemon knows. Publishing that image again is a repair.
    fn shows(&self, style: Style, item: &ItemId) -> bool {
        let saved = self.model.persisted();
        self.slots[style].is_some() && saved.styles[style].current.as_ref() == Some(item)
    }

    fn is_idle(&self, style: Style) -> bool {
        !self.unanswered.iter().any(|effect| match effect {
            Effect::Probe { style: busy, .. }
            | Effect::Publish { style: busy, .. }
            | Effect::Republish(busy) => *busy == style,
            _ => false,
        })
    }

    /// Whether `style`'s clock runs and its slot may change: it is on screen,
    /// hidden styles rotate too, or its slot file is still to be filled.
    fn is_running(&self, style: Style) -> bool {
        style == self.visible || self.config.general.rotate_hidden || self.slots[style].is_none()
    }

    /// The longest wait that the timer can be armed for: the interval of a
    /// style that rotates, or one of the short waits.
    fn longest_wait(&self) -> Duration {
        let rotates = |style: &Style| {
            let is_paused = self.is_paused(*style) || self.is_held(*style);
            self.is_running(*style) && !self.disabled[*style] && !is_paused
        };
        let intervals = Style::ALL
            .into_iter()
            .filter(rotates)
            .map(|style| self.config.styles[style].interval);
        let short_waits = [PROBE_RETRY_DELAY, REASSERT_DELAY, HIDDEN_REFRESH_DELAY];
        intervals
            .chain(self.own_reassert_waits())
            .chain(short_waits)
            .max()
            .expect("there are short waits")
    }

    /// Under `reassert-next-tick` a hidden style with no clock to wait on
    /// writes its taken key back one interval of its own later.
    fn own_reassert_waits(&self) -> impl Iterator<Item = Duration> + '_ {
        let applies =
            self.config.general.on_external_change == ExternalChangePolicy::ReassertNextTick;
        Style::ALL
            .into_iter()
            .filter(move |style| applies && !self.disabled[*style])
            .map(|style| self.config.styles[style].interval)
    }

    /// How the daemon knows of `item` as an image that `style` may show: a
    /// file of its own folder, or of the other style's unless the policy
    /// forbids that. Those are two versions while one of the styles has not
    /// heard of a change yet. Before the style's first scan result it is an
    /// image that the state file names. Empty for any other image.
    fn sightings(&self, style: Style, item: &ItemId) -> Vec<Sighting> {
        let Some(own) = &self.told[style] else {
            let is_saved = self.saved_current[style].as_ref() == Some(item)
                || self.saved_history[style].contains(item);
            return is_saved
                .then_some(Sighting::SavedState)
                .into_iter()
                .collect();
        };
        let may_borrow = self.config.general.empty_style == EmptyStylePolicy::UseOtherStyle;
        let borrowed = self.told[style.other()].as_ref().filter(|_| may_borrow);
        let mut versions: Vec<u64> = [Some(own), borrowed]
            .into_iter()
            .flatten()
            .filter_map(|told| told.get(item).copied())
            .collect();
        versions.dedup();
        versions.into_iter().map(Sighting::File).collect()
    }
}

/// The files of a scan result, as the scanner reports them.
pub fn local_files(found: &BTreeMap<ItemId, u64>) -> Vec<LocalFile> {
    found
        .iter()
        .map(|(item, saves)| version(item.as_str(), *saves))
        .collect()
}

pub fn new_model(
    config: &Config,
    local_files: PerStyle<Option<Vec<LocalFile>>>,
    seed: u64,
    restored: Option<Persisted>,
) -> Model {
    Model::new(ModelInit {
        config: Some(config.clone()),
        local_files,
        restored,
        seed,
        slot_uris: slot_uris(),
        remote_cache_dir: PathBuf::from(REMOTE_CACHE_DIR),
    })
}

fn restored(styles: PerStyle<StylePersisted>, files: PerStyle<Option<Vec<LocalFile>>>) -> Model {
    Model::new(ModelInit {
        config: Some(Config::default()),
        local_files: files,
        restored: Some(Persisted {
            version: STATE_VERSION,
            styles,
            original_keys: None,
            banned: Default::default(),
        }),
        seed: 1,
        slot_uris: slot_uris(),
        remote_cache_dir: PathBuf::from(REMOTE_CACHE_DIR),
    })
}

#[test]
fn a_missing_key_keeps_the_saved_uri_for_persist() {
    let saved_uri = "file:///l/last.jpg".to_owned();
    let style = StylePersisted {
        key_uri: Some(saved_uri.clone()),
        ..StylePersisted::default()
    };
    let mut model = restored(
        PerStyle {
            light: style.clone(),
            dark: style,
        },
        PerStyle::default(),
    );
    let mut keys = user_keys();
    keys.picture_uri_dark = None;

    let effects = step(
        &mut model,
        at(0),
        started(Style::Light, keys, PerStyle::default()),
    );

    let status = effects.iter().find_map(|effect| match effect {
        Effect::Report(status) => Some(status),
        _ => None,
    });
    let status = status.expect("start reports status");
    assert_eq!(status.paused.dark, Some(Pause::NoKey));
    assert_eq!(status.paused.light, None);
    assert_eq!(
        model.persisted().styles.dark.key_uri.as_deref(),
        Some(saved_uri.as_str())
    );
}

/// A run that is killed before it saves anything else must not leave the
/// ended pause in the state file: the next run would take the user's key for
/// the daemon's and write over it.
#[test]
fn a_missing_key_saves_the_end_of_an_external_change_pause_at_start() {
    let paused = StylePersisted {
        paused: true,
        user_paused: true,
        ..StylePersisted::default()
    };
    let mut model = restored(
        PerStyle {
            light: StylePersisted::default(),
            dark: paused,
        },
        PerStyle::default(),
    );
    let mut keys = user_keys();
    keys.picture_uri_dark = None;

    let effects = step(
        &mut model,
        at(0),
        started(Style::Light, keys, PerStyle::default()),
    );

    assert!(
        effects.contains(&Effect::Persist),
        "the start saved nothing"
    );
    let dark = &model.persisted().styles.dark;
    assert!(!dark.paused, "the external-change pause outlived the key");
    assert!(dark.user_paused, "the user pause was dropped");
}

#[test]
fn a_pre_scan_probe_does_not_open_a_relative_or_lossy_id() {
    let probes = |current: &str| {
        let light = StylePersisted {
            current: Some(id(current)),
            ..StylePersisted::default()
        };
        let mut model = restored(
            PerStyle {
                light,
                dark: StylePersisted::default(),
            },
            PerStyle::default(),
        );
        let effects = step(
            &mut model,
            at(0),
            started(Style::Light, user_keys(), PerStyle::default()),
        );
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Probe { path, .. } => Some(path),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    assert!(probes("l/a.jpg").is_empty());
    assert!(probes("/l/\u{FFFD}.jpg").is_empty());
    assert_eq!(probes("/l/a.jpg"), vec![PathBuf::from("/l/a.jpg")]);
}

#[test]
fn a_restored_bag_is_not_cut_to_history_len() {
    let count = HISTORY_LEN + 5;
    let ids: Vec<ItemId> = (0..count).map(|n| id(&format!("/p/{n}"))).collect();
    let over = restored(
        PerStyle {
            light: StylePersisted {
                bag: ids.clone(),
                shown: Some(ids.clone()),
                ..StylePersisted::default()
            },
            dark: StylePersisted::default(),
        },
        PerStyle::default(),
    );
    let saved = over.persisted().styles.light;
    assert_eq!(saved.bag, ids);
    assert_eq!(saved.shown.as_deref(), Some(ids.as_slice()));

    let short: Vec<ItemId> = (0..HISTORY_LEN).map(|n| id(&format!("/q/{n}"))).collect();
    let exact = restored(
        PerStyle {
            light: StylePersisted {
                bag: short.clone(),
                shown: None,
                ..StylePersisted::default()
            },
            dark: StylePersisted::default(),
        },
        PerStyle::default(),
    );
    let saved = exact.persisted().styles.light;
    assert_eq!(saved.bag, short);
    assert_eq!(saved.shown, None);
}

#[test]
fn a_restored_bag_drops_ids_the_catalog_does_not_know() {
    let keep = id("/l/keep.jpg");
    let gone = id("/gone.jpg");
    let next = id("/l/next.jpg");
    let old = id("/l/old.jpg");
    let queued = id("https://img.test/queued.jpg");
    let model = restored(
        PerStyle {
            light: StylePersisted {
                current: Some(old.clone()),
                bag: vec![keep.clone(), gone.clone(), queued.clone(), next.clone()],
                shown: Some(vec![old.clone()]),
                ..StylePersisted::default()
            },
            dark: StylePersisted::default(),
        },
        PerStyle {
            light: Some(vec![
                version("/l/keep.jpg", 0),
                version("/l/next.jpg", 0),
                version("/l/old.jpg", 0),
            ]),
            dark: Some(Vec::new()),
        },
    );

    let saved = model.persisted().styles.light;
    assert_eq!(saved.bag, vec![keep, queued, next]);
    assert!(!saved.bag.contains(&gone));
    assert_eq!(saved.shown.as_deref(), Some([old].as_slice()));
}
