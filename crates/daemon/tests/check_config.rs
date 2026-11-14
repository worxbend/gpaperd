//! `gpaperd check-config`, run as the real binary.
//!
//! Each run gets its own temp `HOME` and XDG directories, the keyfile
//! settings backend, and a session bus address that leads to a socket this
//! test listens on. So a run that touched GNOME, the bus, the slots, the
//! state file or the cache would leave a trace here, and never reaches the
//! user's own session.

use std::fs;
use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use gpaperd_daemon::cache::{Cache, Meta};
use tempfile::TempDir;
use url::Url;

/// A run that takes longer than this has hung: it is killed and fails.
const RUN_LIMIT: Duration = Duration::from_secs(30);
/// How often a running child is looked at.
const LOOK_PERIOD: Duration = Duration::from_millis(20);
/// `EX_NOINPUT`.
const EXIT_NO_INPUT: i32 = 66;
/// `EX_CONFIG`.
const EXIT_CONFIG: i32 = 78;

const TINY_PNG: &[u8] = include_bytes!("fixtures/tiny.png");

/// A home, XDG directories and a bus socket of its own, all empty at first.
struct Sandbox {
    home: TempDir,
    config: TempDir,
    data: TempDir,
    state: TempDir,
    cache: TempDir,
    runtime: TempDir,
    /// Where the images live: outside every directory that must stay as it
    /// was.
    pictures: TempDir,
    bus_dir: TempDir,
    bus: UnixListener,
}

impl Sandbox {
    fn new() -> Sandbox {
        // A socket path must fit in sun_path (108 bytes); $TMPDIR can be longer.
        let bus_dir = tempfile::tempdir_in("/tmp").expect("create a bus dir");
        let bus = UnixListener::bind(bus_dir.path().join("bus")).expect("listen on a bus socket");
        bus.set_nonblocking(true).expect("a non-blocking socket");
        Sandbox {
            home: temp_dir(),
            config: temp_dir(),
            data: temp_dir(),
            state: temp_dir(),
            cache: temp_dir(),
            runtime: temp_dir(),
            pictures: temp_dir(),
            bus_dir,
            bus,
        }
    }

    /// The default config file: `$XDG_CONFIG_HOME/gpaperd/config.toml`.
    fn default_config(&self) -> PathBuf {
        self.config.path().join("gpaperd/config.toml")
    }

    fn write_default_config(&self, text: &str) {
        let file = self.default_config();
        fs::create_dir_all(file.parent().expect("a parent")).expect("create the config dir");
        fs::write(file, text).expect("write the config");
    }

    fn picture(&self, relative: &str) -> PathBuf {
        let path = self.pictures.path().join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create the picture dir");
        fs::write(&path, TINY_PNG).expect("write the picture");
        path
    }

    /// The directories that `check-config` must leave as they were.
    fn guarded(&self) -> [&Path; 6] {
        [
            self.home.path(),
            self.config.path(),
            self.data.path(),
            self.state.path(),
            self.cache.path(),
            self.runtime.path(),
        ]
    }

    /// Every path below the guarded directories.
    fn snapshot(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for dir in self.guarded() {
            list(dir, &mut paths);
        }
        paths.sort();
        paths
    }

    /// Runs `gpaperd check-config` with `args` after it.
    fn check_config(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gpaperd"));
        command
            .arg("check-config")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env("XDG_STATE_HOME", self.state.path())
            .env("XDG_CACHE_HOME", self.cache.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("GSETTINGS_BACKEND", "keyfile")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", self.bus_dir.path().join("bus").display()),
            )
            .env("PICTURES", self.pictures.path())
            .current_dir(self.pictures.path());
        run_bounded(command)
    }

    /// Fails when anything connected to the bus socket.
    fn assert_bus_untouched(&self) {
        match self.bus.accept() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("cannot look at the bus socket: {error}"),
            Ok(_) => panic!("check-config connected to the session bus"),
        }
    }
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("create a temp dir")
}

fn list(dir: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("list a guarded dir") {
        let path = entry.expect("read an entry").path();
        if path.is_dir() {
            list(&path, paths);
        }
        paths.push(path);
    }
}

/// Runs `command` to its end, or kills it after [`RUN_LIMIT`] and fails.
fn run_bounded(mut command: Command) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start gpaperd");
    let started = Instant::now();
    while child.try_wait().expect("look at gpaperd").is_none() {
        if started.elapsed() > RUN_LIMIT {
            let _ = child.kill();
            panic!("gpaperd check-config ran for more than {RUN_LIMIT:?}");
        }
        thread::sleep(LOOK_PERIOD);
    }
    child.wait_with_output().expect("collect the output")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Puts a blob for `url` in the cache below `cache_home`, as an earlier run
/// of the daemon would have.
fn cache_remote_image(cache_home: &Path, url: &Url) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build a runtime");
    let cache = Cache::new(cache_home.join("gpaperd/remote"));
    runtime.block_on(async {
        let mut draft = cache.draft(url).await.expect("start a blob");
        draft.append(TINY_PNG).await.expect("write the blob");
        let meta = Meta {
            mime: Some("image/png".to_owned()),
            ..Meta::new(url.clone())
        };
        cache
            .publish(url, draft, "png", meta)
            .await
            .expect("publish the blob");
    });
}

#[test]
fn valid_config_prints_the_count_of_each_source_and_exits_0() {
    let sandbox = Sandbox::new();
    sandbox.picture("light/a.png");
    sandbox.picture("light/b.jpg");
    sandbox.picture("light/notes.txt");
    sandbox.picture("shared/deep/c.png");
    sandbox.picture("hero.png");
    let cached = Url::parse("https://img.test/walls/dawn.png").expect("a URL");
    cache_remote_image(sandbox.cache.path(), &cached);
    sandbox.write_default_config(
        r#"
[light]
sources = [
  "$PICTURES/light",
  { dir = "${PICTURES}/shared", recursive = true },
  { file = "hero.png" },
  "https://img.test/walls/dawn.png",
]

[dark]
sources = [
  "https://user:secret@img.test/walls/night.png?token=s3cret",
  "$PICTURES/missing",
]
"#,
    );
    // The config names its relative `hero.png` next to itself.
    fs::copy(
        sandbox.pictures.path().join("hero.png"),
        sandbox.config.path().join("gpaperd/hero.png"),
    )
    .expect("copy the hero image");
    let before = sandbox.snapshot();

    let output = sandbox.check_config(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = stdout(&output);
    let pictures = sandbox.pictures.path().display();
    let config_dir = fs::canonicalize(sandbox.config.path().join("gpaperd")).expect("resolve");
    let expected = format!(
        "config file {config}: valid\n\
         light:\n  \
         {pictures}/light: 2 images\n  \
         {pictures}/shared: 1 image\n  \
         {hero}: 1 image\n  \
         https://img.test/walls/dawn.png: remote, cached\n\
         dark:\n  \
         https://img.test/walls/night.png: remote, not cached\n  \
         {pictures}/missing: 0 images\n    \
         skipped {pictures}/missing: No such file or directory (os error 2)\n",
        config = sandbox.default_config().display(),
        hero = config_dir.join("hero.png").display(),
    );
    assert_eq!(report, expected);
    assert!(!report.contains("secret"), "{report}");
    assert_eq!(
        sandbox.snapshot(),
        before,
        "check-config changed a guarded dir"
    );
    sandbox.assert_bus_untouched();
}

#[test]
fn config_option_after_the_subcommand_picks_the_file() {
    let sandbox = Sandbox::new();
    let file = sandbox.pictures.path().join("elsewhere.toml");
    fs::write(&file, "[dark]\nsources = []\n").expect("write the config");

    let output = sandbox.check_config(&["--config", file.to_str().expect("UTF-8")]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "config file {}: valid\nlight:\n  no sources\ndark:\n  no sources\n",
            file.display()
        )
    );
    assert!(sandbox.snapshot().is_empty(), "{:?}", sandbox.snapshot());
    sandbox.assert_bus_untouched();
}

#[test]
fn invalid_config_exits_78_with_line_and_column() {
    let sandbox = Sandbox::new();
    sandbox.write_default_config("[general]\ninterval = \"30s\"\n");
    let before = sandbox.snapshot();

    let output = sandbox.check_config(&[]);

    assert_eq!(output.status.code(), Some(EXIT_CONFIG));
    let message = stderr(&output);
    assert!(
        message.contains("config.toml: line 2, column 12"),
        "{message}"
    );
    assert_eq!(stdout(&output), "");
    assert_eq!(sandbox.snapshot(), before);
    sandbox.assert_bus_untouched();
}

#[test]
fn unknown_key_exits_78_with_line_and_column() {
    let sandbox = Sandbox::new();
    sandbox.write_default_config("[general]\norder = \"random\"\nspeed = 3\n");

    let output = sandbox.check_config(&[]);

    assert_eq!(output.status.code(), Some(EXIT_CONFIG));
    let message = stderr(&output);
    assert!(message.contains("line 3, column 1"), "{message}");
}

#[test]
fn missing_config_file_exits_66_and_names_it() {
    let sandbox = Sandbox::new();

    let output = sandbox.check_config(&["--config", "/nonexistent/gpaperd.toml"]);

    assert_eq!(output.status.code(), Some(EXIT_NO_INPUT));
    let message = stderr(&output);
    assert!(
        message.contains("/nonexistent/gpaperd.toml: no such file"),
        "{message}"
    );
    assert!(sandbox.snapshot().is_empty(), "{:?}", sandbox.snapshot());
    sandbox.assert_bus_untouched();
}
