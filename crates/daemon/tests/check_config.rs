//! `gpaperd check-config`, run as the real binary.
//!
//! Each run gets its own temp `HOME` and XDG directories, the keyfile
//! settings backend, and a session bus address that leads to a socket this
//! test listens on. So a run that touched GNOME, the bus, the slots, the
//! state file or the cache would leave a trace here, and never reaches the
//! user's own session.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
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
/// Any failure that is not a missing or invalid config file.
const EXIT_FAILURE: i32 = 1;

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

    /// Every path below the guarded directories, with file bytes.
    ///
    /// Bytes are part of the record so an in-place rewrite compares unequal.
    fn snapshot(&self) -> Vec<GuardedEntry> {
        let mut entries = Vec::new();
        for dir in self.guarded() {
            list(dir, &mut entries);
        }
        entries.sort();
        entries
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
        match run_bounded(command, RUN_LIMIT) {
            Ok(output) => output,
            Err(timed_out) => panic!(
                "gpaperd check-config ran for more than {RUN_LIMIT:?}; child returned {}",
                timed_out.status
            ),
        }
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

/// One guarded path. `bytes` is `None` for a directory and the file contents
/// otherwise, so a rewrite of the same path does not compare equal.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GuardedEntry {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

fn list(dir: &Path, entries: &mut Vec<GuardedEntry>) {
    for entry in fs::read_dir(dir).expect("list a guarded dir") {
        let path = entry.expect("read an entry").path();
        if path.is_dir() {
            list(&path, entries);
            entries.push(GuardedEntry { path, bytes: None });
        } else {
            let bytes = fs::read(&path).expect("read a guarded file");
            entries.push(GuardedEntry {
                path,
                bytes: Some(bytes),
            });
        }
    }
}

/// The child was killed once `limit` passed, and then reaped.
#[derive(Debug)]
struct TimedOut {
    status: ExitStatus,
    pid: u32,
}

/// Runs `command` until it exits or `limit` passes.
///
/// Stdout and stderr are drained while it runs, so a full pipe cannot stall
/// it into the limit. Past the limit the child is killed and reaped. A
/// check-config run passes [`RUN_LIMIT`]. A test passes a shorter limit to
/// enter that branch without waiting out a hang.
fn run_bounded(mut command: Command, limit: Duration) -> Result<Output, TimedOut> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start gpaperd");
    let stdout_reader = drain(child.stdout.take().expect("piped stdout"));
    let stderr_reader = drain(child.stderr.take().expect("piped stderr"));
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("look at gpaperd") {
            return Ok(Output {
                status,
                stdout: finish_drain(stdout_reader),
                stderr: finish_drain(stderr_reader),
            });
        }
        if started.elapsed() > limit {
            let status = kill_and_reap(&mut child);
            let pid = child.id();
            finish_drain(stdout_reader);
            finish_drain(stderr_reader);
            return Err(TimedOut { status, pid });
        }
        thread::sleep(LOOK_PERIOD);
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).expect("read gpaperd output");
        bytes
    })
}

fn finish_drain(reader: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    reader
        .join()
        .unwrap_or_else(|_| panic!("gpaperd output reader panicked"))
}

/// Kills `child` and reaps it. A child that has already returned is reaped
/// too. Fails the test when that return is still unreaped.
fn kill_and_reap(child: &mut Child) -> ExitStatus {
    match child.kill() {
        Ok(()) => child.wait().unwrap_or_else(|error| {
            panic!(
                "gpaperd check-config ran for more than {RUN_LIMIT:?} and the child was not reaped: {error}"
            )
        }),
        Err(error) => match child.try_wait() {
            Ok(Some(status)) => panic!(
                "gpaperd check-config ran for more than {RUN_LIMIT:?}; kill failed ({error}) and the unreaped child returned {status}"
            ),
            Ok(None) => panic!(
                "gpaperd check-config ran for more than {RUN_LIMIT:?}; kill failed ({error}) and the child is still unreaped"
            ),
            Err(wait_error) => panic!(
                "gpaperd check-config ran for more than {RUN_LIMIT:?}; kill failed ({error}) and the child was not reaped ({wait_error})"
            ),
        },
    }
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

/// How an unlistable cache path is undone.
enum CacheLock {
    /// Mode `000`. Drop restores `755`.
    Mode(PathBuf),
    /// The path is a file. Nothing to restore.
    File,
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        if let CacheLock::Mode(dir) = self {
            // Best effort: Drop cannot report this, and a leftover mode blocks temp-dir cleanup.
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
    }
}

/// Makes `dir` unlistable. `dir` must already exist.
///
/// Root can still list mode `000`, so the path then becomes a file. `read_dir`
/// rejects that for every user.
fn lock_cache(dir: &Path) -> CacheLock {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).expect("lock the cache");
    if fs::read_dir(dir).is_err() {
        return CacheLock::Mode(dir.to_path_buf());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).expect("unlock the cache");
    fs::remove_dir(dir).expect("remove the cache dir");
    fs::write(dir, b"not a directory").expect("replace the cache with a file");
    CacheLock::File
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

#[test]
fn snapshot_fails_when_a_guarded_file_is_rewritten() {
    let sandbox = Sandbox::new();
    sandbox.write_default_config("before\n");
    let before = sandbox.snapshot();

    fs::write(sandbox.default_config(), "after\n").expect("rewrite the config");

    assert_ne!(
        sandbox.snapshot(),
        before,
        "an in-place rewrite still compared equal"
    );
}

/// `dd` block size. The filler count is a whole number of these.
const PIPE_BLOCK: usize = 1024;

/// More bytes than a pipe on this machine can hold. A child that writes this
/// much blocks in `write` until the parent reads, so a drain that starts
/// only after exit never sees the child finish.
fn bytes_past_a_pipe() -> usize {
    let max = fs::read_to_string("/proc/sys/fs/pipe-max-size")
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .unwrap_or(64 * PIPE_BLOCK);
    (max / PIPE_BLOCK + 2) * PIPE_BLOCK
}

#[test]
fn a_full_pipe_is_read_while_the_child_runs() {
    let bytes = bytes_past_a_pipe();
    let blocks = bytes / PIPE_BLOCK;
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "/bin/dd if=/dev/zero bs={PIPE_BLOCK} count={blocks} 2>/dev/null; \
         /bin/dd if=/dev/zero bs={PIPE_BLOCK} count={blocks} >&2 2>/dev/null"
    ));

    let output = run_bounded(command, RUN_LIMIT).expect("a full pipe stalled until the limit");

    assert_eq!(output.status.code(), Some(0), "{:?}", output.status);
    assert_eq!(
        output.stdout.len(),
        bytes,
        "stdout was read only after exit"
    );
    assert_eq!(
        output.stderr.len(),
        bytes,
        "stderr was read only after exit"
    );
}

#[test]
fn a_child_past_the_limit_is_reaped() {
    let mut command = Command::new("/bin/sleep");
    command.arg(RUN_LIMIT.as_secs().to_string());

    // One look crosses the same branch `check_config` uses with `RUN_LIMIT`.
    let timed_out = run_bounded(command, LOOK_PERIOD).expect_err("sleep returned before the limit");

    assert_eq!(
        timed_out.status.code(),
        None,
        "child exited instead of being killed"
    );
    assert!(
        !Path::new(&format!("/proc/{}", timed_out.pid)).exists(),
        "pid {} is still unreaped",
        timed_out.pid
    );
}

#[test]
fn a_file_and_a_symlink_to_it_count_once() {
    let sandbox = Sandbox::new();
    let image = sandbox.picture("walls/a.png");
    symlink(&image, image.parent().expect("a parent").join("link.png")).expect("symlink");
    sandbox.write_default_config(
        r#"
[light]
sources = [
  { dir = "$PICTURES/walls", follow_symlinks = true },
]
"#,
    );

    let output = sandbox.check_config(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = stdout(&output);
    let walls = sandbox.pictures.path().join("walls");
    assert!(
        report.contains(&format!("{}: 1 image\n", walls.display())),
        "{report}"
    );
}

#[test]
fn missing_remote_cache_is_not_cached() {
    let sandbox = Sandbox::new();
    sandbox.write_default_config("[light]\nsources = [\"https://img.test/walls/dawn.png\"]\n");

    let output = sandbox.check_config(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(
        report.contains("https://img.test/walls/dawn.png: remote, not cached\n"),
        "{report}"
    );
}

#[test]
fn unreadable_remote_cache_is_a_failure() {
    let sandbox = Sandbox::new();
    sandbox.write_default_config("[light]\nsources = [\"https://img.test/walls/dawn.png\"]\n");
    let cache_dir = sandbox.cache.path().join("gpaperd/remote");
    fs::create_dir_all(&cache_dir).expect("create the cache dir");
    let before_lock = sandbox.snapshot();
    let lock = lock_cache(&cache_dir);
    let before = match &lock {
        CacheLock::Mode(_) => before_lock,
        CacheLock::File => sandbox.snapshot(),
    };

    let output = sandbox.check_config(&[]);

    assert_eq!(
        output.status.code(),
        Some(EXIT_FAILURE),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("remote cache"),
        "{}",
        stderr(&output)
    );
    drop(lock);
    assert_eq!(
        sandbox.snapshot(),
        before,
        "check-config changed a guarded dir"
    );
}
