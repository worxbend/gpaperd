//! Style switches: each clock counts only time on screen.

use gpaperd_core::event::Effect;
use gpaperd_core::style::Style;

use crate::support::{
    World, alphabetical, armed, at, probed, published, reported, running_world, switch_to,
    two_folders_with,
};

#[test]
fn a_style_switch_keeps_each_styles_time_on_screen() {
    let mut world = running_world();

    let to_dark = world.run(at(300), switch_to(Style::Dark));
    let to_light = world.run(at(400), switch_to(Style::Light));

    // Dark was never on screen: its whole interval is left. The timer is
    // armed for something sooner: the refresh of dark's slot.
    assert_eq!(reported(&to_dark).next_change, Some(at(1200)));
    // Light had 600 s left when it went off screen.
    assert_eq!(armed(&to_light), [Some(at(1000))]);
}

#[test]
fn a_style_switch_never_changes_an_image() {
    let mut world = running_world();

    let trace = world.run(at(300), switch_to(Style::Dark));

    assert!(probed(&trace).is_empty());
    assert!(published(&trace).is_empty());
}

#[test]
fn a_style_switch_is_reported() {
    let mut world = running_world();

    let trace = world.run(at(300), switch_to(Style::Dark));

    let Some(Effect::Report(status)) = trace.last() else {
        panic!("no status after a style switch: {trace:?}");
    };
    assert_eq!(status.visible, Style::Dark);
}

#[test]
fn a_switch_to_the_visible_style_does_nothing() {
    let mut world = running_world();

    let trace = world.run(at(300), switch_to(Style::Light));

    assert!(trace.is_empty());
}

#[test]
fn with_rotate_hidden_a_style_switch_leaves_both_clocks_running() {
    let mut config = alphabetical();
    config.general.rotate_hidden = true;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);

    let trace = world.run(at(300), switch_to(Style::Dark));

    // Light's deadline is the next one, as before the switch.
    assert_eq!(reported(&trace).next_change, Some(at(900)));
    assert_eq!(world.model.persisted().styles.light.remaining, None);
}
