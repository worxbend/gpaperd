//! The reducer's state, and the part of it that survives a restart.

use std::collections::BTreeMap;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::{Deserialize, Serialize};

use url::Url;

use crate::catalog::{
    BadReason, Catalog, CatalogDelta, Check, FileId, FsChange, Item, ItemId, LocalFile, Version,
};
use crate::config::{ApplyMode, Config, EmptyStylePolicy, Order, PictureOptions, Source};
use crate::control::{
    BadItem, Direction, History, PartialMove, ReplyId, Request, STATUS_VERSION, StatusReport,
    StyleReport,
};
use crate::event::{BackgroundKeys, Pause, ScreenLock, Status};
use crate::key::{KeyState, Reassert};
use crate::ordering::{ShuffleBag, is_remote_id, next_alphabetical};
use crate::remote::Remotes;
use crate::schedule::StyleClock;
use crate::style::{PerStyle, Style};

/// Schema version of [`Persisted`]. The state file adapter rejects any other
/// version, so the model never sees one.
pub const STATE_VERSION: u32 = 1;

/// The content of `state.json`.
///
/// This is the seam between the model, which produces and restores it, and
/// the daemon's `state.rs`, which only serialises it. New fields must carry
/// `#[serde(default)]` so that older files still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persisted {
    /// Always [`STATE_VERSION`] when produced by [`Model::persisted`].
    pub version: u32,
    pub styles: PerStyle<StylePersisted>,
    /// The user's keys from before the daemon's first write, kept for
    /// `gpaperctl restore`. Set once and never overwritten.
    pub original_keys: Option<BackgroundKeys>,
}

/// One style's persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StylePersisted {
    /// The item in the slot.
    pub current: Option<ItemId>,
    /// The slot's inode as the daemon last saw it: after its last publish, or
    /// as found on disk at start. A different inode on disk at the next start
    /// means someone else replaced the slot. `None` also while a slot that
    /// was replaced waits for its refill.
    pub slot: Option<FileId>,
    /// Start of the running interval.
    pub last_change: Option<SystemTime>,
    /// On-screen time left, stored while the style is hidden.
    pub remaining: Option<Duration>,
    /// The rest of the shuffle round, next item first. Empty for
    /// alphabetical order, whose cursor is `current`.
    pub bag: Vec<ItemId>,
    /// The items that the shuffle round showed, oldest first. An image that
    /// a scan finds and that is in neither `bag` nor `shown` is new since the
    /// last run and joins the round. Empty for alphabetical order.
    ///
    /// `None` when that is unknown: in a state file from before M2, and
    /// until the first scan after the start from one. The round then showed
    /// every image of that scan that it does not hold, and none joins it.
    pub shown: Option<Vec<ItemId>>,
    /// The slot was written while its style was hidden, so the shell may
    /// hold a stale texture for it: republish it after the next switch to
    /// the style.
    pub dirty_while_hidden: bool,
    /// An external key change paused the style
    /// ([`Pause::ExternalChange`]).
    pub paused: bool,
    /// `gpaperctl pause` or `restore` paused the style ([`Pause::User`]).
    pub user_paused: bool,
    /// The images the style showed before `current`, newest last: what
    /// `gpaperctl previous` steps back through.
    pub history: History,
    /// The URI that the daemon last wrote to the style's key, `None` while
    /// the key is the user's. In uri mode it names an image, so the next run
    /// needs it to tell the daemon's value from someone else's.
    pub key_uri: Option<String>,
}

/// Everything [`Model::new`] needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInit {
    /// The config file. `None` when there is none: the daemon then runs on
    /// the default config and touches no slot and no key until
    /// [`Event::ConfigChanged`](crate::event::Event::ConfigChanged) brings
    /// one.
    pub config: Option<Config>,
    /// Each style's local files when a scan already ran, as the scanner
    /// returned them. Duplicates are allowed; the catalog removes them.
    /// `None` when the scan is still to come: the files then arrive with
    /// [`Event::Scanned`](crate::event::Event::Scanned), and until they do
    /// the style's catalog is unknown, not empty.
    pub local_files: PerStyle<Option<Vec<LocalFile>>>,
    /// The loaded state file, or `None` on a first run or after a corrupt or
    /// unsupported file.
    pub restored: Option<Persisted>,
    /// Seed of the model's RNG. The daemon picks it; tests fix it.
    pub seed: u64,
    /// The `file://` URI of each style's slot file: what the style's key
    /// holds in slots mode.
    pub slot_uris: PerStyle<String>,
    /// The directory of the remote cache. In uri mode a key that names a
    /// file under it shows a cached remote image.
    pub remote_cache_dir: PathBuf,
}

/// All state of the daemon's core. Only [`crate::step::step`] changes it.
#[derive(Debug, Clone)]
pub struct Model {
    config: Config,
    /// The daemon started without a config file, and none came since.
    awaits_config: bool,
    catalogs: PerStyle<Catalog>,
    /// Whether a scan said what the style's catalog holds. Until one does,
    /// the catalog is unknown, not empty: the state file's current item can
    /// still be published, and the style does not turn to the other style's
    /// images.
    is_scanned: PerStyle<bool>,
    /// A reload changed the style's sources and their scan is still to
    /// come. Its catalog may lack images the new sources hold, so the style
    /// does not turn to the other style's images meanwhile.
    awaits_rescan: PerStyle<bool>,
    pub(crate) styles: PerStyle<StyleState>,
    /// The style on screen. Light until `Started` says otherwise.
    pub(crate) visible: Style,
    pub(crate) original_keys: Option<BackgroundKeys>,
    slot_uris: PerStyle<String>,
    remote_cache_dir: PathBuf,
    rng: SmallRng,
    pub(crate) phase: Phase,
    /// The `picture-options` value that goes out with the next key write.
    /// `None` when the value stays as it is, and once it was sent.
    pub(crate) picture_options_owed: Option<PictureOptions>,
    pub(crate) timer: Timer,
    /// The last status sent with `Effect::Report`.
    pub(crate) reported: Option<Status>,
    /// The truncated files that get, or got, another probe, by item. An
    /// entry lasts until its file changes or passes a probe.
    pub(crate) probe_retries: BTreeMap<ItemId, ProbeRetry>,
    /// The remote cache and the fetches.
    pub(crate) remote: Remotes,
    /// The `Next` and `Previous` requests that wait for their rotations,
    /// oldest first.
    pub(crate) requests: Vec<Request>,
    /// What the last both-style request moved before its other style
    /// failed. The next request takes it.
    pub(crate) partial_move: Option<PartialMove>,
    /// The `Reload` requests whose `ConfigRead` is still to come.
    pub(crate) reloads: Vec<ReplyId>,
    /// The `Restore` requests whose `KeysRestored` is still to come.
    pub(crate) restores: Vec<ReplyId>,
    /// Whether the screen is locked, as last reported. Recorded whatever
    /// the config says; [`Model::is_held_by_lock`] reads the setting.
    pub(crate) screen_lock: ScreenLock,
    /// Which background keys an administrator locked, as last reported.
    pub(crate) locks: PerStyle<bool>,
    /// What the background keys hold, as last reported. `None` before
    /// `Started`.
    pub(crate) gnome_keys: Option<BackgroundKeys>,
}

/// The probes that one version of a truncated file was granted after its
/// first one.
#[derive(Debug, Clone)]
pub(crate) struct ProbeRetry {
    /// The version of the file that was truncated. Another version starts
    /// over.
    pub(crate) version: Version,
    /// How many probes were granted so far.
    pub(crate) granted: u32,
    /// When the item may be picked again. `None` once it may, and for good
    /// once the last probe that was granted found the file truncated too.
    pub(crate) due: Option<SystemTime>,
}

/// One style's runtime state.
#[derive(Debug, Clone, Default)]
pub(crate) struct StyleState {
    /// The item in the slot.
    pub(crate) current: Option<ItemId>,
    /// The slot file's inode. `None` while the file is missing or holds
    /// something the daemon did not put there and is about to replace: the
    /// slot is still to be filled. Not kept up to date in uri mode.
    pub(crate) slot: Option<FileId>,
    /// Uri mode: the `file://` URI of the image that passed its probe for
    /// this style, in this run. `None` until one did: the style is still to
    /// get its image.
    pub(crate) shown_uri: Option<String>,
    pub(crate) clock: StyleClock,
    bag: ShuffleBag,
    /// The rotation in flight.
    pub(crate) rotation: Option<Rotation>,
    pub(crate) key: KeyState,
    /// The slot was written while the style was hidden, so the shell may
    /// hold a stale texture for it.
    pub(crate) dirty_while_hidden: bool,
    /// When the dirty slot of the style on screen is published again. `None`
    /// for a hidden style and for a clean slot.
    pub(crate) refresh_due: Option<SystemTime>,
    /// An `Effect::Republish` of the slot is in flight.
    pub(crate) is_refreshing: bool,
    /// `max_pixels` went down: the image on screen is to be probed again
    /// once nothing is in flight for the style.
    pub(crate) reprobe_owed: bool,
    /// A user pause ([`Pause::User`]) holds, next to any pause in `key`.
    pub(crate) user_paused: bool,
    /// The images shown before `current`, newest last.
    pub(crate) history: History,
    /// [`StylePersisted::key_uri`] of the last run, until `Started` decides
    /// whether the key is still the daemon's.
    pub(crate) saved_key_uri: Option<String>,
}

/// Where the daemon is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Built, `Started` not seen yet.
    Created,
    /// `Started` was seen; `Ready` was not sent.
    Starting,
    Running,
    /// `Shutdown` was seen.
    Stopped,
}

/// What the model knows about the interpreter's wall-clock timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timer {
    /// Armed for this instant, or disarmed.
    Set(Option<SystemTime>),
    /// The wall clock was stepped: the deadline must be sent again, even
    /// when it is the same instant.
    Stale,
}

/// One rotation of a style: a candidate on its way into the slot.
#[derive(Debug, Clone)]
pub(crate) struct Rotation {
    pub(crate) candidate: Candidate,
    pub(crate) stage: Stage,
    /// Candidates this rotation gave up on, oldest first. None of them is
    /// tried again before the rotation ends.
    pub(crate) failed: Vec<ItemId>,
    /// The `max_pixels` the probe was sent with.
    pub(crate) max_pixels: u64,
}

/// The effect a rotation waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Probing,
    Publishing,
}

impl Rotation {
    /// Every candidate the rotation took, oldest first.
    pub(crate) fn into_taken(self) -> Vec<ItemId> {
        let mut taken = self.failed;
        taken.push(self.candidate.id);
        taken
    }
}

/// An item picked for a slot, with the file to read it from.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) id: ItemId,
    pub(crate) path: PathBuf,
    /// The version of the file that was picked. `None` for a local item
    /// that only the state file names, before its style's first scan.
    pub(crate) version: Option<Version>,
}

impl Candidate {
    /// `item` as the catalog holds it now.
    fn of(item: &Item) -> Candidate {
        Candidate {
            id: item.id.clone(),
            path: item.origin.path().to_owned(),
            version: Some(item.version()),
        }
    }

    /// Whether `fresh`, the same item as the catalog holds it now, is
    /// another version of the file than the one that was picked.
    pub(crate) fn is_outdated_by(&self, fresh: &Candidate) -> bool {
        self.version.is_some() && self.version != fresh.version
    }
}

impl StyleState {
    /// Whether a probe or a write of the slot is in flight. At most one is,
    /// so that no two of them meet in the slot file.
    pub(crate) fn is_busy(&self) -> bool {
        self.rotation.is_some() || self.is_refreshing
    }

    /// Every reason why the style does not rotate, the one that lasts
    /// longest first: a missing or locked key, a user pause, an external
    /// change.
    pub(crate) fn pauses(&self) -> Vec<Pause> {
        let (lasting, external) = match self.key.pause {
            Some(Pause::ExternalChange) => (None, Some(Pause::ExternalChange)),
            other => (other, None),
        };
        let user = self.user_paused.then_some(Pause::User);
        [lasting, user, external].into_iter().flatten().collect()
    }

    /// Whether the key is the user's until `Resume`: the style is paused by
    /// the user, and this run has not put the daemon's URI in its key, or
    /// handed the key back with `Restore`. No key write is owed then.
    pub(crate) fn is_handed_back(&self) -> bool {
        self.user_paused && self.key.written.is_none()
    }

    /// Whether a pause holds, of whatever kind.
    fn is_paused(&self) -> bool {
        self.key.pause.is_some() || self.user_paused
    }

    fn restore(saved: StylePersisted, order: Order) -> StyleState {
        // The state file is outside input. A current item without a start
        // time would never come due, so it counts as long overdue.
        let overdue = saved.current.as_ref().map(|_| SystemTime::UNIX_EPOCH);
        let bag = match order {
            Order::Random => match saved.shown {
                Some(shown) => ShuffleBag::restore(saved.bag, shown),
                None => ShuffleBag::restore_queue_only(saved.bag),
            },
            Order::Alphabetical => ShuffleBag::default(),
        };
        StyleState {
            current: saved.current,
            slot: saved.slot,
            clock: StyleClock {
                last_change: saved.last_change.or(overdue),
                remaining: saved.remaining,
            },
            bag,
            key: KeyState {
                pause: saved.paused.then_some(Pause::ExternalChange),
                ..KeyState::default()
            },
            dirty_while_hidden: saved.dirty_while_hidden,
            user_paused: saved.user_paused,
            history: saved.history,
            saved_key_uri: saved.key_uri,
            ..StyleState::default()
        }
    }

    fn persisted(&self) -> StylePersisted {
        StylePersisted {
            current: self.current.clone(),
            slot: self.slot,
            last_change: self.clock.last_change,
            remaining: self.clock.remaining,
            bag: self.bag.queue().cloned().collect(),
            shown: self
                .bag
                .knows_shown()
                .then(|| self.bag.shown().cloned().collect()),
            dirty_while_hidden: self.dirty_while_hidden,
            paused: self.key.is_paused_by_change(),
            user_paused: self.user_paused,
            history: self.history.clone(),
            // Before `Started` this run has not looked at the key yet.
            key_uri: self
                .key
                .landed_uri()
                .map(str::to_owned)
                .or(self.saved_key_uri.clone()),
        }
    }
}

impl Model {
    /// Builds the model. Emits nothing: the first effects come from
    /// `step(…, Event::Started { … })`.
    ///
    /// Restoring: `current`, `slot`, the clock and the shuffle round come
    /// from `init.restored`. A restored id that is no longer in the catalog
    /// is not an error: `current` is kept, because the slot still shows that
    /// image. A restored bag is not cut to a length. Local ids neither
    /// catalog knows are dropped when every catalog is known, before the
    /// next draw; known ids keep their order. `shown` is not shortened.
    /// An image that the restored round neither holds nor showed is
    /// new since the last run and joins the round at a random position. When
    /// the configured order is alphabetical the restored round is ignored.
    ///
    /// A style whose `init.local_files` is `None` starts with an unknown
    /// catalog. Unknown local bag ids are dropped once every style's catalog
    /// is known.
    pub fn new(init: ModelInit) -> Model {
        let ModelInit {
            config,
            mut local_files,
            restored,
            seed,
            slot_uris,
            remote_cache_dir,
        } = init;
        let awaits_config = config.is_none();
        let config = config.unwrap_or_default();
        let (mut saved_styles, original_keys) = match restored {
            Some(saved) => (saved.styles, saved.original_keys),
            None => (PerStyle::default(), None),
        };
        let styles = PerStyle::from_fn(|style| {
            let saved = mem::take(&mut saved_styles[style]);
            StyleState::restore(saved, config.styles[style].order)
        });
        let mut model = Model {
            config,
            awaits_config,
            catalogs: PerStyle::default(),
            is_scanned: PerStyle::default(),
            awaits_rescan: PerStyle::default(),
            styles,
            visible: Style::Light,
            original_keys,
            slot_uris,
            remote_cache_dir,
            rng: SmallRng::seed_from_u64(seed),
            phase: Phase::Created,
            picture_options_owed: None,
            timer: Timer::Set(None),
            reported: None,
            probe_retries: BTreeMap::new(),
            remote: Remotes::default(),
            requests: Vec::new(),
            partial_move: None,
            reloads: Vec::new(),
            restores: Vec::new(),
            screen_lock: ScreenLock::default(),
            locks: PerStyle::default(),
            gnome_keys: None,
        };
        for style in Style::ALL {
            if let Some(files) = local_files[style].take() {
                let delta = model.replace_catalog(style, files);
                model.join_rounds_after_scan(style, &delta);
            }
        }
        model
    }

    /// The state to write to disk. `version` is [`STATE_VERSION`].
    ///
    /// Round trip: a model built from `persisted()`, the same config and the
    /// same files continues the same round with the same clocks, whatever the
    /// seed. Only an image that the round neither holds nor showed joins it:
    /// one whose probe failed in the run that saved the state, for example.
    pub fn persisted(&self) -> Persisted {
        Persisted {
            version: STATE_VERSION,
            styles: PerStyle::from_fn(|style| self.styles[style].persisted()),
            original_keys: self.original_keys.clone(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Makes `config` the running config and returns the one it replaces.
    pub(crate) fn replace_config(&mut self, config: Config) -> Config {
        self.awaits_config = false;
        mem::replace(&mut self.config, config)
    }

    /// Whether the daemon started without a config file and none came
    /// since. Until one does, no slot and no key is touched.
    pub(crate) fn awaits_config(&self) -> bool {
        self.awaits_config
    }

    /// The URI that `style`'s key must hold: its slot's in slots mode, its
    /// image's own in uri mode. `None` while the style has nothing to show:
    /// no key ever names a missing file.
    pub(crate) fn wanted_uri(&self, style: Style) -> Option<&str> {
        let state = &self.styles[style];
        match self.config.general.apply_mode {
            ApplyMode::Slots => state.slot.map(|_| self.slot_uris[style].as_str()),
            ApplyMode::Uri => state.shown_uri.as_deref(),
        }
    }

    /// Whether `style` has something for its key to name: a slot file, or
    /// in uri mode an image that passed its probe in this run. A style
    /// without is still to be filled.
    pub(crate) fn has_wallpaper(&self, style: Style) -> bool {
        self.wanted_uri(style).is_some()
    }

    /// Uri mode: whether `style`'s key names a file that left the catalogs
    /// or turned out bad. The shell then has nothing to show.
    ///
    /// Both catalogs count, whichever the style picks from now: an image
    /// that the style borrowed stays when the style gets images of its own.
    pub(crate) fn names_a_gone_file(&self, style: Style) -> bool {
        let state = &self.styles[style];
        let (Some(_), Some(current)) = (&state.shown_uri, &state.current) else {
            return false;
        };
        // Before the scan the state file is all there is.
        let is_listed = |owner: Style| self.catalogs[owner].is_candidate(current);
        self.is_scanned[style] && !Style::ALL.into_iter().any(is_listed)
    }

    /// Records that whatever `style`'s key would name is to be replaced.
    pub(crate) fn clear_wallpaper(&mut self, style: Style) {
        let state = &mut self.styles[style];
        state.slot = None;
        state.shown_uri = None;
    }

    /// Whether `style`'s clock runs, and with it whether its slot may
    /// change: the style is on screen, hidden styles rotate too, or its slot
    /// is still to be filled. Filling a slot is a repair, which does not wait
    /// for the style to be shown.
    pub(crate) fn is_running(&self, style: Style) -> bool {
        style == self.visible || self.config.general.rotate_hidden || !self.has_wallpaper(style)
    }

    /// Whether a probe or a slot write is in flight for any style.
    pub(crate) fn is_busy(&self) -> bool {
        Style::ALL.iter().any(|&style| self.styles[style].is_busy())
    }

    /// The URI to write to `style`'s key now, when one is owed: the key
    /// does not hold what it must, and no pause or wait holds the write back.
    pub(crate) fn owed_uri(&self, style: Style) -> Option<&str> {
        let wanted = self.wanted_uri(style)?;
        let state = &self.styles[style];
        (!state.is_handed_back() && state.key.owes(wanted)).then_some(wanted)
    }

    /// Whether any key holds the URI that this run put there, and is not
    /// paused: a paused key is not the daemon's to write.
    pub(crate) fn holds_a_key(&self) -> bool {
        Style::ALL.iter().any(|&style| {
            let key = &self.styles[style].key;
            key.written.is_some() && key.pause.is_none()
        })
    }

    /// Whether some key in `keys` names its style's slot file or a file in
    /// the remote cache. Such keys were written by an earlier run, so they
    /// are not the user's.
    pub(crate) fn names_a_daemon_file(&self, keys: &BackgroundKeys) -> bool {
        Style::ALL.into_iter().any(|style| {
            keys.uri(style)
                .is_some_and(|uri| uri == self.slot_uris[style] || self.is_in_remote_cache(uri))
        })
    }

    /// Whether `uri` is a `file://` URI of a path under the remote cache.
    fn is_in_remote_cache(&self, uri: &str) -> bool {
        Url::parse(uri)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .is_some_and(|path| path.starts_with(&self.remote_cache_dir))
    }

    /// The value that `style`'s key may be locked on while the style still
    /// rotates: its slot URI, because a rotation only replaces the slot
    /// file. `None` in uri mode, where every rotation writes the key.
    fn lockable_uri(&self, style: Style) -> Option<String> {
        match self.config.general.apply_mode {
            ApplyMode::Slots => Some(self.slot_uris[style].clone()),
            ApplyMode::Uri => None,
        }
    }

    /// Start-up: records what this run can do with `style`'s key. `value` is
    /// what the key holds, `None` when the schema has no such key. The locks
    /// are in [`Model::locks`].
    ///
    /// A locked key is fine when it names the slot already: a rotation only
    /// replaces the slot file. Any other locked key stops the style until
    /// [`Model::follow_lock`] lets it go on; a saved external-change pause
    /// waits under that lock. A missing key stops it for the run.
    pub(crate) fn adopt_key(&mut self, style: Style, value: Option<&str>) {
        let slot_uri = self.lockable_uri(style);
        let is_locked = self.locks[style];
        let state = &mut self.styles[style];
        let Some(value) = value else {
            // A later uri-mode start still has to recognize the last daemon URI.
            state.key.pause = Some(Pause::NoKey);
            return;
        };
        let saved_uri = state.saved_key_uri.take();
        // What the daemon's own value of the key is: the slot URI, or in uri
        // mode the image URI that the last run wrote.
        let daemons_uri = slot_uri.clone().or(saved_uri);
        let user_paused = state.user_paused;
        let key = &mut state.key;
        // Each is as if this run had written the daemon's value: a locked
        // key that names the slot, the key of a style that the last run left
        // paused by an external change, and a user-paused key that holds the
        // daemon's value. So a later change to the key, or its return, is
        // noticed. A user-paused key that holds anything else is the user's,
        // as after `Restore`.
        let holds_daemons_uri = daemons_uri.as_deref() == Some(value);
        let was_daemons = key.pause.is_some() || (user_paused && holds_daemons_uri);
        let is_locked_out = is_locked && slot_uri.as_deref() != Some(value);
        if was_daemons || (is_locked && !is_locked_out) {
            key.written = daemons_uri;
        }
        // A saved external-change pause waits under the lock, so a key that
        // still holds another value once the lock is lifted stays paused.
        if is_locked_out {
            key.lock_out();
        }
    }

    /// Follows what `style`'s key holds now and whether it is locked: an
    /// administrator added or lifted the lock, changed the locked value, or
    /// a reload changed which value the style can live with. Returns whether
    /// the style's [`Pause::LockedKey`] began or ended.
    ///
    /// A key locked on anything but its slot URI stops the style, in place
    /// of an external-change pause. A key that is free again, or locked on
    /// the slot URI by now, lets it go on: a lock on the slot URI counts as
    /// the daemon's own write, as at start.
    pub(crate) fn follow_lock(&mut self, style: Style, value: &str) -> bool {
        let is_locked = self.locks[style];
        let lockable = self.lockable_uri(style);
        let is_locked_out = is_locked && lockable.as_deref() != Some(value);
        let key = &mut self.styles[style].key;
        match key.pause {
            Some(Pause::NoKey) => false,
            Some(Pause::LockedKey) if !is_locked_out => {
                key.unlock();
                if is_locked {
                    key.written = lockable;
                }
                true
            }
            Some(Pause::LockedKey) => false,
            _ if is_locked_out => {
                key.lock_out();
                true
            }
            _ => false,
        }
    }

    /// The tick that `reassert-next-tick` waited for is here: every key
    /// that waited is written back.
    pub(crate) fn reassert_at_tick(&mut self) {
        for style in Style::ALL {
            self.styles[style].key.end_tick_wait();
        }
    }

    /// When `style`'s dirty slot is published again. `None` when nothing is
    /// planned, and while it cannot be done: the slot file is missing or
    /// busy, or the style is paused and shows something else anyway.
    pub(crate) fn refresh_deadline(&self, style: Style) -> Option<SystemTime> {
        let state = &self.styles[style];
        let is_slot_shown = self.config.general.apply_mode == ApplyMode::Slots
            && state.slot.is_some()
            && state.key.pause.is_none()
            && !self.awaits_config;
        state
            .refresh_due
            .filter(|_| is_slot_shown && !state.is_busy())
    }

    /// Whether `style` must rotate now.
    pub(crate) fn is_due(&self, style: Style, now: SystemTime) -> bool {
        self.awaits_deadline(style) && self.deadline(style).is_some_and(|due| due <= now)
    }

    /// The instant to arm the timer for, whichever comes first: the next
    /// rotation, the next probe of a truncated file, the refresh of a dirty
    /// slot, the write-back of a key under `reassert-now`, the next fetch or
    /// the next garbage collection.
    pub(crate) fn next_deadline(&self) -> Option<SystemTime> {
        let next_fetch = self.remote.next_fetch(self.config.general.remote_refresh);
        let next_garbage = self.remote.garbage_due().filter(|_| !self.awaits_config);
        let next_probe_retry = self.probe_retries.values().filter_map(|retry| retry.due);
        let short_waits = Style::ALL.into_iter().flat_map(|style| {
            let reassert = match self.styles[style].key.reassert {
                Some(Reassert::At(due) | Reassert::Retry(due)) => Some(due),
                Some(Reassert::Now | Reassert::RetryNow | Reassert::AtNextTick) | None => None,
            };
            [self.refresh_deadline(style), reassert]
        });
        self.next_rotation()
            .into_iter()
            .chain(next_probe_retry)
            .chain(short_waits.flatten())
            .chain(next_fetch)
            .chain(next_garbage)
            .min()
    }

    /// When the next rotation is due: the earliest deadline among the styles
    /// that wait for one.
    fn next_rotation(&self) -> Option<SystemTime> {
        Style::ALL
            .into_iter()
            .filter(|&style| self.awaits_deadline(style))
            .filter_map(|style| self.deadline(style))
            .min()
    }

    pub(crate) fn status(&self) -> Status {
        Status {
            visible: self.visible,
            current: PerStyle::from_fn(|style| self.styles[style].current.clone()),
            next_change: self.next_rotation(),
            paused: PerStyle::from_fn(|style| self.pauses(style).first().copied()),
        }
    }

    /// Every reason why `style` does not rotate, the one that lasts longest
    /// first. A locked screen comes last.
    fn pauses(&self, style: Style) -> Vec<Pause> {
        let mut pauses = self.styles[style].pauses();
        if self.is_held_by_lock() {
            pauses.push(Pause::ScreenLocked);
        }
        pauses
    }

    /// Whether a locked screen pauses every style: it is locked and
    /// `pause_when_locked` is on.
    pub(crate) fn is_held_by_lock(&self) -> bool {
        self.config.general.pause_when_locked && self.screen_lock == ScreenLock::Locked
    }

    /// The answer to `Status`.
    pub(crate) fn status_report(&self) -> StatusReport {
        let since_epoch = |at: SystemTime| at.duration_since(SystemTime::UNIX_EPOCH).ok();
        StatusReport {
            version: STATUS_VERSION,
            visible: self.visible,
            next_change: self
                .next_rotation()
                .and_then(since_epoch)
                .map(|since| since.as_secs()),
            styles: PerStyle::from_fn(|style| self.style_report(style)),
        }
    }

    fn style_report(&self, style: Style) -> StyleReport {
        let catalog = &self.catalogs[style];
        let bad = catalog.items().filter_map(|item| match &item.check {
            Check::Bad(reason) => Some(BadItem {
                id: item.id.clone(),
                reason: reason.to_string(),
            }),
            Check::Unchecked | Check::Usable => None,
        });
        StyleReport {
            current: self.styles[style].current.clone(),
            paused: self.pauses(style),
            items: catalog.len(),
            usable: catalog.candidates().count(),
            bad: bad.collect(),
        }
    }

    /// Whether a `Next` or `Previous` runs a rotation of `style`. That
    /// rotation may write a hidden slot.
    pub(crate) fn is_requested(&self, style: Style) -> bool {
        self.requests.iter().any(|request| request.runs_for(style))
    }

    /// Whether a `Previous` runs a rotation of `style`: the image it shows
    /// comes from the history and does not go back into it.
    pub(crate) fn steps_back(&self, style: Style) -> bool {
        self.requests
            .iter()
            .any(|request| request.direction == Direction::Back && request.runs_for(style))
    }

    /// Takes the newest image of `style`'s history that it can show now,
    /// dropping every newer one that it cannot. `None` when none is left.
    pub(crate) fn step_back(&mut self, style: Style) -> Option<Candidate> {
        while let Some(earlier) = self.styles[style].history.pop() {
            let is_on_screen = self.styles[style].current.as_ref() == Some(&earlier);
            if let Some(candidate) = self.candidate(style, &earlier).filter(|_| !is_on_screen) {
                return Some(candidate);
            }
        }
        None
    }

    /// The item in `style`'s slot, when it can still be published.
    pub(crate) fn current_candidate(&self, style: Style) -> Option<Candidate> {
        self.candidate(style, self.styles[style].current.as_ref()?)
    }

    /// Whether `style` has a wallpaper and it is `item`.
    pub(crate) fn shows(&self, style: Style, item: &ItemId) -> bool {
        self.has_wallpaper(style) && self.styles[style].current.as_ref() == Some(item)
    }

    /// Records that `style`'s slot shows another image now. In random order
    /// that image had its turn in the round.
    pub(crate) fn set_current(&mut self, style: Style, item: ItemId) {
        let state = &mut self.styles[style];
        if self.config.styles[style].order == Order::Random {
            state.bag.mark_shown(&item);
        }
        state.current = Some(item);
    }

    /// Makes `style`'s catalog hold exactly the files of a scan. From now on
    /// the catalog is known.
    pub(crate) fn replace_catalog(&mut self, style: Style, files: Vec<LocalFile>) -> CatalogDelta {
        let catalogs_were_known = self.catalogs_are_known();
        self.is_scanned[style] = true;
        self.awaits_rescan[style] = false;
        let delta = self.catalogs[style].replace(files);
        if !catalogs_were_known && self.catalogs_are_known() {
            for each in Style::ALL {
                self.drop_unknown_bag(each);
            }
        }
        delta
    }

    fn catalogs_are_known(&self) -> bool {
        Style::ALL.into_iter().all(|style| self.is_scanned[style])
    }

    /// Drops local bag ids neither catalog knows. Known ids stay, in order.
    /// An http(s) URL stays: its blob may still be on its way into the catalog.
    fn drop_unknown_bag(&mut self, style: Style) {
        if self.config.styles[style].order != Order::Random {
            return;
        }
        let queued: Vec<ItemId> = self.styles[style].bag.queue().cloned().collect();
        let shown: Vec<ItemId> = self.styles[style].bag.shown().cloned().collect();
        let knows_shown = self.styles[style].bag.knows_shown();
        let queue: Vec<ItemId> = queued
            .into_iter()
            .filter(|id| self.bag_id_is_known(id))
            .collect();
        self.styles[style].bag = if knows_shown {
            ShuffleBag::restore(queue, shown)
        } else {
            ShuffleBag::restore_queue_only(queue)
        };
    }

    fn bag_id_is_known(&self, id: &ItemId) -> bool {
        is_remote_id(id)
            || Style::ALL
                .into_iter()
                .any(|style| self.catalogs[style].get(id).is_some())
    }

    /// Records that `style`'s sources changed and their scan is to come.
    pub(crate) fn await_rescan(&mut self, style: Style) {
        self.awaits_rescan[style] = true;
    }

    /// Applies what the watcher saw to `style`'s catalog.
    pub(crate) fn apply_changes(&mut self, style: Style, changes: Vec<FsChange>) -> CatalogDelta {
        self.catalogs[style].apply(changes)
    }

    /// Gives the items that are new in `changed`'s catalog, or have new
    /// content, a place in the shuffle round of every style that picks from
    /// that catalog. The item on screen has its place already.
    pub(crate) fn join_rounds(&mut self, changed: Style, delta: &CatalogDelta) {
        self.join_rounds_by(changed, delta, ShuffleBag::insert);
    }

    /// [`Model::join_rounds`] for the delta of `changed`'s scan, which also
    /// tells a round from before M2 what it showed.
    pub(crate) fn join_rounds_after_scan(&mut self, changed: Style, delta: &CatalogDelta) {
        self.join_rounds_by(changed, delta, ShuffleBag::scanned);
        self.learn_shown_from_pool(changed);
    }

    /// Tells `style`'s round from before M2 what it showed once its own scan
    /// is in, when that scan did not reach the round: `style` picks from no
    /// catalog, or from the other style's. What the round does not hold of
    /// the catalog it picks from now, it showed.
    fn learn_shown_from_pool(&mut self, style: Style) {
        let is_shuffled = self.config.styles[style].order == Order::Random;
        if !is_shuffled || self.styles[style].bag.knows_shown() {
            return;
        }
        // The other style's first scan may still give the round a catalog to
        // pick from: that scan tells it what it showed.
        let may_borrow_later = self.config.general.empty_style == EmptyStylePolicy::UseOtherStyle
            && !self.is_scanned[style.other()];
        if self.pool(style).is_none() && may_borrow_later {
            return;
        }
        let on_screen = self.styles[style].current.as_ref();
        let in_pool: Vec<ItemId> = self
            .pool(style)
            .into_iter()
            .flat_map(|pool| self.catalogs[pool].candidates())
            .map(|item| item.id.clone())
            .filter(|id| Some(id) != on_screen)
            .collect();
        self.styles[style].bag.scanned(in_pool, &mut self.rng);
    }

    fn join_rounds_by(
        &mut self,
        changed: Style,
        delta: &CatalogDelta,
        join: fn(&mut ShuffleBag, Vec<ItemId>, &mut SmallRng),
    ) {
        for style in Style::ALL {
            let is_shuffled = self.config.styles[style].order == Order::Random;
            if !is_shuffled || self.pool(style) != Some(changed) {
                continue;
            }
            let state = &mut self.styles[style];
            let on_screen = state.current.as_ref();
            let newcomers = delta
                .added
                .iter()
                .chain(&delta.modified)
                .filter(|&id| Some(id) != on_screen)
                .cloned()
                .collect();
            join(&mut state.bag, newcomers, &mut self.rng);
        }
    }

    /// Takes the next item for `style`'s slot, by the style's order. `None`
    /// when its pool holds nothing but the item on screen and the items in
    /// `failed`.
    ///
    /// `failed` are the candidates the rotation in flight gave up on, oldest
    /// first. The alphabetical cursor continues after the last of them.
    pub(crate) fn pick(&mut self, style: Style, failed: &[ItemId]) -> Option<Candidate> {
        let pool = self.pool(style)?;
        let catalog = &self.catalogs[pool];
        let state = &mut self.styles[style];
        let on_screen = state.current.as_ref();
        let next = match self.config.styles[style].order {
            Order::Random => state.bag.draw(catalog, on_screen, failed, &mut self.rng),
            // The walk came full circle when it meets one of these again.
            Order::Alphabetical => next_alphabetical(catalog, failed.last().or(on_screen))
                .filter(|next| Some(next) != on_screen && !failed.contains(next)),
        }?;
        self.candidate(style, &next)
    }

    /// Gives candidates that a rotation of `style` took and did not show
    /// back to the shuffle round, oldest first, so that the next rotation
    /// draws them again. A candidate whose probe failed stays out.
    ///
    /// Alphabetical order needs none of this: its cursor is the item on
    /// screen, which did not move.
    pub(crate) fn return_to_round(&mut self, style: Style, unshown: Vec<ItemId>) {
        if self.config.styles[style].order != Order::Random {
            return;
        }
        let Some(pool) = self.pool(style) else {
            return;
        };
        let catalog = &self.catalogs[pool];
        let bag = &mut self.styles[style].bag;
        let still_usable = unshown.into_iter().filter(|id| catalog.is_candidate(id));
        for id in still_usable.rev() {
            bag.put_back(id);
        }
    }

    /// Starts `style`'s order over, because it changed: a round of the old
    /// order means nothing in the new one. The alphabetical cursor is the
    /// item on screen, which stays.
    pub(crate) fn reset_round(&mut self, style: Style) {
        self.styles[style].bag = ShuffleBag::default();
    }

    /// Drops every "too large" verdict, because `max_pixels` went up.
    /// Returns the items that may be picked again, per catalog.
    pub(crate) fn forget_size_verdicts(&mut self) -> PerStyle<Vec<ItemId>> {
        PerStyle::from_fn(|style| {
            let catalog = &mut self.catalogs[style];
            let is_too_large =
                |item: &&Item| matches!(item.check, Check::Bad(BadReason::TooLarge { .. }));
            let too_large: Vec<ItemId> = catalog
                .items()
                .filter(is_too_large)
                .map(|item| item.id.clone())
                .collect();
            for id in &too_large {
                catalog.set_check(id, Check::Unchecked);
            }
            too_large
        })
    }

    /// Ends every rotation in flight without a publish. Their candidates
    /// stay in the round.
    pub(crate) fn abandon_rotations(&mut self) {
        for style in Style::ALL {
            if let Some(rotation) = self.styles[style].rotation.take() {
                self.return_to_round(style, rotation.into_taken());
            }
        }
    }

    /// Takes `style`'s rotation when `item` at `stage` is the answer it
    /// waits for. Any other result is stale and takes nothing.
    pub(crate) fn take_rotation(
        &mut self,
        style: Style,
        item: &ItemId,
        stage: Stage,
    ) -> Option<Rotation> {
        self.styles[style]
            .rotation
            .take_if(|rotation| rotation.stage == stage && rotation.candidate.id == *item)
    }

    /// Records the verdict of a probe of `probed`. The same file has the
    /// same id in both styles, so the verdict holds for both catalogs.
    ///
    /// The verdict is about one version of the file, so a catalog that holds
    /// another version by now is left alone. Returns whether any catalog
    /// took the verdict.
    pub(crate) fn record_verdict(
        &mut self,
        probed: &Candidate,
        verdict: Result<(), BadReason>,
    ) -> bool {
        let Some(version) = &probed.version else {
            return false;
        };
        let check = match verdict {
            Ok(()) => Check::Usable,
            Err(reason) => Check::Bad(reason),
        };
        let mut is_recorded = false;
        for style in Style::ALL {
            let catalog = &mut self.catalogs[style];
            if holds_version(catalog, &probed.id, version) {
                catalog.set_check(&probed.id, check.clone());
                is_recorded = true;
            }
        }
        is_recorded
    }

    /// Ends the wait of every truncated file whose next probe is due: its
    /// item may be picked again. Returns those items, per catalog.
    pub(crate) fn release_probe_retries(&mut self, now: SystemTime) -> PerStyle<Vec<ItemId>> {
        let catalogs = &mut self.catalogs;
        // A file that changed or left starts over when it is truncated again.
        self.probe_retries.retain(|id, retry| {
            let holds_it = |style: Style| holds_version(&catalogs[style], id, &retry.version);
            Style::ALL.into_iter().any(holds_it)
        });
        let mut released = PerStyle::<Vec<ItemId>>::default();
        for (id, retry) in &mut self.probe_retries {
            if retry.due.is_none_or(|due| now < due) {
                continue;
            }
            retry.due = None;
            for style in Style::ALL {
                if holds_version(&catalogs[style], id, &retry.version) {
                    catalogs[style].set_check(id, Check::Unchecked);
                    released[style].push(id.clone());
                }
            }
        }
        released
    }

    /// Starts a fresh interval for `style` at `now`. The clock of a style
    /// that does not run stays stopped, with the whole interval left.
    pub(crate) fn restart_clock(&mut self, style: Style, now: SystemTime) {
        self.styles[style].clock.record_change(now);
        self.follow_visibility(style, now);
    }

    /// Runs `style`'s clock while [`Model::is_running`] says so and stops it
    /// while it does not.
    pub(crate) fn follow_visibility(&mut self, style: Style, now: SystemTime) {
        let interval = self.interval(style);
        let is_running = self.is_running(style);
        let clock = &mut self.styles[style].clock;
        if is_running {
            clock.show(now, interval);
        } else {
            clock.hide(now, interval);
        }
    }

    fn interval(&self, style: Style) -> Duration {
        self.config.styles[style].interval
    }

    fn deadline(&self, style: Style) -> Option<SystemTime> {
        self.styles[style].clock.deadline(self.interval(style))
    }

    /// Whether `style` waits for its deadline: no pause holds, a locked
    /// screen included, its clock runs, nothing is in flight for it, and
    /// there is another image to rotate to. A style whose key waits for a tick has one even with no
    /// image to rotate to.
    fn awaits_deadline(&self, style: Style) -> bool {
        let state = &self.styles[style];
        let has_a_tick =
            self.has_other_candidate(style) || state.key.reassert == Some(Reassert::AtNextTick);
        let is_paused = state.is_paused() || self.is_held_by_lock();
        !is_paused && self.is_running(style) && !state.is_busy() && has_a_tick
    }

    pub(crate) fn has_other_candidate(&self, style: Style) -> bool {
        let Some(pool) = self.pool(style) else {
            return false;
        };
        let on_screen = self.styles[style].current.as_ref();
        self.catalogs[pool]
            .candidates()
            .any(|item| Some(&item.id) != on_screen)
    }

    /// The style whose catalog `style` picks from: its own, or the other
    /// style's when its own has no usable image and `empty_style` allows it.
    /// `None` when there is nothing to pick from.
    pub(crate) fn pool(&self, style: Style) -> Option<Style> {
        let has_candidates = |owner: Style| self.catalogs[owner].candidates().next().is_some();
        if has_candidates(style) {
            return Some(style);
        }
        // An unknown catalog may still turn out to hold images, and so may
        // new sources still to be scanned or a URL still to bring its first
        // one.
        let may_borrow = self.is_scanned[style]
            && !self.awaits_rescan[style]
            && !self.awaits_first_blob(style)
            && self.config.general.empty_style == EmptyStylePolicy::UseOtherStyle;
        (may_borrow && has_candidates(style.other())).then_some(style.other())
    }

    /// `id` as `style` can publish it now: the item of the catalog it picks
    /// from, unless that item is bad. `None` when there is no such item.
    ///
    /// Before the style's first scan the state file is all there is, so a
    /// local id itself names the file. A remote item is known from the
    /// cache, scan or not.
    ///
    /// shortcut: an id is a lossy copy of a path that is not UTF-8, so such
    /// a file cannot be published before the scan; persist the path when
    /// that matters.
    pub(crate) fn candidate(&self, style: Style, id: &ItemId) -> Option<Candidate> {
        if !self.is_scanned[style] {
            return self.saved_candidate(style, id);
        }
        let item = self.catalogs[self.pool(style)?].get(id)?;
        item.is_candidate().then(|| Candidate::of(item))
    }

    /// `id` as `style` can publish it before its first scan. Its catalog
    /// holds only remote items then, and a URL that it does not hold has no
    /// blob to read.
    fn saved_candidate(&self, style: Style, id: &ItemId) -> Option<Candidate> {
        if let Some(item) = self.catalogs[style].get(id) {
            return item.is_candidate().then(|| Candidate::of(item));
        }
        let text = id.as_str();
        if !is_pre_scan_path(text) {
            return None;
        }
        Some(Candidate {
            id: id.clone(),
            path: PathBuf::from(text),
            version: None,
        })
    }

    /// Every URL that the running config lists, in config order, light
    /// first, each once.
    pub(crate) fn listed_urls(&self) -> Vec<Url> {
        let mut listed: Vec<Url> = Vec::new();
        for url in Style::ALL.into_iter().flat_map(|style| self.urls_of(style)) {
            if !listed.contains(url) {
                listed.push(url.clone());
            }
        }
        listed
    }

    /// The URLs among `style`'s sources.
    fn urls_of(&self, style: Style) -> impl Iterator<Item = &Url> {
        let sources = self.config.styles[style].sources.iter();
        sources.filter_map(|source| match source {
            Source::Url(url) => Some(url),
            Source::Path(_) | Source::Dir { .. } | Source::File(_) => None,
        })
    }

    /// Whether one of `style`'s URLs may still bring a first image. Until
    /// none may, a style without images of its own keeps its slot.
    fn awaits_first_blob(&self, style: Style) -> bool {
        self.urls_of(style).any(|url| self.remote.awaits_blob(url))
    }

    /// Makes each catalog's remote items the cached blobs of the URLs that
    /// its style lists. Returns what that changed, per catalog.
    pub(crate) fn sync_remote_items(&mut self) -> PerStyle<CatalogDelta> {
        PerStyle::from_fn(|style| {
            let cached: Vec<_> = self
                .urls_of(style)
                .filter_map(|url| self.remote.cached(url))
                .collect();
            self.catalogs[style].set_remote(cached)
        })
    }
}

/// Whether `id` may be opened before the first scan. A relative id would be
/// opened from the process cwd, and a lossy id is not the saved path. A URL
/// has no file until the cache says so.
fn is_pre_scan_path(id: &str) -> bool {
    Url::parse(id).is_err() && Path::new(id).is_absolute() && !id.contains('\u{FFFD}')
}

/// Whether `catalog` holds `id` in `version`.
fn holds_version(catalog: &Catalog, id: &ItemId, version: &Version) -> bool {
    catalog
        .get(id)
        .is_some_and(|item| item.version() == *version)
}
