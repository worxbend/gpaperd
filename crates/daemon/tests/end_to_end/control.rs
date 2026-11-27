//! The control interface end to end: `gpaperctl`'s client on a private bus,
//! the real D-Bus server, interpreter and reducer, and the fake GNOME.

use std::fmt::Debug;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, UNIX_EPOCH};

use futures_util::StreamExt;
use gpaperd_core::control::{BUS_NAME, STATUS_VERSION, StatusReport};
use gpaperd_core::event::Pause;
use gpaperd_core::style::Style;
use gpaperd_daemon::dbus::{self, OwnError};

use crate::bus::PrivateBus;
use crate::world::{
    ALPHABETICAL, Daemon, LIGHT_INTERVAL, MINUTE, PATIENCE, SEED, World, eventually, scenario,
    start_time, users_keys,
};

/// How often [`property_becomes`] reads again.
const READ_PERIOD: Duration = Duration::from_millis(10);

/// A world with three light and two dark images, whose daemon serves its
/// control interface on `bus` and shows the first image of each style.
async fn running_daemon(bus: &PrivateBus) -> (World, Daemon) {
    let world = World::new();
    world.add_images(Style::Light, &["a.png", "b.png", "c.png"]);
    world.add_images(Style::Dark, &["x.png", "y.png"]);
    world.save_config_file(&world.config_text(ALPHABETICAL));
    let mut daemon = world.start_daemon_on_bus(bus.address(), SEED).await;
    world.showing(&mut daemon, "a.png", "x.png").await;
    (world, daemon)
}

/// Waits until `read` gives `wanted`.
async fn property_becomes<T: PartialEq + Debug>(
    what: &str,
    read: impl AsyncFn() -> zbus::Result<T>,
    wanted: T,
) {
    let waiting = async {
        loop {
            let value = read().await.expect("read the property");
            if value == wanted {
                return;
            }
            tokio::time::sleep(READ_PERIOD).await;
        }
    };
    let waited = tokio::time::timeout(PATIENCE, waiting).await;
    assert!(
        waited.is_ok(),
        "timed out waiting for {what} to be {wanted:?}"
    );
}

async fn status_report(client: &gpaperd_ctl::Gpaperd1Proxy<'_>) -> StatusReport {
    let json = client.status().await.expect("Status()");
    serde_json::from_str(&json).expect("Status() returns a StatusReport")
}

#[test]
fn next_shows_the_next_image_and_emits_changed() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;
        let mut changes = client
            .receive_changed()
            .await
            .expect("subscribe to Changed");

        client.next("light").await.expect("Next(light)");

        assert_eq!(world.shown(Style::Light), "b.png");
        let signal = tokio::time::timeout(PATIENCE, changes.next())
            .await
            .expect("a Changed signal in time")
            .expect("the signal stream to be open");
        let args = signal.args().expect("Changed's arguments");
        let b = world.item(Style::Light, "b.png");
        assert_eq!((args.style, args.id), ("light", b.as_str()));
        let b_id = b.as_str().to_owned();
        property_becomes("CurrentLight", async || client.current_light().await, b_id).await;
        daemon.terminate().await;
    });
}

#[test]
fn previous_goes_back_to_the_image_before() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;
        client.next("visible").await.expect("Next(visible)");
        assert_eq!(world.shown(Style::Light), "b.png");

        client.previous("light").await.expect("Previous(light)");

        assert_eq!(world.shown(Style::Light), "a.png");
        let history = &world.saved_state().styles.light.history;
        assert!(history.is_empty(), "{history:?}");
        daemon.terminate().await;
    });
}

#[test]
fn a_paused_style_skips_its_deadline_and_rotates_on_resume() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, mut daemon) = running_daemon(&bus).await;
        let client = bus.client().await;

        client.pause("light").await.expect("Pause(light)");
        let armed = world.clock.times_armed();
        world.clock.step_to(start_time() + LIGHT_INTERVAL + MINUTE);
        eventually("the paused style's deadline is observed", || {
            world.clock.times_armed() > armed
        })
        .await;

        let paused = status_report(&client).await;
        assert_eq!(paused.styles.light.paused, [Pause::User]);
        assert_eq!(world.shown(Style::Light), "a.png");
        assert!(world.saved_state().styles.light.user_paused);
        property_becomes(
            "Paused",
            async || client.paused().await,
            vec!["light".to_owned()],
        )
        .await;

        client.resume("light").await.expect("Resume(light)");

        // Overdue: the rotation comes at once.
        world.showing(&mut daemon, "b.png", "x.png").await;
        assert!(!world.saved_state().styles.light.user_paused);
        daemon.terminate().await;
    });
}

#[test]
fn reload_answers_a_broken_config_with_its_position() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;

        world.save_config_file("[general]\ninterval = \"30s\"\n");
        let (ok, error) = client.reload().await.expect("Reload()");

        assert!(!ok);
        assert!(error.starts_with("line 2, column 12: "), "{error}");
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let reloaded = client.reload().await.expect("Reload()");
        assert_eq!(reloaded, (true, String::new()));
        daemon.terminate().await;
    });
}

#[test]
fn restore_writes_the_users_keys_back_and_pauses_both_styles() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;
        assert_ne!(world.gnome.keys(), users_keys());

        client.restore().await.expect("Restore()");

        eventually("the user's keys", || world.gnome.keys() == users_keys()).await;
        assert_eq!(world.gnome.restores(), [users_keys()]);
        let report = status_report(&client).await;
        for style in Style::ALL {
            assert!(
                report.styles[style].paused.contains(&Pause::User),
                "{report:?}"
            );
        }
        // The keys stay the user's until a resume.
        let writes_before = world.gnome.key_writes().len();
        let armed = world.clock.times_armed();
        world.clock.step_to(start_time() + LIGHT_INTERVAL + MINUTE);
        eventually("the deadline after restore is observed", || {
            world.clock.times_armed() > armed
        })
        .await;
        status_report(&client).await;
        assert_eq!(world.gnome.keys(), users_keys());
        assert_eq!(world.gnome.key_writes().len(), writes_before);

        client.resume("both").await.expect("Resume(both)");

        eventually("the slot URIs in both keys", || {
            world.gnome.key(Style::Light) == Some(world.slot_uri(Style::Light))
                && world.gnome.key(Style::Dark) == Some(world.slot_uri(Style::Dark))
        })
        .await;
        daemon.terminate().await;
    });
}

#[test]
fn restore_whose_write_fails_returns_the_error() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;
        world.gnome.lock_keys();

        let error = client.restore().await.expect_err("Restore() fails");

        let text = error.to_string();
        assert!(text.contains("locked keys"), "{text}");
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
        daemon.terminate().await;
    });
}

#[test]
fn a_restore_in_flight_when_the_daemon_stops_is_not_answered_failed() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, mut daemon) = running_daemon(&bus).await;
        let client = bus.client().await;
        let (release, entered) = world.gnome.hold_the_next_restore();
        let pending = tokio::task::spawn_local(async move { client.restore().await });
        tokio::time::timeout(PATIENCE, entered)
            .await
            .expect("restore to start")
            .expect("the restore entered");

        daemon.signal_stop();
        release.send(()).expect("release the restore");
        let reply = tokio::time::timeout(PATIENCE, pending)
            .await
            .expect("restore to finish")
            .expect("the client task");

        assert!(reply.is_ok(), "{reply:?}");
        assert!(daemon.outcome().await.is_ok());
    });
}

#[test]
fn status_returns_the_report_as_json() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;

        let report = status_report(&client).await;

        assert_eq!(report.version, STATUS_VERSION);
        assert_eq!(report.visible, Style::Light);
        let light = &report.styles.light;
        assert_eq!(light.current, Some(world.item(Style::Light, "a.png")));
        assert_eq!((light.items, light.usable), (3, 3));
        assert!(light.paused.is_empty());
        let deadline = world.clock.armed().expect("an armed alarm");
        let deadline_secs = deadline
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs();
        assert_eq!(report.next_change, Some(deadline_secs));
        assert_eq!(client.style().await.expect("Style"), "light");
        daemon.terminate().await;
    });
}

#[test]
fn a_bad_style_is_invalid_args() {
    scenario(async {
        let bus = PrivateBus::start();
        let (world, daemon) = running_daemon(&bus).await;
        let client = bus.client().await;

        let error = client.next("all").await.expect_err("all is no style");

        let is_invalid_args = match &error {
            zbus::Error::FDO(fdo_error) => matches!(**fdo_error, zbus::fdo::Error::InvalidArgs(_)),
            _ => false,
        };
        assert!(is_invalid_args, "{error:?}");
        assert_eq!(world.shown(Style::Light), "a.png");
        daemon.terminate().await;
    });
}

#[test]
fn a_second_daemon_cannot_take_the_name() {
    scenario(async {
        let bus = PrivateBus::start();
        let (_world, daemon) = running_daemon(&bus).await;

        let builder = zbus::connection::Builder::address(bus.address());
        let second = dbus::serve(builder).await;

        assert!(matches!(second, Err(OwnError::AlreadyOwned)), "{second:?}");
        daemon.terminate().await;
    });
}

/// The real binary, while something else holds the bus name: it must exit
/// 0 before it touches GNOME, a slot or the state file.
#[test]
fn a_second_gpaperd_exits_0_with_a_message() {
    scenario(async {
        let bus = PrivateBus::start();
        let _owner = zbus::connection::Builder::address(bus.address())
            .name(BUS_NAME)
            .build()
            .await
            .expect("take the name");
        let home = tempfile::tempdir().expect("create a temp HOME");
        let dir = |name: &str| home.path().join(name);

        let mut gpaperd = Command::new(env!("CARGO_BIN_EXE_gpaperd"))
            .env_clear()
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", dir("config"))
            .env("XDG_DATA_HOME", dir("data"))
            .env("XDG_STATE_HOME", dir("state"))
            .env("XDG_CACHE_HOME", dir("cache"))
            .env("XDG_RUNTIME_DIR", dir("run"))
            .env("GSETTINGS_BACKEND", "keyfile")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start gpaperd");
        let waiting = async {
            loop {
                if let Some(status) = gpaperd.try_wait().expect("poll gpaperd") {
                    return status;
                }
                tokio::time::sleep(READ_PERIOD).await;
            }
        };
        let exited = tokio::time::timeout(PATIENCE, waiting).await;
        let Ok(status) = exited else {
            let _ = gpaperd.kill();
            let _ = gpaperd.wait();
            panic!("the second gpaperd did not exit");
        };
        let mut stderr = String::new();
        gpaperd
            .stderr
            .take()
            .expect("a piped stderr")
            .read_to_string(&mut stderr)
            .expect("read gpaperd's stderr");

        assert!(status.success(), "{status}: {stderr}");
        assert!(stderr.contains("already running"), "{stderr}");
        let touched: Vec<_> = std::fs::read_dir(home.path())
            .expect("list the temp HOME")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert!(touched.is_empty(), "the second gpaperd created {touched:?}");
    });
}
