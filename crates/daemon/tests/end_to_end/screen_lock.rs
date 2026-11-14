//! `pause_when_locked` against a fake `org.gnome.ScreenSaver` on a private
//! bus: the watch on its own, then the whole daemon.

use std::time::Duration;

use gpaperd_core::event::{Pause, ScreenLock};
use gpaperd_core::style::Style;
use gpaperd_daemon::screensaver;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use crate::bus::PrivateBus;
use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, MINUTE, PATIENCE, SEED, World, let_the_daemon_run, scenario,
};

const SERVICE: &str = "org.gnome.ScreenSaver";
const PATH: &str = "/org/gnome/ScreenSaver";

/// How long a test waits to be sure that nothing comes.
const QUIET: Duration = Duration::from_millis(300);

/// GNOME's screen shield, as far as gpaperd asks it.
struct FakeScreenSaver {
    active: bool,
}

#[interface(name = "org.gnome.ScreenSaver")]
impl FakeScreenSaver {
    fn get_active(&self) -> bool {
        self.active
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

/// Serves the fake shield on `bus`, up (`true`) or down, until the
/// connection is dropped.
async fn serve_screen_saver(bus: &PrivateBus, active: bool) -> Connection {
    zbus::connection::Builder::address(bus.address())
        .name(SERVICE)
        .serve_at(PATH, FakeScreenSaver { active })
        .build()
        .await
        .expect("serve the fake screen saver")
}

/// Emits `ActiveChanged(active)` from the fake shield.
async fn emit_active_changed(service: &Connection, active: bool) {
    let shield = service
        .object_server()
        .interface::<_, FakeScreenSaver>(PATH)
        .await
        .expect("the fake screen saver is served");
    FakeScreenSaver::active_changed(shield.signal_emitter(), active)
        .await
        .expect("emit ActiveChanged");
}

async fn next_lock(locks: &mut UnboundedReceiver<ScreenLock>) -> ScreenLock {
    tokio::time::timeout(PATIENCE, locks.recv())
        .await
        .expect("a screen lock state in time")
        .expect("the watch to be running")
}

#[test]
fn the_watch_reports_the_state_at_start_then_each_change() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_screen_saver(&bus, true).await;
        let mut locks = screensaver::watch(bus.connect().await);

        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
        emit_active_changed(&service, false).await;
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Unlocked);
        emit_active_changed(&service, true).await;
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
    });
}

#[test]
fn without_the_service_the_watch_is_quiet_until_it_appears() {
    scenario(async {
        let bus = PrivateBus::start();
        let mut locks = screensaver::watch(bus.connect().await);

        let early = tokio::time::timeout(QUIET, locks.recv()).await;
        assert!(early.is_err(), "no state without the service: {early:?}");

        // The new owner is asked for its state, then heard.
        let service = serve_screen_saver(&bus, false).await;
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Unlocked);
        emit_active_changed(&service, true).await;
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
    });
}

#[test]
fn a_shield_that_leaves_while_locked_counts_as_unlocked_and_a_new_one_is_asked() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_screen_saver(&bus, true).await;
        let mut locks = screensaver::watch(bus.connect().await);
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);

        // The shell exits: no shield hides the screen any more.
        drop(service);
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Unlocked);

        // A new shell starts locked and sends no `ActiveChanged`.
        let _service = serve_screen_saver(&bus, true).await;
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
    });
}

#[test]
fn a_locked_screen_holds_the_deadline_until_unlock() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_screen_saver(&bus, true).await;
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let general = format!("{ALPHABETICAL}\npause_when_locked = true");
        world.save_config_file(&world.config_text(&general));
        let mut daemon = world.start_daemon_on_bus(bus.address(), SEED).await;
        // Filling an empty slot is a repair: the lock does not hold it.
        let status = world.showing(&mut daemon, "a.png", "x.png").await;
        // `GetActive` may answer after the first apply.
        if status.paused.light != Some(Pause::ScreenLocked) {
            daemon
                .status_where("the screen lock pause", |status| {
                    status.paused.light == Some(Pause::ScreenLocked)
                })
                .await;
        }

        world.clock.advance(LIGHT_INTERVAL + MINUTE);
        let_the_daemon_run().await;
        assert_eq!(world.shown(Style::Light), "a.png");

        emit_active_changed(&service, false).await;
        // Overdue: the rotation comes at once.
        world.showing(&mut daemon, "b.png", "x.png").await;
        daemon.terminate().await;
    });
}
