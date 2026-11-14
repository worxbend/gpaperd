//! The timer: deadlines, early wake-ups and clock steps.

use std::time::Duration;

use gpaperd_core::event::{Effect, Status};
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, id, published, running_world,
    two_folders, two_folders_with,
};

#[test]
fn a_deadline_rotates_the_visible_style_only() {
    let mut world = running_world();

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace), [Some(at(1800))]);
}

#[test]
fn every_rotation_is_saved() {
    let mut world = running_world();

    let trace = world.run(at(900), DEADLINE);

    assert!(trace.contains(&Effect::Persist));
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
}

#[test]
fn every_rotation_is_reported() {
    let mut world = running_world();

    let trace = world.run(at(900), DEADLINE);

    let status = Status {
        visible: Style::Light,
        current: PerStyle {
            light: Some(id("/l/b.jpg")),
            dark: Some(id("/d/a.jpg")),
        },
        next_change: Some(at(1800)),
        paused: PerStyle::default(),
    };
    assert_eq!(trace.last(), Some(&Effect::Report(status)));
}

#[test]
fn rotate_hidden_rotates_both_styles() {
    let mut config = alphabetical();
    config.general.rotate_hidden = true;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Dark, "/d/b.jpg")]
    );
}

#[test]
fn each_style_rotates_on_its_own_interval() {
    let mut config = alphabetical();
    config.general.rotate_hidden = true;
    config.styles.dark.interval = Duration::from_secs(1800);
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);

    let at_light_deadline = world.run(at(900), DEADLINE);
    let at_both_deadlines = world.run(at(1800), DEADLINE);

    assert_eq!(published(&at_light_deadline), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(
        published(&at_both_deadlines),
        [(Style::Light, "/l/c.jpg"), (Style::Dark, "/d/b.jpg")]
    );
}

#[test]
fn a_timer_that_fires_early_is_armed_again() {
    let mut world = running_world();

    let trace = world.run(at(100), DEADLINE);

    assert_eq!(trace, [Effect::Arm(Some(at(900)))]);
}

#[test]
fn a_clock_step_arms_the_timer_again() {
    let mut world = running_world();

    let trace = world.run(at(100), CLOCK_CHANGED);

    assert_eq!(trace, [Effect::Arm(Some(at(900)))]);
}

#[test]
fn a_clock_step_past_the_deadline_rotates() {
    let mut world = running_world();

    let trace = world.run(at(5000), CLOCK_CHANGED);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace).last(), Some(&Some(at(5900))));
}

#[test]
fn a_clock_that_went_back_starts_a_fresh_interval() {
    let mut world = World::new(two_folders());
    world.start(at(1000), Style::Light);

    let trace = world.run(at(400), CLOCK_CHANGED);

    assert!(published(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(1300))]);
}
