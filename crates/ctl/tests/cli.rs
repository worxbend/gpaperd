//! Runs the real `gpaperctl` binary against a fake daemon on a private
//! `dbus-daemon`. Never touches the user's session bus: every child gets a
//! cleared environment with the private bus's address only.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gpaperd_core::control::{BUS_NAME, OBJECT_PATH};
use tempfile::TempDir;
use tokio::sync::oneshot;
use zbus::{fdo, interface};

const STATUS_JSON: &str = r#"{"version":1,"visible":"light","next_change":null,"styles":{"light":{"current":"/pics/day.jpg","paused":["user"],"items":4,"usable":3,"bad":[{"id":"/pics/x.png","reason":"not an image"}]},"dark":{"current":null,"paused":[],"items":0,"usable":0,"bad":[]}}}"#;

/// A private bus. Killed on drop.
struct Bus {
    daemon: Child,
    address: String,
    home: TempDir,
}

impl Bus {
    fn start() -> Bus {
        let home = TempDir::new().expect("temp home");
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon starts");
        let stdout = daemon.stdout.take().expect("piped stdout");
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .expect("dbus-daemon prints its address");
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Bus {
            daemon,
            address,
            home,
        }
    }

    /// Runs `gpaperctl args…` on this bus.
    fn ctl(&self, args: &[&str]) -> Output {
        ctl_with(&self.address, self.home.path(), args)
    }

    /// Runs `gpaperctl args…` on this bus from the working directory `cwd`.
    fn ctl_in(&self, cwd: &Path, args: &[&str]) -> Output {
        ctl_command(&self.address, self.home.path(), args)
            .current_dir(cwd)
            .output()
            .expect("gpaperctl runs")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn ctl_with(address: &str, home: &Path, args: &[&str]) -> Output {
    ctl_command(address, home, args)
        .output()
        .expect("gpaperctl runs")
}

fn ctl_command(address: &str, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpaperctl"));
    command
        .args(args)
        .env_clear()
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("TZ", "UTC");
    command
}

/// How the fake daemon answers.
#[derive(Clone, Default)]
struct Script {
    reload_error: Option<String>,
    restore_error: Option<String>,
    /// `Status()` never answers, as a stopped daemon does.
    status_hangs: bool,
}

/// Stands in for gpaperd's control object; records every call.
struct FakeDaemon {
    calls: Arc<Mutex<Vec<String>>>,
    script: Script,
}

impl FakeDaemon {
    fn record(&self, call: String) {
        self.calls.lock().expect("calls lock").push(call);
    }
}

#[interface(name = "io.github.worxbend.Gpaperd1")]
impl FakeDaemon {
    fn next(&self, style: &str) {
        self.record(format!("Next({style})"));
    }

    fn previous(&self, style: &str) {
        self.record(format!("Previous({style})"));
    }

    fn set(&self, style: &str, id: &str) {
        self.record(format!("Set({style}, {id})"));
    }

    fn pause(&self, style: &str) {
        self.record(format!("Pause({style})"));
    }

    fn resume(&self, style: &str) {
        self.record(format!("Resume({style})"));
    }

    #[zbus(out_args("ok", "error"))]
    fn reload(&self) -> (bool, String) {
        self.record("Reload".to_owned());
        match &self.script.reload_error {
            Some(error) => (false, error.clone()),
            None => (true, String::new()),
        }
    }

    fn refresh_remote(&self) {
        self.record("RefreshRemote".to_owned());
    }

    fn restore(&self) -> fdo::Result<()> {
        self.record("Restore".to_owned());
        match &self.script.restore_error {
            Some(error) => Err(fdo::Error::Failed(error.clone())),
            None => Ok(()),
        }
    }

    #[zbus(out_args("json"))]
    async fn status(&self) -> String {
        self.record("Status".to_owned());
        if self.script.status_hangs {
            std::future::pending::<()>().await;
        }
        STATUS_JSON.to_owned()
    }
}

/// The fake daemon, owning [`BUS_NAME`] on its own runtime thread until
/// dropped.
struct Server {
    calls: Arc<Mutex<Vec<String>>>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(bus: &Bus, script: Script) -> Server {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeDaemon {
            calls: Arc::clone(&calls),
            script,
        };
        let address = bus.address.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let connection = zbus::connection::Builder::address(address.as_str())
                    .serve_at(OBJECT_PATH, fake)
                    .name(BUS_NAME)
                    .build()
                    .await
                    .expect("fake daemon connects");
                ready_tx.send(()).expect("test waits");
                let _ = stopped.await;
                drop(connection);
            });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("fake daemon owns the name");
        Server {
            calls,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "gpaperctl failed: {:?}\n{}",
        output.status,
        stderr(output)
    );
}

#[test]
fn each_command_calls_its_method() {
    let bus = Bus::start();
    let server = Server::start(&bus, Script::default());
    let commands: [&[&str]; 7] = [
        &["next"],
        &["next", "--style", "both"],
        &["previous", "--style", "dark"],
        &["pause", "--style", "light"],
        &["resume"],
        &["reload"],
        &["refresh"],
    ];
    for args in commands {
        let output = bus.ctl(args);
        assert_ok(&output);
        assert_eq!(stdout(&output), "", "{args:?} prints nothing");
    }
    assert_ok(&bus.ctl(&["restore"]));
    assert_eq!(
        server.calls(),
        [
            "Next(visible)",
            "Next(both)",
            "Previous(dark)",
            "Pause(light)",
            "Resume(visible)",
            "Reload",
            "RefreshRemote",
            "Restore",
        ]
    );
}

#[test]
fn set_sends_a_relative_path_absolute_and_canonical() {
    let bus = Bus::start();
    let server = Server::start(&bus, Script::default());
    let pictures = TempDir::new().expect("temp dir");
    std::fs::write(pictures.path().join("day.jpg"), b"image").expect("write image");
    std::os::unix::fs::symlink("day.jpg", pictures.path().join("link.jpg")).expect("symlink");
    let canonical = pictures
        .path()
        .canonicalize()
        .expect("canonical dir")
        .join("day.jpg");

    let output = bus.ctl_in(pictures.path(), &["set", "link.jpg"]);

    assert_ok(&output);
    assert_eq!(stdout(&output), "");
    assert_eq!(
        server.calls(),
        [format!("Set(visible, {})", canonical.display())]
    );
}

#[test]
fn set_sends_a_url_as_typed() {
    let bus = Bus::start();
    let server = Server::start(&bus, Script::default());

    let output = bus.ctl(&["set", "--style", "dark", "https://example.com/night.jpg"]);

    assert_ok(&output);
    assert_eq!(server.calls(), ["Set(dark, https://example.com/night.jpg)"]);
}

#[test]
fn set_of_a_missing_file_says_so_and_never_reaches_the_bus() {
    let bus = Bus::start();
    let server = Server::start(&bus, Script::default());
    let empty = TempDir::new().expect("temp dir");

    let output = bus.ctl_in(empty.path(), &["set", "gone.jpg"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("gpaperctl: cannot find gone.jpg: "),
        "{}",
        stderr(&output)
    );
    assert!(server.calls().is_empty());
}

#[test]
fn status_json_passes_the_document_through() {
    let bus = Bus::start();
    let _server = Server::start(&bus, Script::default());
    let output = bus.ctl(&["status", "--json"]);
    assert_ok(&output);
    assert_eq!(stdout(&output), format!("{STATUS_JSON}\n"));
}

#[test]
fn status_prints_a_summary() {
    let bus = Bus::start();
    let _server = Server::start(&bus, Script::default());
    let output = bus.ctl(&["status"]);
    assert_ok(&output);
    let expected = "\
visible:     light
next change: none
light:       /pics/day.jpg
             4 items, 3 usable, 1 bad; paused: by user
dark:        none
             0 items, 0 usable, 0 bad
";
    assert_eq!(stdout(&output), expected);
}

#[test]
fn a_failed_reload_prints_the_error_and_exits_1() {
    let bus = Bus::start();
    let script = Script {
        reload_error: Some("line 4, column 12: interval too short".to_owned()),
        ..Script::default()
    };
    let _server = Server::start(&bus, script);
    let output = bus.ctl(&["reload"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "gpaperctl: reload failed: line 4, column 12: interval too short\n"
    );
}

#[test]
fn a_daemon_error_prints_its_message_and_exits_1() {
    let bus = Bus::start();
    let script = Script {
        restore_error: Some("no saved keys to restore".to_owned()),
        ..Script::default()
    };
    let _server = Server::start(&bus, script);
    let output = bus.ctl(&["restore"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "gpaperctl: no saved keys to restore\n");
}

#[test]
fn without_a_daemon_every_command_says_so() {
    let bus = Bus::start();
    for args in [
        &["next"][..],
        &["reload"],
        &["status"],
        &["status", "--json"],
    ] {
        let output = bus.ctl(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), "gpaperctl: gpaperd is not running\n");
    }
}

#[test]
fn a_call_the_daemon_never_answers_times_out() {
    let bus = Bus::start();
    let script = Script {
        status_hangs: true,
        ..Script::default()
    };
    let _server = Server::start(&bus, script);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let error = runtime.block_on(async {
        let connection = zbus::connection::Builder::address(bus.address.as_str())
            .method_timeout(Duration::from_millis(300))
            .build()
            .await
            .expect("client connects");
        let daemon = gpaperd_ctl::Gpaperd1Proxy::builder(&connection)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .expect("proxy");
        let call = daemon.status();
        tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .expect("the method timeout ends the call")
            .expect_err("no answer")
    });

    assert!(gpaperd_ctl::is_timeout(&error), "{error:?}");
}

#[test]
fn an_unknown_style_never_reaches_the_bus() {
    let bus = Bus::start();
    let server = Server::start(&bus, Script::default());
    let output = bus.ctl(&["next", "--style", "all"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown style"),
        "{}",
        stderr(&output)
    );
    assert!(server.calls().is_empty());
}
