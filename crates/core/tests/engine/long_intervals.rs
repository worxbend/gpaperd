//! Intervals of a day or a week: the deadline outlives restarts, reboots,
//! suspend and reloads to a shorter interval.

use std::time::Duration;

use gpaperd_core::config::Config;
use gpaperd_core::event::{Effect, Event};
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, published, reported, switch_to, two_folders_with,
};

const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

/// Both styles in alphabetical order, each changing once a week.
fn weekly() -> Config {
    every(WEEK)
}

/// [`weekly`], with the hidden style's clock running too.
fn weekly_rotating_hidden() -> Config {
    let mut config = weekly();
    config.general.rotate_hidden = true;
    config
}

fn every(secs: u64) -> Config {
    let mut config = alphabetical();
    config.styles.light.interval = Duration::from_secs(secs);
    config.styles.dark.interval = Duration::from_secs(secs);
    config
}

/// A daemon that finished its first run at second 0 with light on screen,
/// on a weekly interval.
fn weekly_world() -> World {
    let mut world = World::new(two_folders_with(weekly()));
    world.start(at(0), Style::Light);
    world
}

fn reloaded(config: Config) -> Event {
    Event::ConfigChanged(Ok(config))
}

#[test]
fn a_weekly_deadline_survives_a_restart() {
    let first = weekly_world();

    let trace = first
        .restart(two_folders_with(weekly()))
        .start(at(3 * DAY), Style::Light);

    assert!(published(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(WEEK))]);
}

#[test]
fn a_weekly_rotation_that_fell_due_while_the_machine_was_off_runs_once_at_start() {
    let first = weekly_world();

    let trace = first
        .restart(two_folders_with(weekly()))
        .start(at(WEEK + DAY), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace), [Some(at(WEEK + DAY + WEEK))]);
}

#[test]
fn a_saved_change_ahead_of_the_clock_waits_no_longer_than_one_interval() {
    // The last run saved its change while the clock was three days ahead.
    let mut first = World::new(two_folders_with(weekly()));
    first.start(at(3 * DAY), Style::Light);

    let trace = first
        .restart(two_folders_with(weekly()))
        .start(at(0), Style::Light);

    assert!(published(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(WEEK))]);
}

#[test]
fn a_hidden_style_keeps_its_time_left_of_a_week_across_a_restart() {
    let mut first = weekly_world();
    first.run(at(2 * DAY), switch_to(Style::Dark));
    let mut second = first.restart(two_folders_with(weekly()));
    second.start(at(4 * DAY), Style::Dark);

    let trace = second.run(at(5 * DAY), switch_to(Style::Light));

    // Light was on screen for two days before the switch: five are left.
    assert_eq!(reported(&trace).next_change, Some(at(10 * DAY)));
}

#[test]
fn a_style_switch_saves_the_stopped_clock() {
    // A crash after the switch must not count days off screen as on screen.
    let mut world = weekly_world();

    let trace = world.run(at(2 * DAY), switch_to(Style::Dark));

    assert!(trace.contains(&Effect::Persist));
}

#[test]
fn a_switch_to_the_style_on_screen_saves_nothing() {
    let mut world = weekly_world();

    let trace = world.run(at(2 * DAY), switch_to(Style::Light));

    assert!(!trace.contains(&Effect::Persist));
}

#[test]
fn a_reload_that_stops_the_hidden_clock_saves_it() {
    let mut world = World::new(two_folders_with(weekly_rotating_hidden()));
    world.start(at(0), Style::Light);

    let trace = world.run(at(DAY), reloaded(weekly()));

    assert!(trace.contains(&Effect::Persist));
}

#[test]
fn a_reload_that_starts_the_hidden_clock_saves_it() {
    let mut world = weekly_world();

    let trace = world.run(at(DAY), reloaded(weekly_rotating_hidden()));

    assert!(trace.contains(&Effect::Persist));
}

#[test]
fn a_reload_that_leaves_the_clocks_alone_saves_nothing() {
    let mut world = weekly_world();

    let trace = world.run(at(DAY), reloaded(weekly()));

    assert!(!trace.contains(&Effect::Persist));
}

#[test]
fn a_reload_to_a_shorter_interval_waits_no_longer_than_the_new_one() {
    let mut world = weekly_world();

    let trace = world.run(at(HOUR / 2), reloaded(every(HOUR)));

    assert!(published(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(HOUR))]);
}

#[test]
fn a_reload_to_an_interval_that_is_over_already_rotates_at_once() {
    let mut world = weekly_world();

    let trace = world.run(at(2 * DAY), reloaded(every(DAY)));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace).last(), Some(&Some(at(3 * DAY))));
}

#[test]
fn a_hidden_styles_week_left_is_cut_to_a_shorter_interval_from_a_reload() {
    // Dark is hidden with its whole week left.
    let mut world = weekly_world();
    world.run(at(DAY), reloaded(every(HOUR)));

    let trace = world.run(at(DAY + 60), switch_to(Style::Dark));

    assert_eq!(reported(&trace).next_change, Some(at(DAY + 60 + HOUR)));
}

#[test]
fn a_daily_deadline_that_a_suspend_delayed_rotates_once_on_resume() {
    let mut world = World::new(two_folders_with(every(DAY)));
    world.start(at(0), Style::Light);

    // The machine slept through two deadlines; the timer fires on resume.
    let trace = world.run(at(3 * DAY), DEADLINE);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace), [Some(at(4 * DAY))]);
}
