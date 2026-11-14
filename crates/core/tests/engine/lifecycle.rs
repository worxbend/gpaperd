//! Lifecycle: events before `Started` and after `Shutdown`.

use gpaperd_core::event::{Effect, Event};
use gpaperd_core::style::Style;

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, at, id, published, running_world, switch_to, two_folders,
};

#[test]
fn events_before_started_are_ignored() {
    let mut world = World::new(two_folders());
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id("/l/a.jpg"),
        verdict: Ok(()),
    };

    for early in [
        DEADLINE,
        CLOCK_CHANGED,
        switch_to(Style::Dark),
        probe_passed,
    ] {
        assert!(world.run(at(0), early).is_empty());
    }
    let trace = world.start(at(1), Style::Light);

    assert_eq!(published(&trace).len(), 2);
}

#[test]
fn a_second_started_event_starts_nothing() {
    let mut world = running_world();

    let trace = world.start(at(1), Style::Dark);

    assert!(trace.is_empty());
}

#[test]
fn shutdown_disarms_the_timer_and_saves_the_state() {
    let mut world = running_world();

    let trace = world.run(at(10), Event::Shutdown);

    assert_eq!(trace, [Effect::Arm(None), Effect::Persist]);
}

#[test]
fn nothing_happens_after_shutdown() {
    let mut world = running_world();
    world.run(at(10), Event::Shutdown);

    assert!(world.run(at(900), DEADLINE).is_empty());
    assert!(world.run(at(901), switch_to(Style::Dark)).is_empty());
    assert!(world.run(at(902), Event::Shutdown).is_empty());
}

#[test]
fn shutdown_before_started_saves_nothing_and_ends_the_run() {
    let mut world = World::new(two_folders());

    let at_shutdown = world.run(at(0), Event::Shutdown);
    let at_late_start = world.start(at(1), Style::Light);

    assert!(at_shutdown.is_empty());
    assert!(at_late_start.is_empty());
}
