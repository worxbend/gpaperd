//! `pause_when_locked` against a fake `org.gnome.ScreenSaver` on a private
//! bus: the watch on its own, then the whole daemon.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpaperd_core::event::{Pause, ScreenLock};
use gpaperd_core::style::Style;
use gpaperd_daemon::dbus::REPLY_TIMEOUT;
use gpaperd_daemon::screensaver;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::Notify;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

use crate::bus::PrivateBus;
use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, MINUTE, PATIENCE, SEED, World, eventually, scenario, start_time,
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

/// `GetActive` never returns. The watch's call timeout is what moves on.
struct HungShield;

#[interface(name = "org.gnome.ScreenSaver")]
impl HungShield {
    async fn get_active(&self) -> bool {
        std::future::pending().await
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

/// The first `GetActive` fails. Later calls report the shield up.
struct FailOnceShield {
    calls: AtomicUsize,
}

#[interface(name = "org.gnome.ScreenSaver")]
impl FailOnceShield {
    fn get_active(&self) -> zbus::fdo::Result<bool> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(zbus::fdo::Error::Failed("not yet".into()));
        }
        Ok(true)
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

async fn serve_hung_shield(bus: &PrivateBus) -> Connection {
    zbus::connection::Builder::address(bus.address())
        .name(SERVICE)
        .serve_at(PATH, HungShield)
        .build()
        .await
        .expect("serve a shield whose GetActive waits forever")
}

/// `GetActive` waits until `release`. `entered` fires when the call starts,
/// so a test can queue a signal while the watch is inside the ask.
struct GateShield {
    active: bool,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[interface(name = "org.gnome.ScreenSaver")]
impl GateShield {
    async fn get_active(&self) -> bool {
        self.entered.notify_one();
        self.release.notified().await;
        self.active
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

async fn serve_gate(
    bus: &PrivateBus,
    active: bool,
    entered: Arc<Notify>,
    release: Arc<Notify>,
) -> Connection {
    zbus::connection::Builder::address(bus.address())
        .name(SERVICE)
        .serve_at(
            PATH,
            GateShield {
                active,
                entered,
                release,
            },
        )
        .build()
        .await
        .expect("serve a shield whose GetActive waits")
}

async fn emit_gate(service: &Connection, active: bool) {
    let shield = service
        .object_server()
        .interface::<_, GateShield>(PATH)
        .await
        .expect("the gated shield is served");
    GateShield::active_changed(shield.signal_emitter(), active)
        .await
        .expect("emit ActiveChanged");
}

async fn serve_fail_once(bus: &PrivateBus) -> Connection {
    zbus::connection::Builder::address(bus.address())
        .name(SERVICE)
        .serve_at(
            PATH,
            FailOnceShield {
                calls: AtomicUsize::new(0),
            },
        )
        .build()
        .await
        .expect("serve a shield whose first GetActive fails")
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

        let armed = world.clock.times_armed();
        world
            .clock
            .step_to(start_time() + LIGHT_INTERVAL + MINUTE);
        eventually("the locked screen's deadline is observed", || {
            world.clock.times_armed() > armed
        })
        .await;
        assert_eq!(world.shown(Style::Light), "a.png");

        emit_active_changed(&service, false).await;
        // Overdue: the rotation comes at once.
        world.showing(&mut daemon, "b.png", "x.png").await;
        daemon.terminate().await;
    });
}

#[test]
fn a_hung_get_active_still_hears_the_next_signal() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_hung_shield(&bus).await;
        let mut locks = screensaver::watch(bus.connect().await);
        // The streams are subscribed before the hung call. The signal waits
        // in that queue until the call timeout drops `GetActive`.
        tokio::time::sleep(QUIET).await;
        let shield = service
            .object_server()
            .interface::<_, HungShield>(PATH)
            .await
            .expect("the hung shield is served");
        HungShield::active_changed(shield.signal_emitter(), true)
            .await
            .expect("emit ActiveChanged");

        let lock = tokio::time::timeout(REPLY_TIMEOUT + PATIENCE, locks.recv()).await;

        assert_eq!(
            lock.expect("the signal after the hung GetActive")
                .expect("the watch to be running"),
            ScreenLock::Locked
        );
    });
}

#[test]
fn a_new_owner_whose_get_active_fails_still_reports_the_initial_lock() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_screen_saver(&bus, true).await;
        let mut locks = screensaver::watch(bus.connect().await);
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
        drop(service);
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Unlocked);

        let _service = serve_fail_once(&bus).await;
        // The failed ask must not keep the previous Locked bit.
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Unlocked);
        let later = tokio::time::timeout(REPLY_TIMEOUT + PATIENCE, locks.recv()).await;

        assert_eq!(
            later.expect("the retry of GetActive").expect("the watch"),
            ScreenLock::Locked
        );
    });
}

#[test]
fn a_buffered_unlock_from_the_previous_shield_does_not_clear_the_new_one() {
    scenario(async {
        let bus = PrivateBus::start();
        let service = serve_screen_saver(&bus, true).await;
        let mut locks = screensaver::watch(bus.connect().await);
        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);

        emit_active_changed(&service, false).await;
        drop(service);
        let _next = serve_screen_saver(&bus, true).await;

        let mut saw_new_lock = false;
        loop {
            match tokio::time::timeout(QUIET, locks.recv()).await {
                Ok(Some(ScreenLock::Unlocked)) if saw_new_lock => {
                    panic!("the previous shield unlocked the new one");
                }
                Ok(Some(ScreenLock::Locked)) => saw_new_lock = true,
                Ok(Some(ScreenLock::Unlocked)) => {}
                Ok(None) => panic!("the watch ended"),
                Err(_) => break,
            }
        }
        assert!(saw_new_lock, "the new shield's GetActive was not reported");
    });
}

#[test]
fn a_queued_lock_after_the_shield_leaves_does_not_stick() {
    scenario(async {
        let bus = PrivateBus::start();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let service = serve_gate(&bus, true, Arc::clone(&entered), Arc::clone(&release)).await;
        let mut locks = screensaver::watch(bus.connect().await);
        tokio::time::timeout(PATIENCE, entered.notified())
            .await
            .expect("GetActive started");
        // Both the signal and the name loss sit in the streams until the
        // ask returns. The owner arm is preferred, then the queued lock.
        emit_gate(&service, true).await;
        drop(service);
        release.notify_one();

        let mut unlocked = false;
        loop {
            let wait = if unlocked { QUIET } else { PATIENCE };
            match tokio::time::timeout(wait, locks.recv()).await {
                Ok(Some(ScreenLock::Locked)) if unlocked => {
                    panic!("a queued lock from the departed shield stuck");
                }
                Ok(Some(ScreenLock::Unlocked)) => unlocked = true,
                Ok(Some(ScreenLock::Locked)) => {}
                Ok(None) => panic!("the watch ended"),
                Err(_) if unlocked => break,
                Err(_) => panic!("a lost owner did not count as unlocked"),
            }
        }
    });
}

#[test]
fn a_change_during_get_active_does_not_replace_that_answer() {
    scenario(async {
        let bus = PrivateBus::start();
        let mut locks = screensaver::watch(bus.connect().await);
        let early = tokio::time::timeout(QUIET, locks.recv()).await;
        assert!(early.is_err(), "no state without the service: {early:?}");

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let service = serve_gate(&bus, true, Arc::clone(&entered), Arc::clone(&release)).await;
        tokio::time::timeout(PATIENCE, entered.notified())
            .await
            .expect("the new owner's GetActive started");
        emit_gate(&service, false).await;
        release.notify_one();

        assert_eq!(next_lock(&mut locks).await, ScreenLock::Locked);
        let trailing = tokio::time::timeout(QUIET, locks.recv()).await;
        assert!(
            trailing.is_err(),
            "a change queued during GetActive replaced it: {trailing:?}"
        );
    });
}

#[test]
fn a_closed_screen_lock_watch_does_not_keep_the_screen_paused() {
    scenario(async {
        let (sender, locks) = mpsc::unbounded_channel();
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let general = format!("{ALPHABETICAL}\npause_when_locked = true");
        world.save_config_file(&world.config_text(&general));
        let mut daemon = world.start_daemon_with_screen_lock(world.config(&general), SEED, locks);
        sender.send(ScreenLock::Locked).expect("the watch is open");
        daemon
            .status_where("the screen lock pause", |status| {
                status.paused.light == Some(Pause::ScreenLocked)
            })
            .await;

        drop(sender);

        daemon
            .status_where("the lock watch to unlock", |status| {
                status.paused.light != Some(Pause::ScreenLocked)
            })
            .await;
        assert!(world.logged("screen lock watch stopped"));
        daemon.terminate().await;
    });
}
