//! Config dotfile: the types the reducer reads, and the pure TOML parser.
//!
//! Parsing does no I/O. The caller reads the file and passes its text plus a
//! [`ParseContext`], so tests need no real environment. The surface is the
//! plan's "Config dotfile" section.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, MapAccess, Visitor};
use toml::Spanned;
use url::Url;

use crate::style::PerStyle;

/// Shortest allowed rotation interval, inclusive.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Longest allowed rotation interval, inclusive: a week. The deadline is a
/// saved wall-clock time, so an interval this long survives restarts and
/// suspend.
pub const MAX_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Interval when neither `[general]` nor the style table sets one.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// `max_pixels` when `[general]` does not set it: 100 megapixels.
pub const DEFAULT_MAX_PIXELS: u64 = 100_000_000;
/// Shortest allowed `remote_refresh` period, inclusive. A shorter one would
/// ask every server again right after each answer and pile up blobs.
pub const MIN_REMOTE_REFRESH: Duration = Duration::from_secs(60 * 60);
/// Shortest allowed `poll` period of a directory source, inclusive. A typo
/// such as `5ms` for `5m` would make the watch scan without a pause.
pub const MIN_POLL: Duration = Duration::from_secs(1);

/// A validated config.
///
/// Invariants after [`Config::parse`]: every path is absolute and expanded,
/// every interval is within [`MIN_INTERVAL`]`..=`[`MAX_INTERVAL`], a
/// `remote_refresh` period is at least [`MIN_REMOTE_REFRESH`], every URL is
/// `http` or `https` with a host, and per-style overrides are already resolved
/// into [`StyleConfig`].
///
/// `Config::default()` is what an empty file parses to: default settings and
/// no sources. The daemon uses it when the config file is missing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub general: General,
    pub styles: PerStyle<StyleConfig>,
}

impl Config {
    /// Parses and validates config text. Pure: no file system or environment
    /// access beyond `ctx`.
    ///
    /// Tables `[general]`, `[light]` and `[dark]` are all optional. A missing
    /// key takes its value from the type's `Default`. Any unknown table or key
    /// is an error.
    ///
    /// `[general]` keys:
    /// - `interval`: humantime duration (`90s`, `15m`, `1h`, `7d`), within
    ///   [`MIN_INTERVAL`]`..=`[`MAX_INTERVAL`] inclusive.
    /// - `order`: `"random"` or `"alphabetical"`.
    /// - `picture_options`: `"wallpaper"`, `"centered"`, `"scaled"`,
    ///   `"stretched"`, `"zoom"` or `"spanned"`. `"none"` is rejected.
    /// - `on_external_change`: `"pause"`, `"reassert-next-tick"` or
    ///   `"reassert-now"`.
    /// - `rotate_hidden`: bool.
    /// - `empty_style`: `"use-other-style"` or `"keep-current"`.
    /// - `remote_refresh`: `"startup"`, `"never"` or a humantime duration.
    /// - `link`: bool; `true` is [`LinkPolicy::PreferLink`], `false` is
    ///   [`LinkPolicy::AlwaysCopy`].
    /// - `apply_mode`: `"slots"` or `"uri"`.
    /// - `max_pixels`: a whole number of pixels, greater than zero.
    /// - `pause_when_locked`: bool.
    /// - `favorites_dir`: a path or a `file://` URI, expanded as source
    ///   paths are. Unset by default.
    /// - `on_change`: a non-empty array of strings, the program and its
    ///   arguments: [`HookCommand`]. Unset by default.
    ///
    /// `[light]` and `[dark]` keys: `interval` and `order` (each overrides the
    /// general value for that style) and `sources`.
    ///
    /// A source is a string or a table:
    /// - String starting with `http://` or `https://`: [`Source::Url`].
    /// - String starting with `file://`: percent-decoded to a path,
    ///   [`Source::Path`]. The URI must be absolute and name no remote host.
    /// - String starting with any other `scheme://`: an error.
    /// - Any other string: a path, [`Source::Path`].
    /// - `{ dir = "…" }` with optional `recursive`, `hidden`,
    ///   `follow_symlinks` and `poll` (humantime duration): [`Source::Dir`].
    /// - `{ file = "…" }`: [`Source::File`].
    /// - `{ url = "…" }`: [`Source::Url`].
    ///
    /// `dir` and `file` take a path or a `file://` URI. The `dir` options are
    /// errors without `dir`. `poll` must be at least [`MIN_POLL`], and a
    /// `remote_refresh` duration at least [`MIN_REMOTE_REFRESH`]. Scheme names are matched
    /// without case.
    ///
    /// Path expansion, for every path that is not a `file://` URI: a leading
    /// `~`, alone or before `/`, becomes `ctx.home_dir`; `$VAR` and `${VAR}`
    /// are looked up with
    /// `ctx.env`, and an undefined variable is an error; a relative result is
    /// joined onto `ctx.config_dir`. Paths are not canonicalised and need not
    /// exist.
    ///
    /// A `$` that starts no name stands for itself. There is no escape for a
    /// literal `$NAME`: write such a path as a `file://` URI. `~user` and an
    /// empty path are errors.
    ///
    /// # Errors
    ///
    /// Returns the first problem found, with the 1-based line and column of
    /// the offending key or value in `text`.
    pub fn parse(text: &str, ctx: &ParseContext<'_>) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|err| {
            let offset = err.span().map_or(0, |span| span.start);
            ConfigError::locate(text, offset, err.message())
        })?;
        raw.resolve(ctx)
            .map_err(|invalid| ConfigError::locate(text, invalid.offset, invalid.message))
    }
}

/// What [`Config::parse`] needs from the outside world.
#[derive(Clone, Copy)]
pub struct ParseContext<'a> {
    /// Directory of the *resolved* config file (symlinks followed). Relative
    /// source paths are joined onto it.
    pub config_dir: &'a Path,
    /// Replaces a leading `~`.
    pub home_dir: &'a Path,
    /// Environment lookup for `$VAR` and `${VAR}`. `None` means undefined.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
}

/// A config problem, located in the TOML text.
///
/// `line` and `column` are 1-based; `column` counts characters. A problem
/// that belongs to no token points at line 1, column 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}, column {column}: {message}")]
pub struct ConfigError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl ConfigError {
    /// Turns a byte offset into `text` into a line and a column.
    fn locate(text: &str, offset: usize, message: impl Into<String>) -> ConfigError {
        let before = text.get(..offset).unwrap_or(text);
        let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        ConfigError {
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
            message: message.into(),
        }
    }
}

/// The `[general]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct General {
    /// `None` leaves GNOME's `picture-options` alone, except that the reducer
    /// turns a current `none` into `zoom`.
    pub picture_options: Option<PictureOptions>,
    /// What follows when something else changes a background key.
    pub on_external_change: ExternalChangePolicy,
    /// Rotate the style that is not on screen too.
    pub rotate_hidden: bool,
    pub empty_style: EmptyStylePolicy,
    /// Read from M3 on.
    pub remote_refresh: RemoteRefresh,
    pub link: LinkPolicy,
    pub apply_mode: ApplyMode,
    /// The largest image that is shown, as width times height. Greater than
    /// zero.
    pub max_pixels: u64,
    /// Stop the timers of both styles while the screen is locked. Off by
    /// default.
    pub pause_when_locked: bool,
    /// Where `gpaperctl favorite` copies the image on screen: absolute and
    /// expanded. `None` when unset: the command then fails.
    pub favorites_dir: Option<PathBuf>,
    /// What runs when the image on screen changes. `None` when unset.
    pub on_change: Option<HookCommand>,
}

impl Default for General {
    fn default() -> General {
        General {
            picture_options: None,
            on_external_change: ExternalChangePolicy::default(),
            rotate_hidden: false,
            empty_style: EmptyStylePolicy::default(),
            remote_refresh: RemoteRefresh::default(),
            link: LinkPolicy::default(),
            apply_mode: ApplyMode::default(),
            max_pixels: DEFAULT_MAX_PIXELS,
            pause_when_locked: false,
            favorites_dir: None,
            on_change: None,
        }
    }
}

/// The `on_change` command, run with no shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    /// An absolute path, a leading `~` already expanded, or a bare name
    /// that is looked up in `PATH` when the command runs.
    pub program: PathBuf,
    /// Passed as written: no `~` or `$VAR` expansion.
    pub args: Vec<String>,
}

/// One style's settings, with `[general]` values already filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleConfig {
    /// Within [`MIN_INTERVAL`]`..=`[`MAX_INTERVAL`].
    pub interval: Duration,
    pub order: Order,
    /// In config order. May be empty: see [`EmptyStylePolicy`].
    pub sources: Vec<Source>,
}

impl Default for StyleConfig {
    fn default() -> StyleConfig {
        StyleConfig {
            interval: DEFAULT_INTERVAL,
            order: Order::default(),
            sources: Vec::new(),
        }
    }
}

/// Rotation order within a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Order {
    /// Shuffle bag: every item once per round.
    #[default]
    Random,
    /// Natural, case-insensitive order on the file name.
    Alphabetical,
}

/// A value for GNOME's `picture-options` key. `none` is left out on purpose:
/// it makes the shell load no file at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PictureOptions {
    Wallpaper,
    Centered,
    Scaled,
    Stretched,
    Zoom,
    Spanned,
}

impl PictureOptions {
    /// Every value, in the order of the GSettings enum.
    pub const ALL: [PictureOptions; 6] = [
        PictureOptions::Wallpaper,
        PictureOptions::Centered,
        PictureOptions::Scaled,
        PictureOptions::Stretched,
        PictureOptions::Zoom,
        PictureOptions::Spanned,
    ];

    /// The GSettings enum nick, for example `"zoom"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            PictureOptions::Wallpaper => "wallpaper",
            PictureOptions::Centered => "centered",
            PictureOptions::Scaled => "scaled",
            PictureOptions::Stretched => "stretched",
            PictureOptions::Zoom => "zoom",
            PictureOptions::Spanned => "spanned",
        }
    }
}

/// What to do when something else changes the background keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExternalChangePolicy {
    #[default]
    Pause,
    ReassertNextTick,
    ReassertNow,
}

/// What a style with no usable image shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmptyStylePolicy {
    /// Its slot follows the other style's current image.
    #[default]
    UseOtherStyle,
    /// Its slot is left as it is.
    KeepCurrent,
}

/// When remote URLs are revalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteRefresh {
    /// Once per daemon start.
    #[default]
    Startup,
    /// At start and then on this period.
    Every(Duration),
    /// Only fetch what is missing.
    Never,
}

/// How a slot file gets its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkPolicy {
    /// Hardlink, else reflink, else copy.
    #[default]
    PreferLink,
    /// Always copy the bytes into a new inode.
    AlwaysCopy,
}

/// How a rotation reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyMode {
    /// The background keys point at two slot files; a rotation replaces a
    /// slot file and writes no key.
    #[default]
    Slots,
    /// A rotation writes the image's own `file://` URI to the background
    /// keys. The fallback for a GNOME release that does not reload a
    /// replaced slot file.
    Uri,
}

/// One entry of a style's `sources` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A plain string or `file://` URI. Whether it is a directory or a file is
    /// decided at scan time; a directory is scanned with
    /// `DirOptions::default()`.
    Path(PathBuf),
    /// A `{ dir = "…" }` table.
    Dir { path: PathBuf, options: DirOptions },
    /// A `{ file = "…" }` table.
    File(PathBuf),
    /// An `http` or `https` URL with a host. Never fetched before M3.
    Url(Url),
}

/// Options of a directory source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirOptions {
    /// Descend into subdirectories.
    pub recursive: bool,
    /// Include entries whose name starts with a dot.
    pub hidden: bool,
    pub follow_symlinks: bool,
    /// Poll period for file systems without inotify, at least
    /// [`MIN_POLL`].
    pub poll: Option<Duration>,
}

impl Default for DirOptions {
    fn default() -> DirOptions {
        DirOptions {
            recursive: false,
            hidden: false,
            follow_symlinks: true,
            poll: None,
        }
    }
}

/// A validation failure at a byte offset of the config text.
struct Invalid {
    offset: usize,
    message: String,
}

impl Invalid {
    fn at<T>(raw: &Spanned<T>, message: String) -> Invalid {
        Invalid {
            offset: raw.span().start,
            message,
        }
    }
}

/// The file as written, before validation and path expansion.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, expecting = "a table")]
struct RawConfig {
    general: RawGeneral,
    light: RawStyle,
    dark: RawStyle,
}

impl RawConfig {
    fn resolve(self, ctx: &ParseContext<'_>) -> Result<Config, Invalid> {
        let inherited = self.general.style_defaults()?;
        Ok(Config {
            general: self.general.resolve(ctx)?,
            styles: PerStyle {
                light: self.light.resolve(&inherited, ctx)?,
                dark: self.dark.resolve(&inherited, ctx)?,
            },
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, expecting = "a table")]
struct RawGeneral {
    interval: Option<Spanned<String>>,
    order: Option<Order>,
    picture_options: Option<PictureOptions>,
    on_external_change: ExternalChangePolicy,
    rotate_hidden: bool,
    empty_style: EmptyStylePolicy,
    remote_refresh: Option<Spanned<String>>,
    link: Option<bool>,
    apply_mode: ApplyMode,
    max_pixels: Option<Spanned<i64>>,
    pause_when_locked: bool,
    favorites_dir: Option<Spanned<String>>,
    on_change: Option<Spanned<Vec<String>>>,
}

impl RawGeneral {
    /// What a style table inherits for the keys it does not set.
    fn style_defaults(&self) -> Result<StyleConfig, Invalid> {
        let built_in = StyleConfig::default();
        Ok(StyleConfig {
            interval: interval_or(self.interval.as_ref(), built_in.interval)?,
            order: self.order.unwrap_or(built_in.order),
            sources: built_in.sources,
        })
    }

    fn resolve(self, ctx: &ParseContext<'_>) -> Result<General, Invalid> {
        Ok(General {
            picture_options: self.picture_options,
            on_external_change: self.on_external_change,
            rotate_hidden: self.rotate_hidden,
            empty_style: self.empty_style,
            remote_refresh: match &self.remote_refresh {
                Some(raw) => remote_refresh(raw)?,
                None => RemoteRefresh::default(),
            },
            link: match self.link {
                Some(false) => LinkPolicy::AlwaysCopy,
                Some(true) | None => LinkPolicy::PreferLink,
            },
            apply_mode: self.apply_mode,
            max_pixels: match &self.max_pixels {
                Some(raw) => max_pixels(raw)?,
                None => DEFAULT_MAX_PIXELS,
            },
            pause_when_locked: self.pause_when_locked,
            favorites_dir: match &self.favorites_dir {
                Some(raw) => Some(
                    local_path(raw.get_ref(), ctx)
                        .map_err(|message| Invalid::at(raw, format!("favorites_dir: {message}")))?,
                ),
                None => None,
            },
            on_change: match &self.on_change {
                Some(raw) => Some(hook_command(raw, ctx)?),
                None => None,
            },
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, expecting = "a table")]
struct RawStyle {
    interval: Option<Spanned<String>>,
    order: Option<Order>,
    sources: Vec<Spanned<RawSource>>,
}

impl RawStyle {
    fn resolve(
        self,
        inherited: &StyleConfig,
        ctx: &ParseContext<'_>,
    ) -> Result<StyleConfig, Invalid> {
        Ok(StyleConfig {
            interval: interval_or(self.interval.as_ref(), inherited.interval)?,
            order: self.order.unwrap_or(inherited.order),
            sources: self
                .sources
                .into_iter()
                .map(|raw| resolve_source(raw, ctx))
                .collect::<Result<_, _>>()?,
        })
    }
}

fn interval_or(raw: Option<&Spanned<String>>, fallback: Duration) -> Result<Duration, Invalid> {
    let Some(raw) = raw else {
        return Ok(fallback);
    };
    humantime::parse_duration(raw.get_ref())
        .ok()
        .filter(|interval| (MIN_INTERVAL..=MAX_INTERVAL).contains(interval))
        .ok_or_else(|| {
            let message = format!(
                "interval must be a duration from {} to {}, such as \"15m\"",
                humantime::format_duration(MIN_INTERVAL),
                humantime::format_duration(MAX_INTERVAL),
            );
            Invalid::at(raw, message)
        })
}

fn remote_refresh(raw: &Spanned<String>) -> Result<RemoteRefresh, Invalid> {
    match raw.get_ref().as_str() {
        "startup" => Ok(RemoteRefresh::Startup),
        "never" => Ok(RemoteRefresh::Never),
        period => humantime::parse_duration(period)
            .ok()
            .filter(|period| *period >= MIN_REMOTE_REFRESH)
            .map(RemoteRefresh::Every)
            .ok_or_else(|| {
                let message = format!(
                    "remote_refresh must be \"startup\", \"never\" or a duration of at \
                     least {}, such as \"24h\"",
                    humantime::format_duration(MIN_REMOTE_REFRESH)
                );
                Invalid::at(raw, message)
            }),
    }
}

/// Zero is refused: it would turn every image away.
fn max_pixels(raw: &Spanned<i64>) -> Result<u64, Invalid> {
    u64::try_from(*raw.get_ref())
        .ok()
        .filter(|&pixels| pixels > 0)
        .ok_or_else(|| {
            let message = format!(
                "max_pixels must be a whole number of pixels greater than zero, \
                 such as {DEFAULT_MAX_PIXELS}"
            );
            Invalid::at(raw, message)
        })
}

/// The program of `on_change` gets only `~` expanded: it is no source path,
/// and its arguments are the program's business.
fn hook_command(
    raw: &Spanned<Vec<String>>,
    ctx: &ParseContext<'_>,
) -> Result<HookCommand, Invalid> {
    let Some((program, args)) = raw.get_ref().split_first() else {
        let message = "on_change must be a non-empty array of strings, such as \
                       [\"notify-send\", \"Wallpaper changed\"]";
        return Err(Invalid::at(raw, message.to_owned()));
    };
    let program = hook_program(program, ctx.home_dir)
        .map_err(|message| Invalid::at(raw, format!("on_change: {message}")))?;
    Ok(HookCommand {
        program,
        args: args.to_vec(),
    })
}

/// A relative path with a `/` would depend on the daemon's working
/// directory, so only a bare name, an absolute path or `~/…` is accepted.
/// `~` alone or a trailing `/` names a directory, which can never run.
fn hook_program(text: &str, home_dir: &Path) -> Result<PathBuf, String> {
    if text.is_empty() {
        return Err("the program must not be empty".to_owned());
    }
    if text == "~" || text.ends_with('/') {
        return Err("the program must name a file, not a directory".to_owned());
    }
    let (mut program, rest) = expand_tilde(text, home_dir)?;
    program.push(rest);
    let program = PathBuf::from(program);
    let is_bare_name = !text.contains('/') && !text.starts_with('~');
    if is_bare_name || program.is_absolute() {
        Ok(program)
    } else {
        Err("a program path must be absolute or start with `~/`; \
             a bare name is looked up in PATH"
            .to_owned())
    }
}

/// Parses the `poll` period of a directory source.
fn poll_period(text: &str) -> Option<Duration> {
    humantime::parse_duration(text)
        .ok()
        .filter(|period| *period >= MIN_POLL)
}

/// One entry of `sources`: a string or a table.
enum RawSource {
    Plain(String),
    Table(RawSourceTable),
}

// Hand-written because `#[serde(untagged)]` would drop TOML positions and
// report every mistake as "data did not match any variant".
impl<'de> Deserialize<'de> for RawSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<RawSource, D::Error> {
        struct SourceVisitor;

        impl<'de> Visitor<'de> for SourceVisitor {
            type Value = RawSource;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string or a table with `dir`, `file` or `url`")
            }

            fn visit_str<E>(self, text: &str) -> Result<RawSource, E> {
                Ok(RawSource::Plain(text.to_owned()))
            }

            fn visit_map<A: MapAccess<'de>>(self, table: A) -> Result<RawSource, A::Error> {
                RawSourceTable::deserialize(MapAccessDeserializer::new(table)).map(RawSource::Table)
            }
        }

        deserializer.deserialize_any(SourceVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSourceTable {
    dir: Option<String>,
    file: Option<String>,
    url: Option<String>,
    recursive: Option<bool>,
    hidden: Option<bool>,
    follow_symlinks: Option<bool>,
    poll: Option<String>,
}

impl RawSourceTable {
    fn resolve(&self, ctx: &ParseContext<'_>) -> Result<Source, String> {
        if self.dir.is_none() && self.has_dir_options() {
            return Err(
                "`recursive`, `hidden`, `follow_symlinks` and `poll` are only valid with `dir`"
                    .to_owned(),
            );
        }
        match (&self.dir, &self.file, &self.url) {
            (Some(dir), None, None) => Ok(Source::Dir {
                path: local_path(dir, ctx)?,
                options: self.dir_options()?,
            }),
            (None, Some(file), None) => local_path(file, ctx).map(Source::File),
            (None, None, Some(url)) => remote_url(url).map(Source::Url),
            _ => Err("a source table needs exactly one of `dir`, `file` or `url`".to_owned()),
        }
    }

    fn has_dir_options(&self) -> bool {
        self.recursive.is_some()
            || self.hidden.is_some()
            || self.follow_symlinks.is_some()
            || self.poll.is_some()
    }

    fn dir_options(&self) -> Result<DirOptions, String> {
        let defaults = DirOptions::default();
        let poll = match &self.poll {
            Some(text) => Some(poll_period(text).ok_or_else(|| {
                "poll must be a duration of at least 1s, such as \"5m\"".to_owned()
            })?),
            None => defaults.poll,
        };
        Ok(DirOptions {
            recursive: self.recursive.unwrap_or(defaults.recursive),
            hidden: self.hidden.unwrap_or(defaults.hidden),
            follow_symlinks: self.follow_symlinks.unwrap_or(defaults.follow_symlinks),
            poll,
        })
    }
}

fn resolve_source(raw: Spanned<RawSource>, ctx: &ParseContext<'_>) -> Result<Source, Invalid> {
    let offset = raw.span().start;
    let resolved = match raw.into_inner() {
        RawSource::Plain(text) => plain_source(&text, ctx),
        RawSource::Table(table) => table.resolve(ctx),
    };
    resolved.map_err(|message| Invalid { offset, message })
}

/// Classifies a plain string by its prefix.
fn plain_source(text: &str, ctx: &ParseContext<'_>) -> Result<Source, String> {
    let Some(scheme) = url_scheme(text) else {
        return expand_path(text, ctx).map(Source::Path);
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => remote_url(text).map(Source::Url),
        "file" => file_uri_path(text).map(Source::Path),
        _ => Err(format!(
            "unsupported URL scheme `{scheme}`: use http, https or file"
        )),
    }
}

/// The path of a `dir` or `file` table: anything a plain string may be,
/// except a remote URL.
fn local_path(text: &str, ctx: &ParseContext<'_>) -> Result<PathBuf, String> {
    match plain_source(text, ctx)? {
        Source::Path(path) => Ok(path),
        _ => Err("expected a local path, found a remote URL".to_owned()),
    }
}

/// The scheme of text shaped like `scheme://…`.
fn url_scheme(text: &str) -> Option<&str> {
    let (scheme, _) = text.split_once("://")?;
    let is_scheme_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
    let is_valid =
        scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(is_scheme_char);
    is_valid.then_some(scheme)
}

fn remote_url(text: &str) -> Result<Url, String> {
    // For http and https the parser itself refuses a missing host.
    let url = Url::parse(text).map_err(|err| format!("invalid URL: {err}"))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(url)
    } else {
        Err(format!(
            "unsupported URL scheme `{}`: a remote source must be http or https",
            url.scheme()
        ))
    }
}

fn file_uri_path(text: &str) -> Result<PathBuf, String> {
    Url::parse(text)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .ok_or_else(|| {
            "a file:// URI must hold an absolute local path, such as file:///srv/wall.png"
                .to_owned()
        })
}

/// Expands `~`, `$VAR` and `${VAR}`, then makes the path absolute.
fn expand_path(text: &str, ctx: &ParseContext<'_>) -> Result<PathBuf, String> {
    let (mut expanded, rest) = expand_tilde(text, ctx.home_dir)?;
    expanded.push(expand_variables(rest, ctx.env)?);
    // Checked after the expansion: a variable that is set to nothing would
    // otherwise turn the source into the config directory.
    if expanded.is_empty() {
        return Err("a path must not be empty".to_owned());
    }
    Ok(ctx.config_dir.join(expanded))
}

/// Splits a leading `~`, alone or before `/`, off `text` as `home_dir`.
/// Returns the expanded start, empty without a `~`, and the rest.
fn expand_tilde<'t>(text: &'t str, home_dir: &Path) -> Result<(OsString, &'t str), String> {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            Ok((home_dir.as_os_str().to_owned(), rest))
        }
        Some(_) => Err("`~user` is not expanded: only `~` and `~/…` are; \
                        for a file named like that, write `./~…`"
            .to_owned()),
        None => Ok((OsString::new(), text)),
    }
}

fn expand_variables(
    text: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<OsString, String> {
    let mut expanded = OsString::new();
    let mut rest = text;
    while let Some((literal, after_dollar)) = rest.split_once('$') {
        expanded.push(literal);
        match split_variable(after_dollar)? {
            Some((name, tail)) => {
                let value =
                    env(name).ok_or_else(|| format!("environment variable `{name}` is not set"))?;
                expanded.push(value);
                rest = tail;
            }
            None => {
                expanded.push("$");
                rest = after_dollar;
            }
        }
    }
    expanded.push(rest);
    Ok(expanded)
}

/// Splits the text after a `$` into a variable name and what follows it.
/// `None` means no name starts here, so the `$` stands for itself.
fn split_variable(after_dollar: &str) -> Result<Option<(&str, &str)>, String> {
    if let Some(braced) = after_dollar.strip_prefix('{') {
        return match braced.split_once('}') {
            Some(("", _)) => Err("`${}` names no variable".to_owned()),
            Some(name_and_tail) => Ok(Some(name_and_tail)),
            None => Err("`${` has no closing `}`".to_owned()),
        };
    }
    let name_len = after_dollar
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after_dollar.len());
    let has_name = name_len > 0 && !after_dollar.starts_with(|c: char| c.is_ascii_digit());
    Ok(has_name.then(|| after_dollar.split_at(name_len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_DIR: &str = "/dotfiles/gpaperd";
    const HOME_DIR: &str = "/home/ada";

    /// Parses with a fixed context: `$WALLS` is `/mnt/walls`, `$EMPTY` is set
    /// to nothing, and no other variable is set.
    fn parse(text: &str) -> Result<Config, ConfigError> {
        let env = |name: &str| match name {
            "WALLS" => Some(OsString::from("/mnt/walls")),
            "EMPTY" => Some(OsString::new()),
            _ => None,
        };
        let ctx = ParseContext {
            config_dir: Path::new(CONFIG_DIR),
            home_dir: Path::new(HOME_DIR),
            env: &env,
        };
        Config::parse(text, &ctx)
    }

    /// The sources of `[light]` when its `sources` list is `sources_toml`.
    fn light_sources(sources_toml: &str) -> Vec<Source> {
        let text = format!("[light]\nsources = {sources_toml}\n");
        parse(&text).expect("config parses").styles.light.sources
    }

    fn source_error(sources_toml: &str) -> ConfigError {
        let text = format!("[light]\nsources = {sources_toml}\n");
        parse(&text).expect_err("config is rejected")
    }

    fn path_source(path: &str) -> Source {
        Source::Path(PathBuf::from(path))
    }

    fn url(text: &str) -> Url {
        Url::parse(text).expect("test URL parses")
    }

    #[test]
    fn empty_text_is_the_default_config() {
        assert_eq!(parse(""), Ok(Config::default()));
    }

    #[test]
    fn plan_example_parses() {
        let text = r#"
[general]
interval = "15m"                 # 1m ..= 7d
order = "random"                 # "random" | "alphabetical"
picture_options = "zoom"         # optional; omit to leave GNOME's value alone
on_external_change = "pause"     # "pause" | "reassert-next-tick" | "reassert-now"
rotate_hidden = false            # rotate the style that is not on screen
empty_style = "use-other-style"  # or "keep-current"
remote_refresh = "startup"       # "startup" | a duration like "24h" | "never"
link = true                      # hardlink or reflink when possible; false = always copy

[light]
sources = [
  "~/Pictures/Wallpapers/light",                         # dir or file, decided at scan time
  { dir = "~/Pictures/Wallpapers/shared", recursive = true },
  { file = "~/Pictures/hero.jxl" },
  "https://example.com/wallpapers/dawn.jpg",
]

[dark]
interval = "30m"                 # per-style override
order = "alphabetical"
sources = [
  { dir = "/mnt/nas/wallpapers/dark", poll = "5m" },     # network fs: no inotify
  { url = "https://example.com/wallpapers/night.png" },
  "file:///srv/wall/aurora%20night.png",
]
"#;

        let config = parse(text).expect("plan example parses");

        let expected = Config {
            general: General {
                picture_options: Some(PictureOptions::Zoom),
                ..General::default()
            },
            styles: PerStyle {
                light: StyleConfig {
                    interval: Duration::from_secs(15 * 60),
                    order: Order::Random,
                    sources: vec![
                        path_source("/home/ada/Pictures/Wallpapers/light"),
                        Source::Dir {
                            path: PathBuf::from("/home/ada/Pictures/Wallpapers/shared"),
                            options: DirOptions {
                                recursive: true,
                                ..DirOptions::default()
                            },
                        },
                        Source::File(PathBuf::from("/home/ada/Pictures/hero.jxl")),
                        Source::Url(url("https://example.com/wallpapers/dawn.jpg")),
                    ],
                },
                dark: StyleConfig {
                    interval: Duration::from_secs(30 * 60),
                    order: Order::Alphabetical,
                    sources: vec![
                        Source::Dir {
                            path: PathBuf::from("/mnt/nas/wallpapers/dark"),
                            options: DirOptions {
                                poll: Some(Duration::from_secs(5 * 60)),
                                ..DirOptions::default()
                            },
                        },
                        Source::Url(url("https://example.com/wallpapers/night.png")),
                        path_source("/srv/wall/aurora night.png"),
                    ],
                },
            },
        };
        assert_eq!(config, expected);
    }

    #[test]
    fn general_keys_replace_their_defaults() {
        let text = r#"
[general]
picture_options = "spanned"
on_external_change = "reassert-next-tick"
rotate_hidden = true
empty_style = "keep-current"
remote_refresh = "24h"
link = false
apply_mode = "uri"
max_pixels = 50_000_000
pause_when_locked = true
favorites_dir = "~/Pictures/Favorites"
on_change = ["~/bin/wallpaper-hook", "--quiet"]
"#;

        let general = parse(text).expect("config parses").general;

        let expected = General {
            picture_options: Some(PictureOptions::Spanned),
            on_external_change: ExternalChangePolicy::ReassertNextTick,
            rotate_hidden: true,
            empty_style: EmptyStylePolicy::KeepCurrent,
            remote_refresh: RemoteRefresh::Every(Duration::from_secs(24 * 60 * 60)),
            link: LinkPolicy::AlwaysCopy,
            apply_mode: ApplyMode::Uri,
            max_pixels: 50_000_000,
            pause_when_locked: true,
            favorites_dir: Some(PathBuf::from("/home/ada/Pictures/Favorites")),
            on_change: Some(HookCommand {
                program: PathBuf::from("/home/ada/bin/wallpaper-hook"),
                args: vec!["--quiet".to_owned()],
            }),
        };
        assert_eq!(general, expected);
    }

    #[test]
    fn apply_mode_is_slots_unless_set() {
        let unset = parse("").expect("config parses").general;
        let slots = parse("[general]\napply_mode = \"slots\"\n")
            .expect("config parses")
            .general;

        assert_eq!(unset.apply_mode, ApplyMode::Slots);
        assert_eq!(slots.apply_mode, ApplyMode::Slots);
    }

    #[test]
    fn unknown_apply_mode_is_rejected() {
        let err = parse("[general]\napply_mode = \"xml\"\n").expect_err("unknown mode");

        assert_eq!((err.line, err.column), (2, 14));
    }

    #[test]
    fn pause_when_locked_is_off_unless_set() {
        let general = parse("").expect("config parses").general;

        assert!(!general.pause_when_locked);
    }

    #[test]
    fn pause_when_locked_that_is_no_bool_is_rejected() {
        let err =
            parse("[general]\npause_when_locked = \"yes\"\n").expect_err("a string is no bool");

        assert_eq!((err.line, err.column), (2, 21));
    }

    #[test]
    fn pause_when_locked_in_a_style_table_is_rejected() {
        let err = parse("[dark]\npause_when_locked = true\n").expect_err("the key is general only");

        assert_eq!((err.line, err.column), (2, 1));
    }

    #[test]
    fn favorites_dir_is_unset_unless_set() {
        let general = parse("").expect("config parses").general;

        assert_eq!(general.favorites_dir, None);
    }

    #[test]
    fn favorites_dir_is_expanded_as_a_source_path_is() {
        for (written, expanded) in [
            ("$WALLS/favs", "/mnt/walls/favs"),
            ("favs", "/dotfiles/gpaperd/favs"),
            ("file:///srv/my%20favs", "/srv/my favs"),
        ] {
            let text = format!("[general]\nfavorites_dir = {written:?}\n");
            let general = parse(&text).expect("config parses").general;
            assert_eq!(
                general.favorites_dir,
                Some(PathBuf::from(expanded)),
                "{written}"
            );
        }
    }

    #[test]
    fn favorites_dir_that_is_no_local_path_is_rejected() {
        for (written, message) in [
            (
                "https://example.com/favs",
                "favorites_dir: expected a local path, found a remote URL",
            ),
            (
                "$NOPE/favs",
                "favorites_dir: environment variable `NOPE` is not set",
            ),
            ("$EMPTY", "favorites_dir: a path must not be empty"),
        ] {
            let text = format!("[general]\nfavorites_dir = {written:?}\n");
            let err = parse(&text).expect_err("favorites_dir is rejected");
            assert_eq!((err.line, err.column), (2, 17), "{written}");
            assert_eq!(err.message, message, "{written}");
        }
    }

    #[test]
    fn favorites_dir_in_a_style_table_is_rejected() {
        let err = parse("[light]\nfavorites_dir = \"/favs\"\n").expect_err("general only");

        assert_eq!((err.line, err.column), (2, 1));
    }

    #[test]
    fn on_change_is_unset_unless_set() {
        let general = parse("").expect("config parses").general;

        assert_eq!(general.on_change, None);
    }

    #[test]
    fn on_change_takes_a_program_and_its_arguments_as_written() {
        let text = "[general]\non_change = [\"notify-send\", \"-a\", \"$WALLS\", \"~/x\"]\n";

        let general = parse(text).expect("config parses").general;

        let expected = HookCommand {
            program: PathBuf::from("notify-send"),
            args: vec!["-a".to_owned(), "$WALLS".to_owned(), "~/x".to_owned()],
        };
        assert_eq!(general.on_change, Some(expected));
    }

    #[test]
    fn on_change_expands_a_tilde_in_the_program_only() {
        let text = "[general]\non_change = [\"~/bin/hook\", \"~/arg\"]\n";

        let general = parse(text).expect("config parses").general;

        let expected = HookCommand {
            program: PathBuf::from("/home/ada/bin/hook"),
            args: vec!["~/arg".to_owned()],
        };
        assert_eq!(general.on_change, Some(expected));
    }

    #[test]
    fn on_change_with_an_absolute_program_keeps_it() {
        let text = "[general]\non_change = [\"/usr/bin/true\"]\n";

        let general = parse(text).expect("config parses").general;

        let expected = HookCommand {
            program: PathBuf::from("/usr/bin/true"),
            args: Vec::new(),
        };
        assert_eq!(general.on_change, Some(expected));
    }

    #[test]
    fn on_change_without_a_usable_program_is_rejected() {
        for (written, message) in [
            (
                "[]",
                "on_change must be a non-empty array of strings, such as [\"notify-send\", \"Wallpaper changed\"]",
            ),
            ("[\"\"]", "on_change: the program must not be empty"),
            (
                "[\"~\"]",
                "on_change: the program must name a file, not a directory",
            ),
            (
                "[\"~/\"]",
                "on_change: the program must name a file, not a directory",
            ),
            (
                "[\"/usr/bin/\"]",
                "on_change: the program must name a file, not a directory",
            ),
            (
                "[\"bin/hook\"]",
                "on_change: a program path must be absolute or start with `~/`; \
                 a bare name is looked up in PATH",
            ),
            (
                "[\"~bob/hook\"]",
                "on_change: `~user` is not expanded: only `~` and `~/…` are; \
                 for a file named like that, write `./~…`",
            ),
        ] {
            let text = format!("[general]\non_change = {written}\n");
            let err = parse(&text).expect_err("on_change is rejected");
            assert_eq!((err.line, err.column), (2, 13), "{written}");
            assert_eq!(err.message, message, "{written}");
        }
    }

    #[test]
    fn on_change_that_is_no_array_of_strings_is_rejected_where_it_stands() {
        for (written, column) in [("\"notify-send\"", 13), ("[\"notify-send\", 3]", 29)] {
            let text = format!("[general]\non_change = {written}\n");
            let err = parse(&text).expect_err("on_change is rejected");
            assert_eq!((err.line, err.column), (2, column), "{written}");
        }
    }

    #[test]
    fn on_change_in_a_style_table_is_rejected() {
        let err = parse("[dark]\non_change = [\"true\"]\n").expect_err("general only");

        assert_eq!((err.line, err.column), (2, 1));
    }

    #[test]
    fn max_pixels_is_one_hundred_megapixels_unless_set() {
        let general = parse("").expect("config parses").general;

        assert_eq!(general.max_pixels, 100_000_000);
    }

    #[test]
    fn max_pixels_accepts_any_count_above_zero() {
        for (written, pixels) in [("1", 1), ("9223372036854775807", i64::MAX.unsigned_abs())] {
            let config = parse(&format!("[general]\nmax_pixels = {written}\n"))
                .unwrap_or_else(|err| panic!("{written} is accepted: {err}"));
            assert_eq!(config.general.max_pixels, pixels);
        }
    }

    #[test]
    fn max_pixels_of_zero_or_less_is_rejected() {
        for written in ["0", "-1"] {
            let err = parse(&format!("[general]\nmax_pixels = {written}\n"))
                .expect_err("max_pixels is out of range");
            assert_eq!((err.line, err.column), (2, 14), "{written}");
            assert_eq!(
                err.message,
                "max_pixels must be a whole number of pixels greater than zero, \
                 such as 100000000"
            );
        }
    }

    #[test]
    fn max_pixels_that_is_no_integer_is_rejected() {
        for written in ["\"100MP\"", "1.5"] {
            let err = parse(&format!("[general]\nmax_pixels = {written}\n"))
                .expect_err("max_pixels is no integer");
            assert_eq!((err.line, err.column), (2, 14), "{written}");
        }
    }

    #[test]
    fn styles_inherit_general_interval_and_order() {
        let text = "[general]\ninterval = \"2m\"\norder = \"alphabetical\"\n";

        let styles = parse(text).expect("config parses").styles;

        for style in [styles.light, styles.dark] {
            assert_eq!(style.interval, Duration::from_secs(120));
            assert_eq!(style.order, Order::Alphabetical);
        }
    }

    #[test]
    fn style_table_overrides_general_interval_and_order() {
        let text = r#"
[general]
interval = "2m"
order = "alphabetical"

[dark]
interval = "45m"
order = "random"
"#;

        let styles = parse(text).expect("config parses").styles;

        assert_eq!(styles.dark.interval, Duration::from_secs(45 * 60));
        assert_eq!(styles.dark.order, Order::Random);
        assert_eq!(styles.light.interval, Duration::from_secs(120));
        assert_eq!(styles.light.order, Order::Alphabetical);
    }

    #[test]
    fn interval_bounds_are_inclusive() {
        let week = 7 * 24 * 3600;
        for (written, seconds) in [
            ("1m", 60),
            ("60s", 60),
            ("1h", 3600),
            ("24h", 24 * 3600),
            ("7d", week),
            ("1w", week),
            ("6d 23h 59m 60s", week),
        ] {
            let config = parse(&format!("[general]\ninterval = \"{written}\"\n"))
                .unwrap_or_else(|err| panic!("{written} is accepted: {err}"));
            assert_eq!(config.styles.light.interval, Duration::from_secs(seconds));
        }
    }

    #[test]
    fn interval_outside_one_minute_to_seven_days_is_rejected() {
        for written in ["30s", "59s", "0s", "7d 1s", "8d", "2w", "1month"] {
            let err = parse(&format!("[general]\ninterval = \"{written}\"\n"))
                .expect_err("interval is out of range");
            assert_eq!((err.line, err.column), (2, 12), "{written}");
            assert_eq!(
                err.message,
                "interval must be a duration from 1m to 7days, such as \"15m\""
            );
        }
    }

    #[test]
    fn style_interval_is_validated_too() {
        let err = parse("[dark]\norder = \"random\"\ninterval = \"30s\"\n")
            .expect_err("interval is out of range");

        assert_eq!((err.line, err.column), (3, 12));
    }

    #[test]
    fn interval_that_is_no_duration_is_rejected() {
        let err = parse("[general]\ninterval = \"soon\"\n").expect_err("not a duration");

        assert_eq!((err.line, err.column), (2, 12));
    }

    #[test]
    fn syntax_error_carries_line_and_column() {
        let err = parse("[general]\norder = \"random\"\ninterval = 15m\n")
            .expect_err("unquoted string is a syntax error");

        assert_eq!((err.line, err.column), (3, 12));
        assert!(err.message.contains("quoted"), "{}", err.message);
        assert_eq!(
            err.to_string(),
            format!("line 3, column 12: {}", err.message)
        );
    }

    #[test]
    fn column_counts_characters_not_bytes() {
        let err = source_error(r#"["/ž/é", "ftp://example.com/a.png"]"#);

        assert_eq!((err.line, err.column), (2, 20));
    }

    #[test]
    fn unknown_general_key_is_rejected() {
        let err = parse("[general]\nlink = true\ncolour = \"red\"\n").expect_err("unknown key");

        assert_eq!((err.line, err.column), (3, 1));
        assert!(err.message.contains("`colour`"), "{}", err.message);
    }

    #[test]
    fn unknown_table_is_rejected() {
        let err = parse("[wide]\nsources = []\n").expect_err("unknown table");

        assert_eq!(err.line, 1);
        assert!(err.message.contains("`wide`"), "{}", err.message);
    }

    #[test]
    fn general_only_key_in_a_style_table_is_rejected() {
        let err = parse("[light]\npicture_options = \"zoom\"\n").expect_err("unknown key");

        assert_eq!((err.line, err.column), (2, 1));
        assert!(err.message.contains("`picture_options`"), "{}", err.message);
    }

    #[test]
    fn unknown_source_key_is_rejected() {
        let err = source_error(r#"[{ dir = "a", depth = 3 }]"#);

        assert_eq!((err.line, err.column), (2, 25));
        assert!(err.message.contains("`depth`"), "{}", err.message);
    }

    #[test]
    fn unknown_enum_value_is_rejected() {
        let err = parse("[general]\norder = \"sideways\"\n").expect_err("unknown order");

        assert_eq!((err.line, err.column), (2, 9));
    }

    #[test]
    fn picture_options_none_is_rejected() {
        let err = parse("[general]\npicture_options = \"none\"\n").expect_err("none is refused");

        assert_eq!((err.line, err.column), (2, 19));
    }

    #[test]
    fn wrong_value_type_names_no_internal_type() {
        let err = parse("general = 3\n").expect_err("general must be a table");

        assert_eq!(err.message, "invalid type: integer `3`, expected a table");
    }

    #[test]
    fn remote_refresh_keywords_parse() {
        for (written, expected) in [
            ("startup", RemoteRefresh::Startup),
            ("never", RemoteRefresh::Never),
            ("90m", RemoteRefresh::Every(Duration::from_secs(90 * 60))),
            ("1h", RemoteRefresh::Every(MIN_REMOTE_REFRESH)),
        ] {
            let config = parse(&format!("[general]\nremote_refresh = \"{written}\"\n"))
                .unwrap_or_else(|err| panic!("{written} is accepted: {err}"));
            assert_eq!(config.general.remote_refresh, expected);
        }
    }

    #[test]
    fn remote_refresh_rejects_other_words_and_zero() {
        for written in ["sometimes", "0s", "1ms", "59m 59s"] {
            let err = parse(&format!("[general]\nremote_refresh = \"{written}\"\n"))
                .expect_err("remote_refresh is invalid");
            assert_eq!((err.line, err.column), (2, 18), "{written}");
        }
    }

    #[test]
    fn style_without_sources_is_valid() {
        let config =
            parse("[light]\ninterval = \"5m\"\n\n[dark]\nsources = []\n").expect("config parses");

        assert!(config.styles.light.sources.is_empty());
        assert!(config.styles.dark.sources.is_empty());
    }

    #[test]
    fn plain_string_is_classified_by_prefix() {
        let sources = light_sources(
            r#"["http://example.com/a.jpg", "HTTPS://example.com/b.jpg", "file:///srv/c.png", "/srv/d"]"#,
        );

        assert_eq!(
            sources,
            vec![
                Source::Url(url("http://example.com/a.jpg")),
                Source::Url(url("https://example.com/b.jpg")),
                path_source("/srv/c.png"),
                path_source("/srv/d"),
            ]
        );
    }

    #[test]
    fn file_uri_is_percent_decoded_and_not_expanded() {
        let sources = light_sources(r#"["file:///srv/wall/aurora%20night%23$WALLS.png"]"#);

        assert_eq!(
            sources,
            vec![path_source("/srv/wall/aurora night#$WALLS.png")]
        );
    }

    #[test]
    fn file_uri_with_a_remote_host_is_rejected() {
        let err = source_error(r#"["file://nas/wall.png"]"#);

        assert_eq!((err.line, err.column), (2, 12));
    }

    #[test]
    fn ftp_url_is_rejected_in_every_form() {
        for sources_toml in [
            r#"["ftp://example.com/a.png"]"#,
            r#"[{ url = "ftp://example.com/a.png" }]"#,
            r#"[{ file = "ftp://example.com/a.png" }]"#,
        ] {
            let err = source_error(sources_toml);
            assert_eq!((err.line, err.column), (2, 12), "{sources_toml}");
            assert!(err.message.contains("`ftp`"), "{}", err.message);
        }
    }

    #[test]
    fn url_without_a_host_is_rejected() {
        for sources_toml in [r#"["http://"]"#, r#"[{ url = "https://" }]"#] {
            let err = source_error(sources_toml);
            assert_eq!(err.message, "invalid URL: empty host", "{sources_toml}");
        }
    }

    #[test]
    fn url_table_rejects_text_that_is_no_url() {
        let err = source_error(r#"[{ url = "example.com/a.png" }]"#);

        assert!(err.message.starts_with("invalid URL"), "{}", err.message);
    }

    #[test]
    fn tilde_expands_to_the_home_dir() {
        let sources = light_sources(r#"["~", "~/Pictures"]"#);

        assert_eq!(
            sources,
            vec![path_source("/home/ada"), path_source("/home/ada/Pictures")]
        );
    }

    #[test]
    fn tilde_expands_only_at_the_start() {
        let sources = light_sources(r#"["/srv/~/walls", "./~backup", "walls~"]"#);

        assert_eq!(
            sources,
            vec![
                path_source("/srv/~/walls"),
                path_source("/dotfiles/gpaperd/./~backup"),
                path_source("/dotfiles/gpaperd/walls~"),
            ]
        );
    }

    #[test]
    fn tilde_with_a_user_name_is_rejected() {
        let err = source_error(r#"["~bob/walls"]"#);

        assert_eq!((err.line, err.column), (2, 12));
        assert_eq!(
            err.message,
            "`~user` is not expanded: only `~` and `~/…` are; \
             for a file named like that, write `./~…`"
        );
    }

    #[test]
    fn variables_expand_in_both_spellings() {
        let sources = light_sources(r#"["$WALLS/light", "${WALLS}light", "~/$WALLS"]"#);

        assert_eq!(
            sources,
            vec![
                path_source("/mnt/walls/light"),
                path_source("/mnt/wallslight"),
                path_source("/home/ada//mnt/walls"),
            ]
        );
    }

    #[test]
    fn undefined_variable_is_an_error() {
        for sources_toml in [r#"["/srv", "$NOPE/walls"]"#, r#"["/srv", "${NOPE}/walls"]"#] {
            let err = source_error(sources_toml);
            assert_eq!((err.line, err.column), (2, 20), "{sources_toml}");
            assert_eq!(err.message, "environment variable `NOPE` is not set");
        }
    }

    #[test]
    fn dollar_that_starts_no_name_is_literal() {
        let sources = light_sources(r#"["/srv/cash$", "/srv/$5/a$-b"]"#);

        assert_eq!(
            sources,
            vec![path_source("/srv/cash$"), path_source("/srv/$5/a$-b")]
        );
    }

    #[test]
    fn malformed_braced_variable_is_an_error() {
        assert_eq!(
            source_error(r#"["${WALLS/x"]"#).message,
            "`${` has no closing `}`"
        );
        assert_eq!(
            source_error(r#"["${}/x"]"#).message,
            "`${}` names no variable"
        );
    }

    #[test]
    fn relative_path_resolves_against_the_config_dir() {
        let sources = light_sources(r#"["walls/light", "./a.png", { dir = "shared" }]"#);

        assert_eq!(
            sources,
            vec![
                path_source("/dotfiles/gpaperd/walls/light"),
                path_source("/dotfiles/gpaperd/./a.png"),
                Source::Dir {
                    path: PathBuf::from("/dotfiles/gpaperd/shared"),
                    options: DirOptions::default(),
                },
            ]
        );
    }

    #[test]
    fn empty_path_is_rejected() {
        assert_eq!(source_error(r#"[""]"#).message, "a path must not be empty");
    }

    #[test]
    fn path_that_expands_to_nothing_is_rejected() {
        for sources_toml in [r#"["/srv", "$EMPTY"]"#, r#"["/srv", "${EMPTY}"]"#] {
            let err = source_error(sources_toml);
            assert_eq!((err.line, err.column), (2, 20), "{sources_toml}");
            assert_eq!(err.message, "a path must not be empty");
        }
    }

    #[test]
    fn dir_table_reads_every_option() {
        let sources = light_sources(
            r#"[{ dir = "$WALLS", recursive = true, hidden = true, follow_symlinks = false, poll = "90s" }]"#,
        );

        let expected = Source::Dir {
            path: PathBuf::from("/mnt/walls"),
            options: DirOptions {
                recursive: true,
                hidden: true,
                follow_symlinks: false,
                poll: Some(Duration::from_secs(90)),
            },
        };
        assert_eq!(sources, vec![expected]);
    }

    #[test]
    fn dir_and_file_tables_accept_a_file_uri() {
        let sources =
            light_sources(r#"[{ dir = "file:///srv/my%20walls" }, { file = "~/a.png" }]"#);

        assert_eq!(
            sources,
            vec![
                Source::Dir {
                    path: PathBuf::from("/srv/my walls"),
                    options: DirOptions::default(),
                },
                Source::File(PathBuf::from("/home/ada/a.png")),
            ]
        );
    }

    #[test]
    fn dir_table_rejects_a_remote_url() {
        let err = source_error(r#"[{ dir = "https://example.com/walls" }]"#);

        assert_eq!(err.message, "expected a local path, found a remote URL");
    }

    #[test]
    fn poll_must_be_a_duration_of_at_least_a_second() {
        for written in ["0s", "5ms", "999ms", "often"] {
            let err = source_error(&format!(r#"[{{ dir = "a", poll = "{written}" }}]"#));
            assert_eq!(
                err.message, "poll must be a duration of at least 1s, such as \"5m\"",
                "{written}"
            );
        }
    }

    #[test]
    fn poll_of_exactly_a_second_is_accepted() {
        let sources = light_sources(r#"[{ dir = "/walls", poll = "1s" }]"#);

        let expected = Source::Dir {
            path: PathBuf::from("/walls"),
            options: DirOptions {
                poll: Some(MIN_POLL),
                ..DirOptions::default()
            },
        };
        assert_eq!(sources, vec![expected]);
    }

    #[test]
    fn source_table_needs_exactly_one_kind() {
        for sources_toml in [
            "[{}]",
            r#"[{ dir = "a", file = "b" }]"#,
            r#"[{ dir = "a", file = "b", recursive = true }]"#,
            r#"[{ file = "b", url = "https://example.com/a.png" }]"#,
        ] {
            let err = source_error(sources_toml);
            assert_eq!(
                err.message, "a source table needs exactly one of `dir`, `file` or `url`",
                "{sources_toml}"
            );
            assert_eq!((err.line, err.column), (2, 12), "{sources_toml}");
        }
    }

    #[test]
    fn dir_options_need_a_dir() {
        for sources_toml in [
            r#"[{ recursive = true }]"#,
            r#"[{ file = "a.png", recursive = true }]"#,
            r#"[{ url = "https://example.com/a.png", poll = "5m" }]"#,
        ] {
            let err = source_error(sources_toml);
            assert_eq!(
                err.message,
                "`recursive`, `hidden`, `follow_symlinks` and `poll` are only valid with `dir`",
                "{sources_toml}"
            );
        }
    }

    #[test]
    fn source_of_another_type_is_rejected_where_it_stands() {
        let err = source_error("[\n  \"a\",\n  42,\n]");

        assert_eq!((err.line, err.column), (4, 3));
    }

    #[test]
    fn array_of_tables_spelling_is_accepted() {
        let text = "[[dark.sources]]\ndir = \"/srv/walls\"\nhidden = true\n";

        let sources = parse(text).expect("config parses").styles.dark.sources;

        let expected = Source::Dir {
            path: PathBuf::from("/srv/walls"),
            options: DirOptions {
                hidden: true,
                ..DirOptions::default()
            },
        };
        assert_eq!(sources, vec![expected]);
    }
}
