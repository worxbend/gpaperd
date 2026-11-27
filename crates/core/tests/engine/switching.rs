//! Style switches: each clock counts only time on screen.

use std::time::SystemTime;

use gpaperd_core::catalog::FileId;
use gpaperd_core::config::{Config, ExternalChangePolicy};
use gpaperd_core::event::{Effect, Event, Method, SlotWrite};
use gpaperd_core::step::{HIDDEN_REFRESH_DELAY, step};
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, count, is_set_keys, keys_changed, keys_pointed,
    probed, published, reported, republished, running_world, switch_to, two_folders_with,
};

const PICKED: &str = "file:///home/u/Pictures/picked.jpg";

fn refresh_after(switch_secs: u64) -> SystemTime {
    at(switch_secs) + HIDDEN_REFRESH_DELAY
}

fn is_dirty(world: &World, style: Style) -> bool {
    world.model.persisted().styles[style].dirty_while_hidden
}

fn reassert_now() -> Config {
    let mut config = alphabetical();
    config.general.on_external_change = ExternalChangePolicy::ReassertNow;
    config
}

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

#[test]
fn a_refresh_keeps_the_dirty_mark_until_the_new_inode_is_stored() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));

    // The copy is asked for and not answered. The mark stays.
    let issued = step(&mut world.model, refresh_after(300), DEADLINE);
    assert_eq!(republished(&issued), [Style::Dark]);
    assert!(is_dirty(&world, Style::Dark));

    // Dark leaves the screen before the copy lands. Success stores the
    // inode and keeps the mark: the shell did not show this copy.
    step(&mut world.model, at(301), switch_to(Style::Light));
    let stored = FileId { dev: 2, ino: 90 };
    let landed = step(
        &mut world.model,
        at(301),
        Event::Republished {
            style: Style::Dark,
            result: Ok(SlotWrite {
                method: Method::Copy,
                slot: stored,
            }),
        },
    );

    assert!(republished(&landed).is_empty());
    assert!(is_dirty(&world, Style::Dark));
    assert_eq!(world.model.persisted().styles.dark.slot, Some(stored));

    let back = world.run(at(400), switch_to(Style::Dark));
    assert_eq!(armed(&back), [Some(refresh_after(400))]);
}

#[test]
fn a_failed_refresh_lets_the_other_style_write_its_key() {
    let mut world = World::new(two_folders_with(reassert_now()));
    world.start(at(0), Style::Light);
    world.run(at(300), switch_to(Style::Dark));
    let issued = step(&mut world.model, refresh_after(300), DEADLINE);
    assert_eq!(republished(&issued), [Style::Dark]);

    world.keys.picture_uri = PICKED.to_owned();
    let taken = step(&mut world.model, at(301), keys_changed(world.keys.clone()));
    let still_copying = step(&mut world.model, at(302), DEADLINE);

    // The in-flight copy keeps both styles busy. An unrelated wake does
    // not drop that, so the owed key write waits.
    assert_eq!(count(&taken, is_set_keys), 0);
    assert_eq!(count(&still_copying, is_set_keys), 0);
    assert!(is_dirty(&world, Style::Dark));

    let failed = Event::Republished {
        style: Style::Dark,
        result: Err("No space left on device".to_owned()),
    };
    let after = step(&mut world.model, at(303), failed);

    assert_eq!(keys_pointed(&after), [vec![Style::Light]]);
    assert!(!is_dirty(&world, Style::Dark));
}
