//! Events into the reducer and effects out of it.
//!
//! Adapters turn the outside world into [`Event`]s. The interpreter executes
//! each [`Effect`] and feeds its result back as an event, so retries and "try
//! the next candidate" stay in the core. Everything here is plain data.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::catalog::{BadReason, FileId, FsChange, ItemId, LocalFile, RemoteBlob};
use crate::config::{Config, ConfigError, HookCommand, LinkPolicy, PictureOptions, Source};
use crate::control::{Command, Reply, ReplyId};
use crate::style::{PerStyle, Style};

/// Something that happened outside the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The daemon is up. Always the first event, sent exactly once.
    Started {
        /// GNOME's state before the daemon wrote anything.
        gnome: GnomeSnapshot,
        /// Inode of each slot file as found on disk; `None` when it is missing.
        slots: PerStyle<Option<FileId>>,
    },
    /// The wall-clock timer fired.
    Fired(Fired),
    /// The config file changed and was read again. `Ok` holds the new config,
    /// to be applied as a diff against the running one. `Err` is a file that
    /// does not parse or validate: the running config stays.
    ConfigChanged(Result<Config, ConfigError>),
    /// Answer to [`Effect::Watch`]: every local image file of `style`'s
    /// sources, as one scan found them. It replaces what the catalog knew of
    /// the style. Duplicates are allowed; the catalog removes them.
    Scanned { style: Style, files: Vec<LocalFile> },
    /// Local files of `style` changed after its last [`Event::Scanned`]:
    /// what one look at the sources found. Every [`FsChange::Removed`] comes
    /// before every [`FsChange::Found`], so a renamed file leaves the catalog
    /// before its inode comes back under the new path.
    Fs {
        style: Style,
        changes: Vec<FsChange>,
    },
    /// Result of [`Effect::Probe`], with the same `style` and `item`.
    Probed {
        style: Style,
        item: ItemId,
        verdict: Result<(), BadReason>,
    },
    /// Result of [`Effect::Publish`], with the same `style` and `item`. The
    /// error is the I/O error text.
    Published {
        style: Style,
        item: ItemId,
        result: Result<SlotWrite, String>,
    },
    /// Result of [`Effect::Republish`] for `style`. The error is the I/O
    /// error text.
    Republished {
        style: Style,
        result: Result<SlotWrite, String>,
    },
    /// `style`'s slot file is gone: someone deleted it or moved it away. Not
    /// sent for a slot that the daemon itself replaces.
    SlotLost(Style),
    /// What the remote cache held when the daemon started: one entry per URL
    /// whose blob is on disk, for every URL the cache knows, listed in the
    /// config or not. Sent once, right after [`Event::Started`] and before
    /// any other event. Until it comes, which remote images are cached is
    /// unknown.
    CacheLoaded(Vec<RemoteBlob>),
    /// Result of [`Effect::Fetch`] for `url`.
    Fetched { url: Url, outcome: FetchOutcome },
    /// GNOME's style or background keys changed.
    Gnome(GnomeEvent),
    /// A control client sent `command`. The core answers with exactly one
    /// [`Effect::Reply`] carrying the same `id`, also before
    /// [`Event::Started`] and after [`Event::Shutdown`], so that no caller
    /// waits for ever.
    Control { id: ReplyId, command: Command },
    /// Answer to [`Effect::ReadConfig`]: the config file as read again, for
    /// the [`Command::Reload`] with this `id`. Applied as
    /// [`Event::ConfigChanged`] is, then answered with [`Reply::Reloaded`].
    ConfigRead {
        id: ReplyId,
        result: Result<Config, ConfigError>,
    },
    /// Answer to [`Effect::RestoreKeys`] for the [`Command::Restore`] with
    /// this `id`: whether every original key was written, with the error
    /// text when not. The keys that could be written are written even then.
    /// Answered with [`Reply::Done`] or [`Reply::Failed`].
    KeysRestored {
        id: ReplyId,
        result: Result<(), String>,
    },
    /// Answer to [`Effect::SaveFavorites`] for the [`Command::Favorite`]
    /// with this `id`: whether every copy was written, with the I/O error
    /// text when not. Answered with [`Reply::Done`] or [`Reply::Failed`].
    FavoritesSaved {
        id: ReplyId,
        result: Result<(), String>,
    },
    /// Keys of an [`Effect::SetKeys`] that were not stored, or whose write
    /// was not confirmed. A `Some` names such a key. A `None` key was stored
    /// or was not part of the write. An unconfirmed write may still land
    /// later, and GNOME then reports it as any other change.
    KeyWriteFailed {
        uris: PerStyle<Option<String>>,
        picture_options: Option<PictureOptions>,
    },
    /// The screen was locked or unlocked: `org.gnome.ScreenSaver`'s
    /// `ActiveChanged`, or its `GetActive` at start. May come before
    /// [`Event::Started`]. Only read under `pause_when_locked`.
    ScreenLock(ScreenLock),
    /// `SIGTERM` or `SIGINT`. The last event.
    Shutdown,
}

/// Whether the screen is locked, as GNOME's screen shield says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenLock {
    Locked,
    #[default]
    Unlocked,
}

/// Why the timer woke up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// The armed deadline passed.
    Deadline,
    /// The wall clock was stepped. The armed deadline may be stale: recompute
    /// and re-arm.
    ClockChanged,
}

/// How a fetch of one URL ended. The fetcher has already updated the cache
/// and logged the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// A new version is in the cache: a `200` with an image. `blob` is its
    /// absolute path, under a name no earlier version had.
    Stored { blob: PathBuf },
    /// `304 Not Modified`, or a refusal that is not retried: `401`, `403`,
    /// a body that is not an image or is too large. A cached blob, if any,
    /// stays.
    NotModified,
    /// `404` or `410`. A cached blob stays in the cache and may still be
    /// shown.
    Gone,
    /// The server was not reached or stopped answering: a connect, DNS or
    /// TLS error, a timeout, or a body cut short. A cached blob stays. Fetch
    /// again `retry_in` from now, or sooner when connectivity becomes full.
    Unreachable { retry_in: Duration },
    /// Anything else: a `429`, a `5xx`, a refused redirect, or a cache write
    /// that failed. A cached blob stays. Fetch again no sooner than
    /// `retry_in` from now, which honours the server's `Retry-After`,
    /// whatever the connectivity does.
    Failed { retry_in: Duration },
}

/// Whether the network is fully up, as GLib's `GNetworkMonitor` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// The default route reaches the internet.
    Full,
    /// Anything less: no network, local only, limited, or a captive portal.
    Limited,
}

/// How a slot got its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Hardlink,
    Reflink,
    Copy,
}

impl Method {
    /// `"hardlink"`, `"reflink"` or `"copy"`: the `method` log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Hardlink => "hardlink",
            Method::Reflink => "reflink",
            Method::Copy => "copy",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A successful publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotWrite {
    pub method: Method,
    /// Inode of the slot file after the rename.
    pub slot: FileId,
}

/// A change reported by the GNOME adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GnomeEvent {
    /// `color-scheme` changed and the shell now shows this style. Not sent
    /// when the style stays the same.
    StyleChanged(Style),
    /// One or more background keys changed, whoever wrote them: the daemon's
    /// own writes are reported too. Holds one consistent read of all keys,
    /// taken after the change settled.
    KeysChanged(BackgroundKeys),
    /// An administrator locked or unlocked a background key: whether each
    /// style's key is locked now, as in [`GnomeSnapshot::locked`]. Sent when
    /// that differs from the last report or from the snapshot at start.
    LocksChanged(PerStyle<bool>),
    /// Sent once with the level at start, then whenever the level becomes
    /// full or stops being full. Not sent for a change between two levels
    /// below full.
    ConnectivityChanged(Connectivity),
}

/// The `org.gnome.desktop.background` keys the daemon cares about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundKeys {
    pub picture_uri: String,
    /// `None` when the schema has no `picture-uri-dark` key (GNOME < 42).
    pub picture_uri_dark: Option<String>,
    /// The enum nick, for example `"zoom"` or `"none"`.
    pub picture_options: String,
}

impl BackgroundKeys {
    /// What `style`'s key holds: `picture-uri` for light, `picture-uri-dark`
    /// for dark. `None` for a key that the schema does not have.
    pub fn uri(&self, style: Style) -> Option<&str> {
        match style {
            Style::Light => Some(&self.picture_uri),
            Style::Dark => self.picture_uri_dark.as_deref(),
        }
    }
}

/// GNOME's state, read in one go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GnomeSnapshot {
    /// The style the shell shows now.
    pub visible: Style,
    pub keys: BackgroundKeys,
    /// Whether an administrator locked the style's key: `picture-uri` for
    /// light, `picture-uri-dark` for dark. The daemon cannot write a locked
    /// key. `false` for a key that the schema does not have.
    pub locked: PerStyle<bool>,
}

/// Something the interpreter must do. Effects of one `step` run in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Write the background keys in one transaction: `picture-uri` to
    /// `uris.light`, `picture-uri-dark` to `uris.dark` and, with `Some`,
    /// `picture-options`. A value that is `None` leaves its key as it is; at
    /// least one of the three is `Some`. A failure is logged and answered
    /// with [`Event::KeyWriteFailed`], which names the keys that were not
    /// stored, or the whole write when settings did not answer.
    ///
    /// A URI is the style's slot URI in slots mode and the image's own
    /// `file://` URI in uri mode. A key that later holds anything but the
    /// last URI written to it was changed from outside.
    SetKeys {
        uris: PerStyle<Option<String>>,
        picture_options: Option<PictureOptions>,
    },
    /// Check that `path` is an image the shell can show. Answer with
    /// [`Event::Probed`].
    Probe {
        style: Style,
        item: ItemId,
        path: PathBuf,
        /// The running config's `max_pixels`: a larger image is
        /// [`BadReason::TooLarge`].
        max_pixels: u64,
    },
    /// Replace `style`'s slot with the content of `path`. Answer with
    /// [`Event::Published`].
    Publish {
        style: Style,
        item: ItemId,
        path: PathBuf,
        link: LinkPolicy,
    },
    /// Replace this style's slot with a copy of its own content: the same
    /// bytes on a new inode, so that the shell drops a stale texture and
    /// loads the file again. Needs no source file. Answer with
    /// [`Event::Republished`], exactly once.
    ///
    /// Never emitted while a [`Effect::Publish`] of the same style is
    /// unanswered, and no publish of that style follows before the answer:
    /// the two never meet in the slot's temp file.
    Republish(Style),
    /// Scan `sources` and answer with [`Event::Scanned`]. Then watch them and
    /// send an [`Event::Fs`] for every later change. This replaces `style`'s
    /// earlier watch: once it ran, no event of the earlier watch arrives.
    ///
    /// Emitted for each style when the daemon starts, whether or not the
    /// model was built over a scan, and for a style whose sources a config
    /// reload changed.
    Watch { style: Style, sources: Vec<Source> },
    /// Download `url` into the remote cache: a conditional `GET` when a
    /// version is cached, a plain one otherwise. Answer with
    /// [`Event::Fetched`], exactly once.
    ///
    /// Emitted once per URL, however many styles list it, and never while a
    /// fetch of the same URL is unanswered. The interpreter runs at most
    /// four fetches at once and queues the rest.
    Fetch(Url),
    /// No style lists `url` any more: abort its fetch in flight and forget
    /// its backoff. No [`Event::Fetched`] of an earlier fetch of `url`
    /// arrives after this. The cache entry stays until garbage collection.
    DropRemote(Url),
    /// Collect the remote cache's garbage: delete the entry of every URL
    /// that is not in `keep` and was not for more than seven days, and every
    /// blob that no `meta.toml` names. A slot linked to a deleted blob keeps
    /// its data. No event comes back; a failure is logged.
    CollectGarbage { keep: Vec<Url> },
    /// Arm the timer for this instant, replacing any earlier deadline. `None`
    /// disarms it. The one timer serves every deadline of the model: the
    /// rotations and the short delays listed in [`crate::step`].
    Arm(Option<SystemTime>),
    /// Write `Model::persisted()` to the state file.
    Persist,
    /// The first apply is settled: send `READY=1`. Emitted exactly once.
    Ready,
    /// The status changed: update `STATUS=` and log it. The D-Bus server
    /// also derives its properties from it and emits `PropertiesChanged`
    /// for those that differ.
    Report(Status),
    /// Answer the control request `id`. Exactly one per
    /// [`Event::Control`].
    Reply { id: ReplyId, reply: Reply },
    /// `item` is on screen in `style` now: emit the D-Bus `Changed` signal.
    /// After every successful publish, and in uri mode after the key write
    /// that shows a new image.
    Changed { style: Style, item: ItemId },
    /// Read the config file again for the [`Command::Reload`] `id`. Answer
    /// with [`Event::ConfigRead`], exactly once.
    ReadConfig(ReplyId),
    /// Write `keys` back as they are, in one transaction: `gpaperctl
    /// restore`. Holds the original keys saved in the state file. A key the
    /// schema does not have (`picture_uri_dark` is `None`) is left alone.
    /// Answer with [`Event::KeysRestored`] for `id`, exactly once.
    RestoreKeys { id: ReplyId, keys: BackgroundKeys },
    /// Copy each of `images` into `dir`, creating `dir` when it is missing:
    /// `gpaperctl favorite`. A copy never replaces a file: when `name` is
    /// taken it gets a numeric suffix. No source file is changed. Answer
    /// with [`Event::FavoritesSaved`] for `id`, exactly once.
    SaveFavorites {
        id: ReplyId,
        dir: PathBuf,
        images: Vec<Favorite>,
    },
    /// Start the `on_change` command, with no shell, and do not wait for
    /// it: nothing comes back. A failure to start it and a non-zero exit
    /// are logged.
    RunHook(HookRun),
}

/// One run of the `on_change` command for [`Effect::RunHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRun {
    pub command: HookCommand,
    /// `GPAPERD_EVENT`.
    pub event: HookEvent,
    /// `GPAPERD_STYLE`: the style whose image changed, or the style now on
    /// screen.
    pub style: Style,
    /// `GPAPERD_ID`: the item that style shows.
    pub item: ItemId,
    /// `GPAPERD_PATH`: the image's own file, a local image's path or a
    /// remote image's cached blob, never the slot. `None` for a remote image
    /// that no catalog holds any more.
    pub path: Option<PathBuf>,
    /// `GPAPERD_VISIBLE`: whether `style` is on screen.
    pub visible: bool,
}

/// Why [`Effect::RunHook`] runs the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// A style shows another image.
    Changed,
    /// The shell switched to the other style.
    Switched,
}

impl HookEvent {
    /// `"changed"` or `"switched"`: the value of `GPAPERD_EVENT`.
    pub const fn as_str(self) -> &'static str {
        match self {
            HookEvent::Changed => "changed",
            HookEvent::Switched => "switched",
        }
    }
}

/// One image for [`Effect::SaveFavorites`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    /// The file to copy: a local image, or a remote image's cached blob.
    pub source: PathBuf,
    /// The name of the copy: one path component, never empty, `.` or `..`.
    pub name: String,
}

/// A summary of the daemon's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub visible: Style,
    /// The item in each slot, as far as the daemon knows.
    pub current: PerStyle<Option<ItemId>>,
    /// When the next rotation is due. The timer may be armed for something
    /// sooner, such as the next probe of a truncated file.
    pub next_change: Option<SystemTime>,
    /// Why a style does not rotate. `None` for a style that does, or that
    /// only has nothing to rotate to. When several reasons hold, the one
    /// that lasts longest: [`Pause::NoKey`], [`Pause::LockedKey`],
    /// [`Pause::User`], [`Pause::ExternalChange`], then
    /// [`Pause::ScreenLocked`].
    pub paused: PerStyle<Option<Pause>>,
}

/// Why a style does not rotate although nothing is wrong with its images.
///
/// Serialises in kebab case, as in the `Status` JSON: `"user"`,
/// `"external-change"`, `"locked-key"`, `"no-key"`, `"screen-locked"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pause {
    /// `gpaperctl pause` or `restore` stopped the style. Saved; ends only on
    /// `gpaperctl resume`. Separate from the pauses below: both can hold at
    /// once, and ending one leaves the other.
    User,
    /// Something else changed the style's key, and `on_external_change` or
    /// its flip guard stopped the style. Ends on a config reload, or when
    /// the key holds the daemon's URI again.
    ExternalChange,
    /// An administrator locked the style's key on another value than the
    /// daemon's URI. Lasts until the lock is lifted, or until the locked
    /// value is the slot URI.
    LockedKey,
    /// The schema has no key for the style: dark on GNOME < 42. Lasts for
    /// the run.
    NoKey,
    /// The screen is locked and `pause_when_locked` is on. Holds both
    /// styles, next to any other pause; ends on unlock, or when a reload
    /// turns the setting off. Not saved.
    ScreenLocked,
}
