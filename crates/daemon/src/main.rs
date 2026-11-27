//! The `gpaperd` binary: command line, config file, signals, exit codes,
//! systemd notifications and the D-Bus control interface around the
//! interpreter.
//!
//! The bus name comes first: a second gpaperd finds it owned, says so and
//! exits 0 before it reads the config or touches a slot or a key.

mod check;

use std::env;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::{Parser, Subcommand};
use gpaperd_core::config::Config;
use gpaperd_core::control::BUS_NAME;
use gpaperd_core::event::{Pause, ScreenLock, Status};
use gpaperd_core::style::Style;
use gpaperd_daemon::config_watch::{ConfigFile, ConfigWatch, LoadError};
use gpaperd_daemon::dbus::{self, ControlPort, OwnError};
use gpaperd_daemon::fetch::{self, FetchSettings};
use gpaperd_daemon::gnome::{GnomeError, GnomeThread};
use gpaperd_daemon::interp::{Interpreter, Notice, Ports};
use gpaperd_daemon::logging;
use gpaperd_daemon::paths::Paths;
use gpaperd_daemon::probe::Prober;
use gpaperd_daemon::screensaver;
use gpaperd_daemon::timer::WallTimer;
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

// The unit file lists both codes in `RestartPreventExitStatus`: a restart
// cannot help.
/// `EX_UNAVAILABLE`: not a GNOME session, or settings that would not persist.
const EXIT_UNAVAILABLE: u8 = 69;
/// `EX_CONFIG`: the config file cannot be used.
const EXIT_CONFIG: u8 = 78;
/// Any other failure. systemd restarts the service after it.
const EXIT_FAILURE: u8 = 1;

/// How long systemd waits for `READY=1` once the GNOME session is opened:
/// five minutes. The unit's `TimeoutStartSec` alone would fail a start whose
/// session bus is slow to answer, or whose first image comes from a slow
/// network mount.
const FIRST_APPLY_ALLOWANCE_USEC: u32 = 5 * 60 * 1_000_000;

/// Rotates the wallpapers of GNOME's light and dark styles.
#[derive(Debug, Parser)]
#[command(name = "gpaperd", version)]
struct Cli {
    /// Read this config file instead of $XDG_CONFIG_HOME/gpaperd/config.toml
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,
    /// Without one, gpaperd runs as the daemon.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check the config file, count the images of each source, and exit.
    /// Touches neither GNOME, the session bus, the slots nor the state file.
    /// Exits 0 when valid, 66 when the file is missing, 78 when it is invalid.
    CheckConfig,
}

/// Why gpaperd ends with an error.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("config file {}: {reason}", file.display())]
    Config { file: PathBuf, reason: LoadError },
    #[error("{0}")]
    NotGnome(GnomeError),
    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

impl Failure {
    fn exit_code(&self) -> u8 {
        match self {
            // Most I/O on a file that exists can clear. Exit 1 so systemd restarts.
            // Bytes that are not UTF-8 (InvalidData) never clear: they fall through
            // to 78, which stays in RestartPreventExitStatus, as check-config does.
            Failure::Config {
                reason: LoadError::Io(error),
                ..
            } if error.kind() != io::ErrorKind::InvalidData => EXIT_FAILURE,
            Failure::Config { .. } => EXIT_CONFIG,
            Failure::NotGnome(_) => EXIT_UNAVAILABLE,
            Failure::Other(_) => EXIT_FAILURE,
        }
    }
}

impl From<GnomeError> for Failure {
    fn from(error: GnomeError) -> Failure {
        match error {
            GnomeError::SchemaMissing { .. } | GnomeError::VolatileBackend { .. } => {
                Failure::NotGnome(error)
            }
            GnomeError::Write { .. } | GnomeError::NoAnswer | GnomeError::Stopped => {
                Failure::Other(error.into())
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init();
    if let Some(Command::CheckConfig) = cli.command {
        return check::run(cli.config.as_deref());
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            tracing::error!("{failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

fn run(cli: &Cli) -> Result<(), Failure> {
    let paths = Paths::resolve(&environment, cli.config.as_deref())
        .context("cannot locate gpaperd's directories")?;
    // The timers are for glycin's tokio backend, for the config watch's
    // debounce and for the D-Bus replies. The daemon's own deadlines stay
    // on the timerfd.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("cannot start the async runtime")?;
    let outcome = runtime.block_on(start(paths));
    // A probe or a publish that hangs on a dead network mount must not hold
    // up the exit.
    runtime.shutdown_background();
    outcome
}

/// Takes the bus name, loads the config and runs the daemon. Returns at
/// once when another gpaperd owns the name.
async fn start(paths: Paths) -> Result<(), Failure> {
    // Dropping the connection gives up the name: it lives until the end.
    let Some((connection, control)) = control_interface().await else {
        return Ok(());
    };
    let screen_lock = connection
        .clone()
        .map_or_else(screensaver::detached, screensaver::watch);
    let file = config_file(&paths);
    // Before the first load: a change between the two is then seen.
    let config_watch = ConfigWatch::start(file.clone());
    let config = load_config(&file)?;
    serve(config, config_watch, paths, control, screen_lock).await
}

/// Serves the control interface on the session bus. `None` when another
/// gpaperd owns the name. A bus that cannot be reached is logged, and the
/// daemon runs without the control interface. So does a bus that goes away
/// later: the daemon keeps rotating, and GLib's own session connection is
/// kept from stopping it (see `gnome.rs`).
async fn control_interface() -> Option<(Option<zbus::Connection>, ControlPort)> {
    match dbus::serve(zbus::connection::Builder::session()).await {
        Ok((connection, control)) => Some((Some(connection), control)),
        Err(OwnError::AlreadyOwned) => {
            tracing::info!("gpaperd is already running in this session: exiting");
            // A `Type=notify` unit whose process exits before `READY=1`
            // failed, and `Restart=on-failure` would retry it until the
            // start limit. This exit is no failure.
            let status = format!("another gpaperd owns {BUS_NAME}");
            notify_systemd(&[NotifyState::Ready, NotifyState::Status(&status)]);
            None
        }
        Err(OwnError::Bus(error)) => {
            tracing::warn!(reason = %error, "session bus unavailable: running without gpaperctl control");
            Some((None, ControlPort::detached()))
        }
    }
}

fn environment(name: &str) -> Option<OsString> {
    env::var_os(name)
}

fn config_file(paths: &Paths) -> ConfigFile {
    ConfigFile {
        path: paths.config_file.clone(),
        home_dir: paths.home_dir.clone(),
        env: environment,
    }
}

/// Reads and validates the config file. A missing file is no failure: it
/// gives `None`, and the daemon idles until the file appears.
fn load_config(file: &ConfigFile) -> Result<Option<Config>, Failure> {
    match file.load() {
        Ok(Some(config)) => Ok(Some(config)),
        Ok(None) => {
            tracing::warn!(
                path = %file.path.display(),
                "no config file: nothing rotates until it appears"
            );
            Ok(None)
        }
        Err(reason) => Err(Failure::Config {
            file: file.path.clone(),
            reason,
        }),
    }
}

/// Runs the daemon until `SIGTERM` or `SIGINT`.
async fn serve(
    config: Option<Config>,
    config_watch: ConfigWatch,
    paths: Paths,
    control: ControlPort,
    screen_lock: mpsc::UnboundedReceiver<ScreenLock>,
) -> Result<(), Failure> {
    let shutdown = stop_request().context("cannot listen for termination signals")?;
    let mut shutdown = pin!(shutdown);
    let (gnome_sender, gnome_events) = mpsc::unbounded_channel();
    // Before `GnomeThread::spawn`: its bounded session-bus open alone may
    // outlast the unit's `TimeoutStartSec`.
    notify_systemd(&[NotifyState::ExtendTimeoutUsec(FIRST_APPLY_ALLOWANCE_USEC)]);
    let ports = Ports {
        gnome: GnomeThread::spawn(gnome_sender)?,
        clock: WallTimer::new().context("cannot create the wall-clock timer")?,
        notify: Box::new(tell_service_manager),
        fetch: FetchSettings::default(),
        control,
        screen_lock,
        image_check: Arc::new(Prober::new()),
    };
    let start = Interpreter::start(config, paths, seed_from_clock(), ports);
    let started = tokio::select! {
        interpreter = start => Some(interpreter.context("cannot start")?),
        () = &mut shutdown => None,
    };
    // `None`: asked to stop while the saved state loaded. Nothing ran yet,
    // so there is nothing to save.
    if let Some(interpreter) = started {
        let outcome = interpreter.run(gnome_events, config_watch, shutdown).await;
        outcome.context("event loop ended")?;
    }
    tracing::info!("stopped");
    Ok(())
}

/// A future that completes on `SIGTERM` or `SIGINT`, after it has told
/// systemd `STOPPING=1`.
///
/// The handlers are installed by this call, not by the first poll, so a
/// signal that arrives before the event loop runs is not lost.
fn stop_request() -> io::Result<impl Future<Output = ()>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("termination signal received");
        notify_systemd(&[NotifyState::Stopping]);
    })
}

/// A seed that differs from run to run. A shuffle needs no more than that.
fn seed_from_clock() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    since_epoch.as_secs() ^ u64::from(since_epoch.subsec_nanos())
}

fn tell_service_manager(notice: Notice) {
    match notice {
        Notice::Ready => {
            tracing::info!("ready");
            notify_systemd(&[NotifyState::Ready]);
        }
        Notice::Status(status) => {
            let summary = status_line(&status);
            tracing::info!("{summary}");
            notify_systemd(&[NotifyState::Status(&summary)]);
        }
    }
}

/// Sends state changes to systemd. Does nothing when gpaperd does not run as
/// a service (no `NOTIFY_SOCKET`).
fn notify_systemd(states: &[NotifyState]) {
    if let Err(error) = sd_notify::notify(states) {
        tracing::warn!(reason = %error, "systemd not notified");
    }
}

/// The one-line summary for `STATUS=` and the log: the style on screen, the
/// image in each slot (a remote one without the URL's query), the time of the next change, and each style that
/// does not rotate with the reason.
///
/// A file name may hold any character. Each control character becomes `?`:
/// a line break in `STATUS=` would start another `sd_notify` field.
fn status_line(status: &Status) -> String {
    let shown = |style: Style| match &status.current[style] {
        Some(item) => file_name(&fetch::loggable(item)).to_owned(),
        None => "nothing".to_owned(),
    };
    let next_change = match status.next_change {
        Some(due) => format!("next change at {}", deadline_text(due)),
        None => "no change scheduled".to_owned(),
    };
    let mut summary = format!(
        "{} style on screen; light: {}; dark: {}; {next_change}",
        status.visible,
        shown(Style::Light),
        shown(Style::Dark),
    );
    for style in Style::ALL {
        if let Some(pause) = status.paused[style] {
            summary.push_str(&format!("; {style} paused: {}", pause_reason(pause)));
        }
    }
    summary.replace(char::is_control, "?")
}

/// `humantime` panics on a time before the Unix epoch and on year 10000 or
/// later (`format!` turns its format error into a panic). D-Bus already
/// maps a pre-epoch time to `0`. The same number is printed here. A later
/// time is the raw unix seconds, still without `humantime`.
const HUMANTIME_LIMIT_SECS: u64 = 253_402_300_800;

fn deadline_text(due: SystemTime) -> String {
    match due.duration_since(UNIX_EPOCH) {
        Ok(since) if since.as_secs() < HUMANTIME_LIMIT_SECS => {
            humantime::format_rfc3339_seconds(due).to_string()
        }
        Ok(since) => since.as_secs().to_string(),
        Err(_) => "0".to_owned(),
    }
}

fn pause_reason(pause: Pause) -> &'static str {
    match pause {
        Pause::User => "paused by gpaperctl",
        Pause::ExternalChange => "something else changed its key",
        Pause::LockedKey => "its key is locked on another value",
        Pause::NoKey => "GNOME has no key for it",
        Pause::ScreenLocked => "the screen is locked",
    }
}

/// The last segment of an item id, which is a path.
fn file_name(item: &str) -> &str {
    item.rsplit_once('/').map_or(item, |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use gpaperd_core::catalog::ItemId;
    use gpaperd_core::config::{ParseContext, Source};
    use gpaperd_core::style::PerStyle;
    use tempfile::TempDir;

    use super::*;

    /// Paths whose config file is `config.toml` in a new temp directory.
    fn paths_in_temp_dir() -> (TempDir, Paths) {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            slots_dir: dir.path().join("slots"),
            state_file: dir.path().join("state.json"),
            remote_cache_dir: dir.path().join("remote"),
            home_dir: dir.path().to_path_buf(),
        };
        (dir, paths)
    }

    #[test]
    fn missing_config_file_gives_no_config_and_no_failure() {
        let (_dir, paths) = paths_in_temp_dir();

        let config = load_config(&config_file(&paths)).expect("a missing file is no failure");

        assert_eq!(config, None);
    }

    #[test]
    fn invalid_config_file_fails_with_its_position_and_exit_code_78() {
        let (_dir, paths) = paths_in_temp_dir();
        fs::write(&paths.config_file, "[general]\ninterval = \"30s\"\n").expect("write the config");

        let failure = load_config(&config_file(&paths)).expect_err("30s is below the minimum");

        assert_eq!(failure.exit_code(), 78);
        let message = failure.to_string();
        assert!(
            message.contains("config.toml: line 2, column 12"),
            "{message}"
        );
    }

    #[test]
    fn an_unreadable_config_file_exits_1_so_systemd_can_restart() {
        let (_dir, paths) = paths_in_temp_dir();
        fs::create_dir(&paths.config_file).expect("a directory where the config would be");

        let failure =
            load_config(&config_file(&paths)).expect_err("a directory is not a config file");

        assert_eq!(failure.exit_code(), EXIT_FAILURE);
    }

    #[test]
    fn a_config_file_that_is_not_utf8_exits_78_like_check_config() {
        let (_dir, paths) = paths_in_temp_dir();
        fs::write(&paths.config_file, b"\xff").expect("write the config");

        let failure = load_config(&config_file(&paths)).expect_err("0xff is not UTF-8");

        assert_eq!(failure.exit_code(), 78);
    }

    #[test]
    fn relative_source_resolves_against_the_real_config_file() {
        let (dir, mut paths) = paths_in_temp_dir();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir(&dotfiles).expect("create the dotfiles dir");
        fs::write(
            dotfiles.join("config.toml"),
            "[light]\nsources = [\"walls\"]\n",
        )
        .expect("write the config");
        paths.config_file = dir.path().join("link.toml");
        std::os::unix::fs::symlink(dotfiles.join("config.toml"), &paths.config_file)
            .expect("link the config");

        let config = load_config(&config_file(&paths))
            .expect("a valid config")
            .expect("the config file is there");

        let walls = fs::canonicalize(&dotfiles).expect("resolve").join("walls");
        assert_eq!(config.styles.light.sources, [Source::Path(walls)]);
    }

    #[test]
    fn the_example_config_is_valid() {
        let example = include_str!("../../../dist/config.example.toml");
        let context = ParseContext {
            config_dir: Path::new("/etc/gpaperd"),
            home_dir: Path::new("/home/user"),
            env: &|_| None,
        };

        let config = Config::parse(example, &context).expect("the example to parse");

        assert_eq!(config.styles.light.sources.len(), 4);
        assert_eq!(config.styles.dark.sources.len(), 3);
    }

    #[test]
    fn a_session_that_is_not_gnome_exits_with_69() {
        let not_gnome = [
            GnomeError::SchemaMissing {
                schema: "org.gnome.desktop.background".to_owned(),
            },
            GnomeError::VolatileBackend {
                backend: "GMemorySettingsBackend".to_owned(),
            },
        ];

        for error in not_gnome {
            assert_eq!(Failure::from(error).exit_code(), 69);
        }
    }

    #[test]
    fn a_dead_settings_thread_is_an_ordinary_failure() {
        assert_eq!(Failure::from(GnomeError::Stopped).exit_code(), 1);
    }

    #[test]
    fn settings_that_do_not_answer_are_an_ordinary_failure() {
        assert_eq!(Failure::from(GnomeError::NoAnswer).exit_code(), 1);
    }

    #[test]
    fn status_line_names_both_images_and_the_next_change() {
        let status = Status {
            visible: Style::Dark,
            current: PerStyle {
                light: Some(ItemId::new("/pictures/light/dawn.jpg")),
                dark: Some(ItemId::new("/pictures/dark/night sky.png")),
            },
            next_change: Some(UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
            paused: PerStyle::default(),
        };

        assert_eq!(
            status_line(&status),
            "dark style on screen; light: dawn.jpg; dark: night sky.png; \
             next change at 2027-01-15T08:00:00Z"
        );
    }

    #[test]
    fn status_line_of_a_deadline_before_the_epoch_uses_the_dbus_fallback() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle::default(),
            next_change: Some(UNIX_EPOCH - Duration::from_secs(5)),
            paused: PerStyle::default(),
        };

        let line = status_line(&status);

        assert!(line.contains("next change at 0"), "{line}");
    }

    #[test]
    fn status_line_of_a_deadline_at_year_10000_prints_unix_seconds() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle::default(),
            next_change: Some(UNIX_EPOCH + Duration::from_secs(HUMANTIME_LIMIT_SECS)),
            paused: PerStyle::default(),
        };

        let line = status_line(&status);

        assert!(line.contains("next change at 253402300800"), "{line}");
    }

    #[test]
    fn status_line_stays_one_line_whatever_a_file_is_named() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle {
                light: Some(ItemId::new("/pictures/a\nWATCHDOG=trigger\r\nX=\0.png")),
                dark: None,
            },
            next_change: None,
            paused: PerStyle::default(),
        };

        assert_eq!(
            status_line(&status),
            "light style on screen; light: a?WATCHDOG=trigger??X=?.png; dark: nothing; \
             no change scheduled"
        );
    }

    #[test]
    fn status_line_names_a_remote_image_without_credentials_or_query() {
        let url = url::Url::parse("https://ada:hunter2@img.test/walls/dawn.jpg?token=s3cret")
            .expect("a valid URL");
        let status = Status {
            visible: Style::Light,
            current: PerStyle {
                light: Some(ItemId::from_url(&url)),
                dark: None,
            },
            next_change: None,
            paused: PerStyle::default(),
        };

        assert_eq!(
            status_line(&status),
            "light style on screen; light: dawn.jpg; dark: nothing; no change scheduled"
        );
    }

    #[test]
    fn status_line_names_each_paused_style_and_why() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle::default(),
            next_change: None,
            paused: PerStyle {
                light: Some(Pause::ExternalChange),
                dark: Some(Pause::NoKey),
            },
        };

        assert_eq!(
            status_line(&status),
            "light style on screen; light: nothing; dark: nothing; no change scheduled; \
             light paused: something else changed its key; \
             dark paused: GNOME has no key for it"
        );
    }

    #[test]
    fn status_line_of_a_style_with_a_locked_key_says_so() {
        let status = Status {
            visible: Style::Dark,
            current: PerStyle::default(),
            next_change: None,
            paused: PerStyle {
                light: None,
                dark: Some(Pause::LockedKey),
            },
        };

        assert_eq!(
            status_line(&status),
            "dark style on screen; light: nothing; dark: nothing; no change scheduled; \
             dark paused: its key is locked on another value"
        );
    }

    #[test]
    fn status_line_of_a_locked_screen_says_so() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle::default(),
            next_change: None,
            paused: PerStyle {
                light: Some(Pause::ScreenLocked),
                dark: Some(Pause::ScreenLocked),
            },
        };

        assert_eq!(
            status_line(&status),
            "light style on screen; light: nothing; dark: nothing; no change scheduled; \
             light paused: the screen is locked; dark paused: the screen is locked"
        );
    }

    #[test]
    fn status_line_of_an_idle_daemon_says_so() {
        let status = Status {
            visible: Style::Light,
            current: PerStyle::default(),
            next_change: None,
            paused: PerStyle::default(),
        };

        assert_eq!(
            status_line(&status),
            "light style on screen; light: nothing; dark: nothing; no change scheduled"
        );
    }
}
