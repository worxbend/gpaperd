//! `pause_when_locked`: a locked screen pauses the timers of both styles.

use gpaperd_core::config::Config;
use gpaperd_core::control::{Command, Reply, StyleTarget};
use gpaperd_core::event::{Event, Pause, ScreenLock};
use gpaperd_core::model::Model;
use gpaperd_core::step::step;
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, id, probed, published, reported, running_world,
    the_reply, two_folders_with,
};

const LOCKED: Event = Event::ScreenLock(ScreenLock::Locked);
const UNLOCKED: Event = Event::ScreenLock(ScreenLock::Unlocked);

/// Alphabetical order, 900 s interval, `pause_when_locked = true`.
fn pausing_when_locked() -> Config {
    let mut config = alphabetical();
    config.general.pause_when_locked = true;
    config
}

/// A daemon with `pause_when_locked` that finished its first run at second
/// 0 with light on screen.
fn locking_world() -> World {
    let mut world = World::new(two_folders_with(pausing_when_locked()));
    world.start(at(0), Style::Light);
    world
}

#[test]
fn a_lock_stops_the_timer_at_its_deadline() {
    let mut world = locking_world();

    let at_lock = world.run(at(10), LOCKED);
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(probed(&at_deadline).is_empty());
    assert_eq!(armed(&at_lock), [None]);
    assert_eq!(reported(&at_lock).paused.light, Some(Pause::ScreenLocked));
    assert_eq!(reported(&at_lock).paused.dark, Some(Pause::ScreenLocked));
}

#[test]
fn an_unlock_when_overdue_rotates_at_once() {
    let mut world = locking_world();
    world.run(at(10), LOCKED);

    let trace = world.run(at(1000), UNLOCKED);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(reported(&trace).paused.light, None);
}

#[test]
fn an_unlock_before_the_deadline_keeps_it() {
    let mut world = locking_world();
    world.run(at(10), LOCKED);

    let trace = world.run(at(500), UNLOCKED);

    assert!(probed(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn an_unlock_does_not_end_a_user_pause() {
    let mut world = locking_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    let at_lock = world.run(at(10), LOCKED);

    let at_unlock = world.run(at(1000), UNLOCKED);
    let at_resume = world.ask(at(1001), 2, Command::Resume(StyleTarget::Light));

    assert_eq!(reported(&at_lock).paused.light, Some(Pause::User));
    assert!(probed(&at_unlock).is_empty());
    assert_eq!(published(&at_resume), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn status_lists_the_lock_next_to_a_user_pause() {
    let mut world = locking_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    world.run(at(10), LOCKED);

    let trace = world.ask(at(11), 2, Command::Status);

    let Reply::Status(report) = the_reply(&trace) else {
        panic!("expected a status");
    };
    assert_eq!(
        report.styles.light.paused,
        [Pause::User, Pause::ScreenLocked]
    );
    assert_eq!(report.styles.dark.paused, [Pause::ScreenLocked]);
}

#[test]
fn a_resume_while_locked_keeps_the_lock_pause() {
    let mut world = locking_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    world.run(at(10), LOCKED);

    let trace = world.ask(at(1000), 2, Command::Resume(StyleTarget::Light));

    assert!(probed(&trace).is_empty());
    assert_eq!(reported(&trace).paused.light, Some(Pause::ScreenLocked));
}

#[test]
fn without_the_setting_a_lock_changes_nothing() {
    let mut world = running_world();

    let at_lock = world.run(at(10), LOCKED);
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(at_lock.is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_lock_before_start_counts() {
    let mut world = World::new(two_folders_with(pausing_when_locked()));
    world.run(at(0), LOCKED);
    let at_start = world.start(at(0), Style::Light);

    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(reported(&at_start).paused.light, Some(Pause::ScreenLocked));
    assert!(probed(&at_deadline).is_empty());
}

#[test]
fn a_rotation_in_flight_when_the_screen_locks_is_not_published() {
    let mut world = locking_world();
    let at_deadline = step(&mut world.model, at(900), DEADLINE);
    assert_eq!(probed(&at_deadline), [(Style::Light, "/l/b.jpg")]);

    world.run(at(901), LOCKED);
    let trace = world.run(
        at(902),
        Event::Probed {
            style: Style::Light,
            item: id("/l/b.jpg"),
            verdict: Ok(()),
        },
    );

    assert!(published(&trace).is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn next_while_locked_still_rotates() {
    let mut world = locking_world();
    world.run(at(10), LOCKED);

    let trace = world.ask(at(20), 1, Command::Next(StyleTarget::Visible));

    assert_eq!(the_reply(&trace), &Reply::Done);
    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_reload_that_turns_the_setting_on_while_locked_pauses() {
    let mut world = running_world();
    world.run(at(10), LOCKED);

    let at_reload = world.run(at(20), Event::ConfigChanged(Ok(pausing_when_locked())));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(reported(&at_reload).paused.light, Some(Pause::ScreenLocked));
    assert!(probed(&at_deadline).is_empty());
}

#[test]
fn a_reload_that_turns_the_setting_off_while_locked_rotates_when_overdue() {
    let mut world = locking_world();
    world.run(at(10), LOCKED);

    let trace = world.run(at(1000), Event::ConfigChanged(Ok(alphabetical())));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_lock_after_shutdown_is_ignored() {
    let mut model = Model::new(two_folders_with(pausing_when_locked()));
    step(&mut model, at(0), Event::Shutdown);

    assert!(step(&mut model, at(1), LOCKED).is_empty());
}
