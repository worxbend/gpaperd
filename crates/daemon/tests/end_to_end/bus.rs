//! A private session bus for the control scenarios: one `dbus-daemon` per
//! scenario, killed when it is dropped. No scenario reaches the user's bus.
//!
//! The daemon is not started with `--session`. Its config has no service
//! directories, so `GetActive` cannot exec a session service.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use gpaperd_ctl::Gpaperd1Proxy;
use tempfile::TempDir;
use zbus::proxy::CacheProperties;

use crate::world::scenario;

const BUS_CONFIG: &str = "bus.conf";

pub struct PrivateBus {
    child: Child,
    address: String,
    /// Socket directory. Dropped after the daemon is reaped.
    files: TempDir,
}

impl PrivateBus {
    pub fn start() -> PrivateBus {
        let files = tempfile::tempdir().expect("private bus directory");
        write_bus_config(files.path());
        let started = SpawnedBus::spawn(&files.path().join(BUS_CONFIG)).finish(files);
        match started {
            Ok(bus) => bus,
            Err(reason) => panic!("{reason}"),
        }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .expect("connect to the private bus")
    }

    /// The client that `gpaperctl` uses. Properties are read from the
    /// daemon on each call, never from a cache.
    pub async fn client(&self) -> Gpaperd1Proxy<'static> {
        Gpaperd1Proxy::builder(&self.connect().await)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("build the proxy")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        kill_and_reap(&mut self.child);
    }
}

/// A `dbus-daemon` that is killed if start fails before [`PrivateBus`] owns it.
struct SpawnedBus {
    child: Option<Child>,
}

impl SpawnedBus {
    fn spawn(config: &Path) -> SpawnedBus {
        let child = dbus_daemon(config)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start dbus-daemon");
        SpawnedBus { child: Some(child) }
    }

    fn finish(mut self, files: TempDir) -> Result<PrivateBus, String> {
        let address = printed_address(self.child.as_mut().expect("spawned dbus-daemon"))?;
        Ok(PrivateBus {
            child: self.child.take().expect("spawned dbus-daemon"),
            address,
            files,
        })
    }
}

impl Drop for SpawnedBus {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_and_reap(&mut child);
        }
    }
}

fn dbus_daemon(config: &Path) -> Command {
    let mut command = Command::new("dbus-daemon");
    command
        .arg("--config-file")
        .arg(config)
        .args(["--print-address", "--nofork", "--nopidfile"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

/// No `<standard_session_servicedirs/>`: a name with no owner is not activated.
fn write_bus_config(dir: &Path) {
    let socket_dir = dir.to_str().expect("the private bus directory is utf-8");
    let config = format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={socket_dir}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
    );
    fs::write(dir.join(BUS_CONFIG), config).expect("write the bus config");
}

fn printed_address(child: &mut Child) -> Result<String, String> {
    let Some(stdout) = child.stdout.take() else {
        return Err("a piped stdout".to_owned());
    };
    let mut address = String::new();
    if let Err(error) = BufReader::new(stdout).read_line(&mut address) {
        return Err(format!("dbus-daemon prints its address: {error}"));
    }
    let address = address.trim().to_owned();
    if address.is_empty() {
        return Err("dbus-daemon printed no address".to_owned());
    }
    Ok(address)
}

/// `wait` has no deadline. This file has none to reuse.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn command_line(pid: u32) -> Vec<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).expect("read the daemon command");
    bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8(arg.to_vec()).expect("a utf-8 command line"))
        .collect()
}

async fn activatable_names(bus: &PrivateBus) -> Vec<String> {
    let connection = bus.connect().await;
    let dbus = zbus::fdo::DBusProxy::new(&connection)
        .await
        .expect("bus daemon proxy");
    dbus.list_activatable_names()
        .await
        .expect("list activatable names")
        .into_iter()
        .map(|name| name.to_string())
        .collect()
}

fn assert_child_reaped(pid: u32) {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("child {pid} status: {error}"),
        Ok(stat) => panic!("child {pid} was not reaped: {stat}"),
    }
}

#[test]
fn get_active_is_not_sent_on_a_bus_that_loads_session_services() {
    scenario(async {
        let bus = PrivateBus::start();
        let args = command_line(bus.child.id());
        assert!(
            !args.iter().any(|arg| arg == "--session"),
            "GetActive would run on a --session bus: {args:?}"
        );
        let config_flag = args
            .iter()
            .position(|arg| arg == "--config-file")
            .expect("the daemon is started from a config file");
        let config_path = args
            .get(config_flag + 1)
            .expect("a path follows --config-file");
        assert_eq!(
            Path::new(config_path),
            bus.files.path().join(BUS_CONFIG),
            "dbus-daemon was not given the private config: {args:?}"
        );
        let config = fs::read_to_string(config_path).expect("read the bus config");
        assert!(
            !config.contains("standard_session_servicedirs") && !config.contains("<include"),
            "GetActive is sent on a bus that loads session services:\n{config}"
        );
        let names = activatable_names(&bus).await;
        assert!(
            !names.iter().any(|name| name == "org.gnome.ScreenSaver"),
            "GetActive can activate org.gnome.ScreenSaver: {names:?}"
        );
    });
}

#[test]
fn a_failed_start_kills_and_reaps_the_child() {
    let files = tempfile::tempdir().expect("private bus directory");
    write_bus_config(files.path());
    let child = dbus_daemon(&files.path().join(BUS_CONFIG))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start dbus-daemon");
    let pid = child.id();
    let started = SpawnedBus { child: Some(child) }.finish(files);

    assert!(started.is_err(), "a start that prints no address must fail");
    assert_child_reaped(pid);
}
