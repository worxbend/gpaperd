//! The refresh of a slot that was written while its style was hidden: the
//! shell may hold a stale texture for it.

use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::FileId;
use gpaperd_core::config::Config;
use gpaperd_core::event::{Effect, Event, Method, SlotWrite};
use gpaperd_core::step::{HIDDEN_REFRESH_DELAY, step};
use gpaperd_core::style::Style;

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, id, probed, published, republished,
    running_world, switch_to, two_folders, two_folders_with,
};

/// The instant at which a slot is refreshed after a switch at `switch_secs`.
fn refresh_after(switch_secs: u64) -> SystemTime {
    at(switch_secs) + HIDDEN_REFRESH_DELAY
}

fn is_dirty(world: &World, style: Style) -> bool {
    world.model.persisted().styles[style].dirty_while_hidden
}

fn rotate_hidden() -> Config {
    let mut config = alphabetical();
    config.general.rotate_hidden = true;
    config
}

#[test]
fn a_slot_that_is_written_while_hidden_is_dirty() {
    let world = running_world();

    assert!(is_dirty(&world, Style::Dark));
    assert!(!is_dirty(&world, Style::Light));
}

#[test]
fn a_switch_to_a_dirty_slot_republishes_it_500_ms_later() {
    let mut world = running_world();

    let at_switch = world.run(at(300), switch_to(Style::Dark));
    let at_refresh = world.run(refresh_after(300), DEADLINE);

    assert!(republished(&at_switch).is_empty());
    assert_eq!(armed(&at_switch), [Some(refresh_after(300))]);
    assert_eq!(republished(&at_refresh), [Style::Dark]);
    // The content is the same: no image is probed or published.
    assert!(probed(&at_refresh).is_empty());
    assert!(published(&at_refresh).is_empty());
}

#[test]
fn the_refresh_clears_the_mark_and_saves_the_new_inode() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));

    let at_refresh = world.run(refresh_after(300), DEADLINE);

    let saved = world.model.persisted().styles.dark;
    assert!(!saved.dirty_while_hidden);
    assert_eq!(saved.slot, world.slots.dark);
    assert!(at_refresh.contains(&Effect::Persist));
}

#[test]
fn the_refresh_does_not_change_the_interval() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));

    let at_refresh = world.run(refresh_after(300), DEADLINE);

    // Dark was never on screen before: its whole interval is left.
    assert_eq!(armed(&at_refresh), [Some(at(1200))]);
}

#[test]
fn a_slot_is_refreshed_once() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    world.run(refresh_after(300), DEADLINE);
    world.run(at(400), switch_to(Style::Light));

    let back_on_dark = world.run(at(500), switch_to(Style::Dark));
    let later = world.run(refresh_after(500), DEADLINE);

    assert!(republished(&back_on_dark).is_empty());
    assert!(republished(&later).is_empty());
}

#[test]
fn a_style_that_leaves_the_screen_before_its_refresh_keeps_the_mark() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));

    let away = world.run(
        at(300) + Duration::from_millis(200),
        switch_to(Style::Light),
    );
    let at_old_refresh = world.run(refresh_after(300), DEADLINE);
    world.run(at(400), switch_to(Style::Dark));
    let at_new_refresh = world.run(refresh_after(400), DEADLINE);

    assert!(republished(&away).is_empty());
    assert!(republished(&at_old_refresh).is_empty());
    assert_eq!(republished(&at_new_refresh), [Style::Dark]);
}

#[test]
fn a_clean_slot_is_not_refreshed_at_a_switch() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    world.run(refresh_after(300), DEADLINE);

    // Light was written while it was on screen.
    let to_light = world.run(at(400), switch_to(Style::Light));

    assert_eq!(armed(&to_light), [Some(at(1000))]);
    assert!(!is_dirty(&world, Style::Light));
}

#[test]
fn with_rotate_hidden_every_hidden_rotation_makes_the_slot_dirty() {
    let mut world = World::new(two_folders_with(rotate_hidden()));
    world.start(at(0), Style::Light);
    world.run(at(100), switch_to(Style::Dark));
    world.run(refresh_after(100), DEADLINE);
    world.run(at(200), switch_to(Style::Light));
    world.run(refresh_after(200), DEADLINE);
    assert!(!is_dirty(&world, Style::Dark));

    // Dark rotates while light is on screen.
    let at_deadline = world.run(at(900), DEADLINE);
    world.run(at(1000), switch_to(Style::Dark));
    let at_refresh = world.run(refresh_after(1000), DEADLINE);

    assert!(published(&at_deadline).contains(&(Style::Dark, "/d/b.jpg")));
    assert_eq!(republished(&at_refresh), [Style::Dark]);
}

#[test]
fn a_dirty_slot_that_is_on_screen_at_start_is_refreshed() {
    let first = running_world();

    // The style changed while the daemon was off.
    let mut second = first.restart(two_folders());
    let at_start = second.start(at(60), Style::Dark);
    let at_refresh = second.run(refresh_after(60), DEADLINE);

    assert!(armed(&at_start).contains(&Some(refresh_after(60))));
    assert_eq!(republished(&at_refresh), [Style::Dark]);
}

#[test]
fn the_refresh_waits_for_a_rotation_in_flight() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    // Dark leaves the screen 100 ms before its deadline. No event came in
    // between, so its refresh never ran.
    world.run(
        at(1200) - Duration::from_millis(100),
        switch_to(Style::Light),
    );
    step(&mut world.model, at(2000), switch_to(Style::Dark));
    let rotating = step(
        &mut world.model,
        at(2000) + Duration::from_millis(100),
        DEADLINE,
    );
    assert_eq!(probed(&rotating), [(Style::Dark, "/d/b.jpg")]);

    let at_refresh = step(&mut world.model, refresh_after(2000), DEADLINE);
    let probe_passed = Event::Probed {
        style: Style::Dark,
        item: id("/d/b.jpg"),
        verdict: Ok(()),
    };
    let publishing = step(&mut world.model, at(2001), probe_passed);
    let published_ok = Event::Published {
        style: Style::Dark,
        item: id("/d/b.jpg"),
        result: Ok(SlotWrite {
            method: Method::Copy,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };
    let after_publish = step(&mut world.model, at(2002), published_ok);

    assert!(republished(&at_refresh).is_empty());
    assert!(republished(&publishing).is_empty());
    assert_eq!(republished(&after_publish), [Style::Dark]);
}

#[test]
fn no_rotation_starts_while_a_refresh_is_in_flight() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    let refreshing = step(&mut world.model, refresh_after(300), DEADLINE);
    assert_eq!(republished(&refreshing), [Style::Dark]);

    // Dark's deadline passes before the republish is answered.
    let at_deadline = step(&mut world.model, at(1200), DEADLINE);
    let refreshed = Event::Republished {
        style: Style::Dark,
        result: Ok(SlotWrite {
            method: Method::Copy,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };
    let after_refresh = step(&mut world.model, at(1201), refreshed);

    assert!(probed(&at_deadline).is_empty());
    assert_eq!(probed(&after_refresh), [(Style::Dark, "/d/b.jpg")]);
}

#[test]
fn a_slot_that_is_lost_during_its_refresh_is_filled_after_it() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    step(&mut world.model, refresh_after(300), DEADLINE);
    let nothing_to_copy = Event::Republished {
        style: Style::Dark,
        result: Err("No such file or directory".to_owned()),
    };

    let at_loss = step(&mut world.model, at(301), Event::SlotLost(Style::Dark));
    let after_refresh = world.run(at(301), nothing_to_copy);

    assert!(probed(&at_loss).is_empty());
    assert_eq!(published(&after_refresh), [(Style::Dark, "/d/a.jpg")]);
}

#[test]
fn a_refresh_that_failed_is_not_tried_again() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));
    step(&mut world.model, refresh_after(300), DEADLINE);
    let failed = Event::Republished {
        style: Style::Dark,
        result: Err("No space left on device".to_owned()),
    };

    let after_failure = step(&mut world.model, at(301), failed);
    let later = world.run(at(302), CLOCK_CHANGED);

    assert!(republished(&after_failure).is_empty());
    assert!(republished(&later).is_empty());
    assert!(!is_dirty(&world, Style::Dark));
}

#[test]
fn a_republish_answer_that_nobody_waits_for_is_ignored() {
    let mut world = running_world();
    let stray = Event::Republished {
        style: Style::Light,
        result: Ok(SlotWrite {
            method: Method::Copy,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };

    let trace = world.run(at(100), stray);

    assert!(trace.is_empty());
    assert_eq!(world.model.persisted().styles.light.slot, world.slots.light);
}

#[test]
fn a_clock_that_goes_back_does_not_delay_the_refresh() {
    let mut world = running_world();
    world.run(at(300), switch_to(Style::Dark));

    let at_clock_step = world.run(at(100), CLOCK_CHANGED);
    let at_refresh = world.run(refresh_after(100), DEADLINE);

    assert_eq!(armed(&at_clock_step), [Some(refresh_after(100))]);
    assert_eq!(republished(&at_refresh), [Style::Dark]);
}
