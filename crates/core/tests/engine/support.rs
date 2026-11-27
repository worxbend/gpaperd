//! Fixtures shared by the engine tests.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::{
    BadReason, Catalog, FileId, FileStamp, FsChange, ItemId, LocalFile, RemoteBlob,
};
use gpaperd_core::config::{Config, ConfigError, Order, PictureOptions, Source};
use gpaperd_core::control::{Command, Reply, ReplyId};
use gpaperd_core::event::{
    BackgroundKeys, Effect, Event, FetchOutcome, Fired, GnomeEvent, GnomeSnapshot, Method,
    SlotWrite, Status,
};
use gpaperd_core::model::{Model, ModelInit, Persisted, STATE_VERSION, StylePersisted};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};
use url::Url;

/// Where the test timeline starts, in seconds after the epoch (2027).
const TIMELINE_START_SECS: u64 = 1_800_000_000;

/// Steps that [`World::run_all`] takes before it gives up. One event leads to
/// a few dozen at most: both styles' rotations with all their candidates, and
/// the report of a key write.
const MAX_STEPS_PER_RUN: usize = 200;

/// The instant `secs` seconds into the test timeline.
pub fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(TIMELINE_START_SECS + secs)
}

pub fn id(path: &str) -> ItemId {
    ItemId::new(path)
}

/// A scanned file on an inode of its own.
pub fn file(path: &str) -> LocalFile {
    linked(path, inode_of(path))
}

/// A scanned file on a chosen inode. Two paths on one inode are hardlinks.
pub fn linked(path: &str, ino: u64) -> LocalFile {
    LocalFile {
        path: PathBuf::from(path),
        stamp: FileStamp {
            id: FileId { dev: 1, ino },
            size: 1,
            mtime: SystemTime::UNIX_EPOCH,
        },
    }
}

/// FNV-1a of the path, with the top bit set so that it never meets the small
/// inode numbers tests pick by hand.
fn inode_of(path: &str) -> u64 {
    let hash = path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    hash | 1 << 63
}

/// `path` after `saves` edits in place: the same inode with a later mtime.
/// `version(path, 0)` is `file(path)`.
pub fn version(path: &str, saves: u64) -> LocalFile {
    let mut saved = file(path);
    saved.stamp.mtime += Duration::from_secs(saves);
    saved
}

pub fn files(paths: &[&str]) -> Vec<LocalFile> {
    paths.iter().map(|path| file(path)).collect()
}

pub fn url(text: &str) -> Url {
    Url::parse(text).expect("test URL parses")
}

/// The directory of the remote cache: each blob lies under it.
pub const REMOTE_CACHE_DIR: &str = "/cache";

/// Version `n` of `url`'s image in the remote cache. Each version has a blob
/// of its own.
pub fn blob(text: &str, n: u32) -> RemoteBlob {
    let url = url(text);
    let blob = PathBuf::from(format!("{REMOTE_CACHE_DIR}{}/v{n}.jpg", url.path()));
    RemoteBlob { url, blob }
}

/// The cache's answer at start: these blobs are on disk.
pub fn cache_loaded(blobs: &[RemoteBlob]) -> Event {
    Event::CacheLoaded(blobs.to_vec())
}

/// The fetcher's answer for `text`.
pub fn fetched(text: &str, outcome: FetchOutcome) -> Event {
    Event::Fetched {
        url: url(text),
        outcome,
    }
}

/// The fetcher stored `remote` as the new version of its URL.
pub fn stored(remote: &RemoteBlob) -> Event {
    let outcome = FetchOutcome::Stored {
        blob: remote.blob.clone(),
    };
    Event::Fetched {
        url: remote.url.clone(),
        outcome,
    }
}

/// A config whose styles list these URLs and nothing else, in
/// alphabetical order.
pub fn remote_config(light: &[&str], dark: &[&str]) -> Config {
    let sources = |urls: &[&str]| urls.iter().map(|text| Source::Url(url(text))).collect();
    let mut config = alphabetical();
    config.styles.light.sources = sources(light);
    config.styles.dark.sources = sources(dark);
    config
}

/// The URL of every `Fetch` effect, in order.
pub fn fetches(trace: &[Effect]) -> Vec<&str> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Fetch(url) => Some(url.as_str()),
            _ => None,
        })
        .collect()
}

/// The URL of every `DropRemote` effect, in order.
pub fn dropped(trace: &[Effect]) -> Vec<&str> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::DropRemote(url) => Some(url.as_str()),
            _ => None,
        })
        .collect()
}

/// The `keep` list of every `CollectGarbage` effect, in order.
pub fn collected(trace: &[Effect]) -> Vec<Vec<&str>> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::CollectGarbage { keep } => Some(keep.iter().map(Url::as_str).collect()),
            _ => None,
        })
        .collect()
}

/// The path of every `Probe` effect, in order.
pub fn probed_paths(trace: &[Effect]) -> Vec<PathBuf> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Probe { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

pub fn catalog(paths: &[&str]) -> Catalog {
    Catalog::from_local_files(files(paths))
}

pub fn found(path: &str) -> FsChange {
    FsChange::Found(file(path))
}

pub fn removed(path: &str) -> FsChange {
    FsChange::Removed(path.into())
}

/// The watcher's report of `changes` to `style`'s files.
pub fn changed(style: Style, changes: Vec<FsChange>) -> Event {
    Event::Fs { style, changes }
}

/// The result of a scan of `style`'s sources.
pub fn scanned(style: Style, paths: &[&str]) -> Event {
    let files = files(paths);
    Event::Scanned { style, files }
}

/// The background keys of a user who never ran the daemon.
pub fn user_keys() -> BackgroundKeys {
    BackgroundKeys {
        picture_uri: "file:///usr/share/backgrounds/day.jpg".to_owned(),
        picture_uri_dark: Some("file:///usr/share/backgrounds/night.jpg".to_owned()),
        picture_options: "zoom".to_owned(),
    }
}

pub fn started(visible: Style, keys: BackgroundKeys, slots: PerStyle<Option<FileId>>) -> Event {
    started_with_locks(visible, keys, slots, PerStyle::default())
}

/// `Started` in a session whose administrator locked the keys of `locked`.
pub fn started_with_locks(
    visible: Style,
    keys: BackgroundKeys,
    slots: PerStyle<Option<FileId>>,
    locked: PerStyle<bool>,
) -> Event {
    let gnome = GnomeSnapshot {
        visible,
        keys,
        locked,
    };
    Event::Started { gnome, slots }
}

/// GNOME's report that the background keys hold `keys` now.
pub fn keys_changed(keys: BackgroundKeys) -> Event {
    Event::Gnome(GnomeEvent::KeysChanged(keys))
}

/// Does to GNOME's `keys` what `effect` says when it is a key write. Any
/// other effect leaves them alone.
pub fn write_keys(keys: &mut BackgroundKeys, effect: &Effect) {
    let Effect::SetKeys {
        uris,
        picture_options,
    } = effect
    else {
        return;
    };
    if let Some(uri) = &uris.light {
        keys.picture_uri = uri.clone();
    }
    if let Some(uri) = &uris.dark {
        keys.picture_uri_dark = Some(uri.clone());
    }
    if let Some(options) = picture_options {
        keys.picture_options = options.as_str().to_owned();
    }
}

/// The URIs of the daemon's slot files.
pub fn slot_uris() -> PerStyle<String> {
    PerStyle::from_fn(|style| format!("file:///home/u/.local/share/gpaperd/slots/{style}"))
}

pub const DEADLINE: Event = Event::Fired(Fired::Deadline);
pub const CLOCK_CHANGED: Event = Event::Fired(Fired::ClockChanged);

pub fn switch_to(style: Style) -> Event {
    Event::Gnome(GnomeEvent::StyleChanged(style))
}

/// The default config with both styles in alphabetical order, so that the
/// items a test expects are exact. The interval is 900 s.
pub fn alphabetical() -> Config {
    let mut config = Config::default();
    config.styles.light.order = Order::Alphabetical;
    config.styles.dark.order = Order::Alphabetical;
    config
}

pub fn init(config: Config, light: &[&str], dark: &[&str]) -> ModelInit {
    ModelInit {
        config: Some(config),
        local_files: PerStyle {
            light: Some(files(light)),
            dark: Some(files(dark)),
        },
        restored: None,
        seed: 7,
        slot_uris: slot_uris(),
        remote_cache_dir: PathBuf::from(REMOTE_CACHE_DIR),
    }
}

/// A daemon whose scans are still to come: each style's files arrive with
/// its `Scanned` event.
pub fn unscanned(config: Config) -> ModelInit {
    ModelInit {
        local_files: PerStyle::default(),
        ..init(config, &[], &[])
    }
}

/// Six shuffled light images, so that a round outlasts a test.
pub fn six_shuffled() -> ModelInit {
    init(
        Config::default(),
        &[
            "/l/1.jpg", "/l/2.jpg", "/l/3.jpg", "/l/4.jpg", "/l/5.jpg", "/l/6.jpg",
        ],
        &["/d/only.jpg"],
    )
}

/// Three light and three dark images in alphabetical order.
pub fn two_folders() -> ModelInit {
    two_folders_with(alphabetical())
}

pub fn two_folders_with(config: Config) -> ModelInit {
    init(
        config,
        &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"],
        &["/d/a.jpg", "/d/b.jpg", "/d/c.jpg"],
    )
}

/// A daemon that finished its first run at second 0 with light on screen.
pub fn running_world() -> World {
    let mut world = World::new(two_folders());
    world.start(at(0), Style::Light);
    world
}

/// Two light images and a dark folder that starts with five broken ones, so
/// that dark's first apply gives up; its sixth image, `/d/f.jpg`, is fine.
/// Dark's interval is 600 s, so that it falls due before light does.
pub fn dark_folder_starts_broken(mut config: Config) -> World {
    let dark = [
        "/d/a.jpg", "/d/b.jpg", "/d/c.jpg", "/d/d.jpg", "/d/e.jpg", "/d/f.jpg",
    ];
    config.styles.dark.interval = Duration::from_secs(600);
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &dark));
    world.broken = dark[..5].iter().map(|path| id(path)).collect();
    world
}

/// A slot file that the daemon did not write.
pub fn foreign_slot() -> Option<FileId> {
    Some(FileId { dev: 2, ino: 500 })
}

/// A state file from a run that left `current` in the light slot at second 0,
/// with `bag` as the rest of its shuffle round and `shown` as what the round
/// showed. The light slot on disk must be [`foreign_slot`] for the daemon to
/// find it intact. Dark never ran.
pub fn light_showing(current: &str, bag: &[&str], shown: &[&str]) -> Persisted {
    let ids = |paths: &[&str]| paths.iter().map(|path| id(path)).collect();
    Persisted {
        version: STATE_VERSION,
        styles: PerStyle {
            light: StylePersisted {
                current: Some(id(current)),
                slot: foreign_slot(),
                last_change: Some(at(0)),
                remaining: None,
                bag: ids(bag),
                shown: Some(ids(shown)),
                ..StylePersisted::default()
            },
            dark: StylePersisted::default(),
        },
        original_keys: None,
        banned: Default::default(),
    }
}

/// Stands in for the interpreter and for GNOME: it answers every `Probe`,
/// `Publish` and `Republish` the reducer emits, and it reports every key
/// write back, as GNOME does.
pub struct World {
    pub model: Model,
    /// Items whose probe fails because they are no image.
    pub broken: BTreeSet<ItemId>,
    /// Items whose probe fails for another reason.
    pub rejected: BTreeMap<ItemId, BadReason>,
    /// Items whose publish fails.
    pub unwritable: BTreeSet<ItemId>,
    /// Inode of each slot file on disk.
    pub slots: PerStyle<Option<FileId>>,
    /// GNOME's background keys.
    pub keys: BackgroundKeys,
    /// What reading the config file again gives, for `ReadConfig`.
    pub config_file: Result<Config, ConfigError>,
    /// Why writing the original keys back fails, for `RestoreKeys`. `None`
    /// when it works.
    pub restore_error: Option<String>,
    /// GNOME refuses every key write: the keys stay as they are.
    pub refuses_key_writes: bool,
    /// Why copying a favorite fails, for `SaveFavorites`. `None` when it
    /// works.
    pub favorite_error: Option<String>,
    last_slot_inode: u64,
}

impl World {
    pub fn new(init: ModelInit) -> World {
        let config_file = Ok(init.config.clone().unwrap_or_default());
        World {
            config_file,
            model: Model::new(init),
            broken: BTreeSet::new(),
            rejected: BTreeMap::new(),
            unwritable: BTreeSet::new(),
            slots: PerStyle::default(),
            keys: user_keys(),
            restore_error: None,
            refuses_key_writes: false,
            favorite_error: None,
            last_slot_inode: 0,
        }
    }

    /// The daemon after a restart: the slot files are still on disk and the
    /// state comes from the state file. Every file is readable and writable
    /// again.
    pub fn restart(&self, init: ModelInit) -> World {
        let restored = Some(self.model.persisted());
        World {
            slots: self.slots,
            keys: self.keys.clone(),
            last_slot_inode: self.last_slot_inode,
            ..World::new(ModelInit { restored, ..init })
        }
    }

    /// Sends `Started` with the keys as GNOME holds them and the slots as
    /// they are on disk.
    pub fn start(&mut self, now: SystemTime, visible: Style) -> Vec<Effect> {
        self.run(now, started(visible, self.keys.clone(), self.slots))
    }

    /// Something other than the daemon writes `uri` to `style`'s key.
    pub fn set_key(&mut self, now: SystemTime, style: Style, uri: &str) -> Vec<Effect> {
        match style {
            Style::Light => self.keys.picture_uri = uri.to_owned(),
            Style::Dark => self.keys.picture_uri_dark = Some(uri.to_owned()),
        }
        self.run(now, keys_changed(self.keys.clone()))
    }

    /// Feeds `event` at `now`, then answers effects until the reducer is
    /// quiet. Returns every effect in order.
    pub fn run(&mut self, now: SystemTime, event: Event) -> Vec<Effect> {
        self.run_all(now, [event])
    }

    /// Like [`World::run`], for events that all arrive before the first
    /// answer does.
    pub fn run_all<const N: usize>(&mut self, now: SystemTime, events: [Event; N]) -> Vec<Effect> {
        let mut trace = Vec::new();
        let mut inbox = VecDeque::from(events);
        let mut steps = 0;
        while let Some(event) = inbox.pop_front() {
            steps += 1;
            // A reducer that answers its own answers would fill the memory.
            assert!(
                steps <= MAX_STEPS_PER_RUN,
                "the reducer is not quiet after {MAX_STEPS_PER_RUN} steps; the last one was {event:?}"
            );
            for effect in step(&mut self.model, now, event) {
                inbox.extend(self.answer(&effect));
                trace.push(effect);
            }
        }
        trace
    }

    fn answer(&mut self, effect: &Effect) -> Vec<Event> {
        let answer = match effect {
            Effect::Probe { style, item, .. } => Some(Event::Probed {
                style: *style,
                item: item.clone(),
                verdict: self.probe(item),
            }),
            Effect::Publish { style, item, .. } => Some(Event::Published {
                style: *style,
                item: item.clone(),
                result: self.publish(*style, item),
            }),
            Effect::Republish(style) => Some(Event::Republished {
                style: *style,
                result: Ok(self.write_slot(*style)),
            }),
            Effect::SetKeys {
                uris,
                picture_options,
            } if self.refuses_key_writes => Some(Event::KeyWriteFailed {
                uris: uris.clone(),
                picture_options: *picture_options,
            }),
            Effect::SetKeys { .. } => {
                write_keys(&mut self.keys, effect);
                Some(keys_changed(self.keys.clone()))
            }
            Effect::RestoreKeys { id, keys } => return self.restore(*id, keys),
            Effect::ReadConfig(id) => Some(Event::ConfigRead {
                id: *id,
                result: self.config_file.clone(),
            }),
            Effect::SaveFavorites { id, .. } => Some(Event::FavoritesSaved {
                id: *id,
                result: self.favorite_error.clone().map_or(Ok(()), Err),
            }),
            _ => None,
        };
        answer.into_iter().collect()
    }

    /// Writes `keys` back, unless [`World::restore_error`] says it fails,
    /// and answers with GNOME's report of the change and `KeysRestored`.
    fn restore(&mut self, id: ReplyId, keys: &BackgroundKeys) -> Vec<Event> {
        let restored = Event::KeysRestored {
            id,
            result: self.restore_error.clone().map_or(Ok(()), Err),
        };
        if self.restore_error.is_some() {
            return vec![restored];
        }
        self.keys = keys.clone();
        vec![keys_changed(self.keys.clone()), restored]
    }

    /// Sends `command` as control request `n` and answers everything that
    /// follows.
    pub fn ask(&mut self, now: SystemTime, n: u64, command: Command) -> Vec<Effect> {
        self.run(now, control(n, command))
    }

    fn probe(&self, item: &ItemId) -> Result<(), BadReason> {
        if self.broken.contains(item) {
            return Err(BadReason::NotAnImage);
        }
        self.rejected.get(item).cloned().map_or(Ok(()), Err)
    }

    fn publish(&mut self, style: Style, item: &ItemId) -> Result<SlotWrite, String> {
        if self.unwritable.contains(item) {
            return Err("No space left on device".to_owned());
        }
        Ok(self.write_slot(style))
    }

    /// Puts `style`'s slot on a new inode.
    fn write_slot(&mut self, style: Style) -> SlotWrite {
        self.last_slot_inode += 1;
        let slot = FileId {
            dev: 2,
            ino: self.last_slot_inode,
        };
        self.slots[style] = Some(slot);
        SlotWrite {
            method: Method::Hardlink,
            slot,
        }
    }

    /// The item the daemon believes is in `style`'s slot.
    pub fn current(&self, style: Style) -> Option<ItemId> {
        self.model.persisted().styles[style].current.clone()
    }
}

/// Style and item of every `Publish` effect, in order.
pub fn published(trace: &[Effect]) -> Vec<(Style, &str)> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Publish { style, item, .. } => Some((*style, item.as_str())),
            _ => None,
        })
        .collect()
}

/// Style and item of every `Probe` effect, in order.
pub fn probed(trace: &[Effect]) -> Vec<(Style, &str)> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Probe { style, item, .. } => Some((*style, item.as_str())),
            _ => None,
        })
        .collect()
}

pub fn is_publish(effect: &Effect) -> bool {
    matches!(effect, Effect::Publish { .. })
}

pub fn is_set_keys(effect: &Effect) -> bool {
    matches!(effect, Effect::SetKeys { .. })
}

pub fn is_ready(effect: &Effect) -> bool {
    matches!(effect, Effect::Ready)
}

pub fn is_watch(effect: &Effect) -> bool {
    matches!(effect, Effect::Watch { .. })
}

pub fn count(trace: &[Effect], wanted: fn(&Effect) -> bool) -> usize {
    trace.iter().filter(|effect| wanted(effect)).count()
}

/// The styles whose key each `SetKeys` effect writes, in order.
pub fn keys_pointed(trace: &[Effect]) -> Vec<Vec<Style>> {
    let written = |uris: &PerStyle<Option<String>>| {
        Style::ALL
            .into_iter()
            .filter(|&style| uris[style].is_some())
            .collect()
    };
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetKeys { uris, .. } => Some(written(uris)),
            _ => None,
        })
        .collect()
}

/// The last status that `trace` reports.
pub fn reported(trace: &[Effect]) -> &Status {
    let mut statuses = trace.iter().rev().filter_map(|effect| match effect {
        Effect::Report(status) => Some(status),
        _ => None,
    });
    statuses.next().expect("a status report")
}

/// The URIs and the `picture-options` value of every `SetKeys` effect, in
/// order.
pub fn keys_written(trace: &[Effect]) -> Vec<(PerStyle<Option<&str>>, Option<PictureOptions>)> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetKeys {
                uris,
                picture_options,
            } => Some((
                PerStyle::from_fn(|style| uris[style].as_deref()),
                *picture_options,
            )),
            _ => None,
        })
        .collect()
}

/// The style of every `Republish` effect, in order.
pub fn republished(trace: &[Effect]) -> Vec<Style> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Republish(style) => Some(*style),
            _ => None,
        })
        .collect()
}

/// The deadline of every `Arm` effect, in order.
pub fn armed(trace: &[Effect]) -> Vec<Option<SystemTime>> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Arm(deadline) => Some(*deadline),
            _ => None,
        })
        .collect()
}

/// Control request `n` for `command`.
pub fn control(n: u64, command: Command) -> Event {
    Event::Control {
        id: ReplyId(n),
        command,
    }
}

/// Every reply in `trace`, in order, with its request number.
pub fn replies(trace: &[Effect]) -> Vec<(u64, &Reply)> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Reply { id, reply } => Some((id.0, reply)),
            _ => None,
        })
        .collect()
}

/// The one reply in `trace`.
pub fn the_reply(trace: &[Effect]) -> &Reply {
    match replies(trace).as_slice() {
        [(_, reply)] => reply,
        other => panic!("expected one reply, got {other:?}"),
    }
}

/// Where the first effect that `wanted` matches is in `trace`.
pub fn position(trace: &[Effect], wanted: impl Fn(&Effect) -> bool) -> usize {
    trace
        .iter()
        .position(wanted)
        .expect("the effect is in the trace")
}
