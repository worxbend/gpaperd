//! Scheduler maths: deadlines, clock steps, and time left across hide and show.

use std::time::Duration;

use gpaperd_core::schedule::StyleClock;

use crate::support::at;

const INTERVAL: Duration = Duration::from_secs(900);

fn clock_changed_at(secs: u64) -> StyleClock {
    let mut clock = StyleClock::default();
    clock.record_change(at(secs));
    clock
}

#[test]
fn a_clock_that_never_started_has_no_deadline() {
    assert_eq!(StyleClock::default().deadline(INTERVAL), None);
}

#[test]
fn the_deadline_is_one_interval_after_the_last_change() {
    let clock = clock_changed_at(100);

    assert_eq!(clock.deadline(INTERVAL), Some(at(1000)));
}

#[test]
fn a_change_starts_a_fresh_interval() {
    let mut clock = clock_changed_at(100);

    clock.record_change(at(700));

    assert_eq!(clock.deadline(INTERVAL), Some(at(1600)));
}

#[test]
fn a_change_restarts_a_stopped_clock() {
    let mut clock = clock_changed_at(0);
    clock.hide(at(300), INTERVAL);

    clock.record_change(at(400));

    assert_eq!(clock.remaining, None);
    assert_eq!(clock.deadline(INTERVAL), Some(at(1300)));
}

#[test]
fn a_clock_that_went_back_starts_a_fresh_interval() {
    let mut clock = clock_changed_at(1000);

    clock.absorb_clock_step(at(400));

    assert_eq!(clock.last_change, Some(at(400)));
    assert_eq!(clock.deadline(INTERVAL), Some(at(1300)));
}

#[test]
fn a_clock_that_went_forward_keeps_its_interval() {
    let mut clock = clock_changed_at(1000);

    clock.absorb_clock_step(at(5000));

    assert_eq!(clock.last_change, Some(at(1000)));
}

#[test]
fn a_clock_overdue_at_start_has_a_deadline_in_the_past() {
    let clock = clock_changed_at(0);
    let daemon_start = at(10_000);

    assert!(
        clock
            .deadline(INTERVAL)
            .is_some_and(|due| due <= daemon_start)
    );
}

#[test]
fn hiding_stores_the_time_left_and_stops_the_clock() {
    let mut clock = clock_changed_at(0);

    clock.hide(at(300), INTERVAL);

    assert_eq!(clock.remaining, Some(Duration::from_secs(600)));
    assert_eq!(clock.deadline(INTERVAL), None);
}

#[test]
fn hiding_an_overdue_clock_stores_zero() {
    let mut clock = clock_changed_at(0);

    clock.hide(at(2000), INTERVAL);

    assert_eq!(clock.remaining, Some(Duration::ZERO));
}

#[test]
fn hiding_twice_keeps_the_first_time_left() {
    let mut clock = clock_changed_at(0);
    clock.hide(at(300), INTERVAL);

    clock.hide(at(800), INTERVAL);

    assert_eq!(clock.remaining, Some(Duration::from_secs(600)));
}

#[test]
fn hiding_a_clock_that_never_started_changes_nothing() {
    let mut clock = StyleClock::default();

    clock.hide(at(300), INTERVAL);

    assert_eq!(clock, StyleClock::default());
}

#[test]
fn showing_resumes_with_the_time_left() {
    let mut clock = clock_changed_at(0);
    clock.hide(at(300), INTERVAL);

    clock.show(at(5000), INTERVAL);

    assert_eq!(clock.remaining, None);
    assert_eq!(clock.deadline(INTERVAL), Some(at(5600)));
}

#[test]
fn the_interval_counts_only_time_on_screen() {
    let mut clock = clock_changed_at(0);

    clock.hide(at(300), INTERVAL);
    clock.show(at(5000), INTERVAL);
    clock.hide(at(5100), INTERVAL);
    clock.show(at(9000), INTERVAL);

    // 300 s + 100 s were on screen, so 500 s are left.
    assert_eq!(clock.deadline(INTERVAL), Some(at(9500)));
}

#[test]
fn showing_a_running_clock_changes_nothing() {
    let mut clock = clock_changed_at(0);

    clock.show(at(300), INTERVAL);

    assert_eq!(clock, clock_changed_at(0));
}

#[test]
fn showing_never_waits_longer_than_the_interval() {
    let mut clock = clock_changed_at(0);
    clock.hide(at(0), Duration::from_secs(3600));

    // A config change shortened the interval while the style was hidden.
    clock.show(at(100), INTERVAL);

    assert_eq!(clock.deadline(INTERVAL), Some(at(1000)));
}
