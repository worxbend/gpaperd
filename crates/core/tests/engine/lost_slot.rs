//! A slot file that someone removed is filled again at once.

use gpaperd_core::catalog::BadReason;
use gpaperd_core::event::Event;
use gpaperd_core::step::step;
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, alphabetical, armed, at, changed, count, found, id, is_publish, is_set_keys, probed,
    published, removed, running_world, unscanned,
};

#[test]
fn a_lost_slot_is_filled_again_at_once_with_the_image_it_showed() {
    let mut world = running_world();

    let trace = world.run(at(100), Event::SlotLost(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    // The key still names the slot, and the image keeps its interval.
    assert_eq!(count(&trace, is_set_keys), 0);
    assert_eq!(armed(&trace).last(), Some(&Some(at(900))));
}

#[test]
fn a_lost_slot_of_the_hidden_style_is_filled_again_too() {
    let mut world = running_world();

    let trace = world.run(at(100), Event::SlotLost(Style::Dark));

    assert_eq!(published(&trace), [(Style::Dark, "/d/a.jpg")]);
}

#[test]
fn a_lost_slot_whose_image_is_gone_gets_the_next_image() {
    let mut world = running_world();
    world.run(at(100), changed(Style::Light, vec![removed("/l/a.jpg")]));

    let trace = world.run(at(200), Event::SlotLost(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_slot_lost_during_a_rotation_is_filled_by_that_rotation() {
    let mut world = running_world();
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Ok(()),
    };
    step(&mut world.model, at(900), DEADLINE);

    let at_loss = step(&mut world.model, at(901), Event::SlotLost(Style::Light));
    let after_probe = step(&mut world.model, at(902), probe_passed);

    assert!(probed(&at_loss).is_empty());
    assert_eq!(published(&after_probe), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_slot_lost_before_the_scan_is_filled_from_the_saved_item() {
    let mut second = running_world().restart(unscanned(alphabetical()));
    second.start(at(60), Style::Light);

    let trace = second.run(at(61), Event::SlotLost(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn a_lost_slot_with_no_image_for_it_is_filled_when_one_appears() {
    let mut world = running_world();
    let everything = vec![
        removed("/l/a.jpg"),
        removed("/l/b.jpg"),
        removed("/l/c.jpg"),
    ];
    world.run(at(100), changed(Style::Light, everything));
    let dark_too = vec![
        removed("/d/a.jpg"),
        removed("/d/b.jpg"),
        removed("/d/c.jpg"),
    ];
    world.run(at(101), changed(Style::Dark, dark_too));

    let at_loss = world.run(at(200), Event::SlotLost(Style::Light));
    let at_new_image = world.run(at(300), changed(Style::Light, vec![found("/l/new.jpg")]));

    assert_eq!(count(&at_loss, is_publish), 0);
    assert_eq!(published(&at_new_image), [(Style::Light, "/l/new.jpg")]);
}

#[test]
fn a_slot_lost_during_a_rotation_that_finds_nothing_gets_its_image_back() {
    let mut world = running_world();
    world.broken = [id("/l/b.jpg"), id("/l/c.jpg")].into();
    let probe_failed = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Err(BadReason::NotAnImage),
    };
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), Event::SlotLost(Style::Light));

    let trace = world.run(at(902), probe_failed);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}
