//! File changes while the daemon runs: images that come, go and change.

use std::collections::BTreeSet;
use std::time::Duration;

use gpaperd_core::catalog::{BadReason, FileId, FsChange, ItemId};
use gpaperd_core::config::{Config, EmptyStylePolicy};
use gpaperd_core::event::{Effect, Event, Method, SlotWrite};
use gpaperd_core::model::ModelInit;
use gpaperd_core::step::step;
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, changed, count, found, id, init, is_publish,
    is_set_keys, keys_pointed, linked, probed, published, removed, running_world, six_shuffled,
    version,
};

/// The light image after an edit in place.
fn edited(path: &str) -> Event {
    changed(Style::Light, vec![FsChange::Found(version(path, 1))])
}

fn probe_passed(path: &str) -> Event {
    Event::Probed {
        style: Style::Light,
        item: id(path),
        verdict: Ok(()),
    }
}

/// A daemon over one light and one dark image, light on screen.
fn single_images(config: Config) -> World {
    let mut world = World::new(init(config, &["/l/only.jpg"], &["/d/only.jpg"]));
    world.start(at(0), Style::Light);
    world
}

fn keep_current() -> Config {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    config
}

#[test]
fn a_new_image_sorts_into_the_alphabetical_order() {
    let mut world = running_world();

    let at_change = world.run(at(100), changed(Style::Light, vec![found("/l/ab.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(at_change.is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/ab.jpg")]);
}

#[test]
fn a_new_image_joins_the_shuffle_round_in_progress() {
    let new = id("/l/new.jpg");
    let mut places_of_new = BTreeSet::new();

    for seed in 0..40 {
        let mut world = World::new(ModelInit {
            seed,
            ..six_shuffled()
        });
        world.start(at(0), Style::Light);

        world.run(at(100), changed(Style::Light, vec![found("/l/new.jpg")]));

        let rest_of_round = world.model.persisted().styles.light.bag;
        assert_eq!(rest_of_round.len(), 6, "seed {seed}");
        places_of_new.insert(rest_of_round.iter().position(|id| *id == new));
    }

    assert_eq!(places_of_new.len(), 6);
}

#[test]
fn an_image_that_left_and_came_back_has_no_second_turn_in_its_round() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let first_shown = world.current(Style::Light).expect("the first apply");
    world.run(at(900), DEADLINE);

    world.run(
        at(1000),
        changed(Style::Light, vec![removed(first_shown.as_str())]),
    );
    world.run(
        at(1001),
        changed(Style::Light, vec![found(first_shown.as_str())]),
    );

    let rest_of_round = world.model.persisted().styles.light.bag;
    assert_eq!(rest_of_round.len(), 4);
    assert!(!rest_of_round.contains(&first_shown));
}

#[test]
fn a_removed_image_is_skipped_in_alphabetical_order() {
    let mut world = running_world();

    world.run(at(100), changed(Style::Light, vec![removed("/l/b.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(probed(&at_deadline), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_removed_image_is_skipped_by_the_shuffle_round() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let rest_of_round = world.model.persisted().styles.light.bag;
    let (next, after_next) = (&rest_of_round[0], &rest_of_round[1]);

    world.run(at(100), changed(Style::Light, vec![removed(next.as_str())]));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(probed(&at_deadline), [(Style::Light, after_next.as_str())]);
}

#[test]
fn a_deleted_current_image_changes_nothing_until_the_next_tick() {
    let mut world = running_world();

    let at_change = world.run(at(100), changed(Style::Light, vec![removed("/l/a.jpg")]));

    assert!(at_change.is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn the_cursor_resumes_after_a_deleted_current_image() {
    let mut world = running_world();
    world.run(at(900), DEADLINE);
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));

    world.run(at(1000), changed(Style::Light, vec![removed("/l/b.jpg")]));
    let at_deadline = world.run(at(1800), DEADLINE);

    assert_eq!(published(&at_deadline), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn the_current_image_edited_in_place_is_published_again() {
    let mut world = running_world();
    let slot_before = world.slots.light;

    let trace = world.run(at(100), edited("/l/a.jpg"));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_ne!(world.slots.light, slot_before);
    assert_eq!(count(&trace, is_set_keys), 0);
    // It is the same image, so its interval keeps running.
    assert_eq!(armed(&trace).last(), Some(&Some(at(900))));
}

#[test]
fn the_current_image_replaced_by_an_atomic_save_is_published_again() {
    let mut world = running_world();
    let saved = FsChange::Found(linked("/l/a.jpg", 7));

    let trace = world.run(at(100), changed(Style::Light, vec![saved]));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn an_edit_of_an_image_that_is_not_on_screen_publishes_nothing() {
    let mut world = running_world();

    let trace = world.run(at(100), edited("/l/b.jpg"));

    assert!(trace.is_empty());
}

#[test]
fn the_hidden_styles_image_is_published_again_after_an_edit_too() {
    let mut world = running_world();
    let dark_edit = FsChange::Found(version("/d/a.jpg", 1));

    let trace = world.run(at(100), changed(Style::Dark, vec![dark_edit]));

    assert_eq!(published(&trace), [(Style::Dark, "/d/a.jpg")]);
    // Dark stays hidden with its whole interval left.
    let dark = world.model.persisted().styles.dark;
    assert_eq!(dark.remaining, Some(Duration::from_secs(900)));
}

#[test]
fn a_failed_probe_of_a_file_that_changed_since_is_not_held_against_it() {
    let mut world = running_world();
    let old_content_rejected = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Err(BadReason::NotAnImage),
    };
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), edited("/l/b.jpg"));

    let after_probe = step(&mut world.model, at(902), old_content_rejected);

    assert_eq!(probed(&after_probe), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_edit_gives_the_image_on_screen_no_second_turn_in_its_round() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let on_screen = world.current(Style::Light).expect("the first apply");

    world.run(at(100), edited(on_screen.as_str()));

    let rest_of_round = world.model.persisted().styles.light.bag;
    assert_eq!(rest_of_round.len(), 5);
    assert!(!rest_of_round.contains(&on_screen));
}

#[test]
fn an_edited_image_that_no_longer_validates_stays_until_the_next_tick() {
    let mut world = running_world();
    world.broken.insert(id("/l/a.jpg"));

    let at_edit = world.run(at(100), edited("/l/a.jpg"));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(probed(&at_edit), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(count(&at_edit, is_publish), 0);
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_edit_gives_a_bad_image_another_probe() {
    let mut world = running_world();
    world.broken.insert(id("/l/b.jpg"));
    world.run(at(900), DEADLINE);
    assert_eq!(world.current(Style::Light), Some(id("/l/c.jpg")));
    world.broken.clear();

    world.run(at(1000), edited("/l/b.jpg"));
    world.run(at(1800), DEADLINE);
    let trace = world.run(at(2700), DEADLINE);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_image_that_changes_under_its_probe_is_probed_again() {
    let mut world = running_world();
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), edited("/l/b.jpg"));

    let after_probe = step(&mut world.model, at(902), probe_passed("/l/b.jpg"));

    assert_eq!(probed(&after_probe), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(count(&after_probe, is_publish), 0);
}

#[test]
fn an_image_removed_under_its_probe_is_not_published() {
    let mut world = running_world();
    let gone = changed(Style::Light, vec![removed("/l/b.jpg")]);
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), gone);

    let after_probe = step(&mut world.model, at(902), probe_passed("/l/b.jpg"));

    assert_eq!(probed(&after_probe), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(count(&after_probe, is_publish), 0);
}

#[test]
fn an_image_that_changes_on_its_way_into_the_slot_is_published_again() {
    let mut world = running_world();
    let written = Event::Published {
        style: Style::Light,
        item: id("/l/b.jpg"),
        result: Ok(SlotWrite {
            method: Method::Hardlink,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), probe_passed("/l/b.jpg"));
    step(&mut world.model, at(902), edited("/l/b.jpg"));

    let after_publish = step(&mut world.model, at(903), written);

    assert_eq!(probed(&after_publish), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_second_image_starts_the_rotation() {
    let mut world = single_images(alphabetical());

    let at_change = world.run(at(100), changed(Style::Light, vec![found("/l/second.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(armed(&at_change), [Some(at(900))]);
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/second.jpg")]);
}

#[test]
fn a_second_image_is_shown_at_once_when_the_first_is_overdue() {
    let mut world = single_images(alphabetical());

    let trace = world.run(
        at(5000),
        changed(Style::Light, vec![found("/l/second.jpg")]),
    );

    assert_eq!(published(&trace), [(Style::Light, "/l/second.jpg")]);
}

#[test]
fn a_style_that_shrinks_to_one_image_stops_rotating() {
    let mut world = running_world();
    let all_but_one = vec![removed("/l/b.jpg"), removed("/l/c.jpg")];

    let at_change = world.run(at(100), changed(Style::Light, all_but_one));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(armed(&at_change), [None]);
    assert!(at_deadline.is_empty());
}

#[test]
fn a_style_that_shrinks_to_no_image_turns_to_the_other_styles_images() {
    let mut world = single_images(alphabetical());

    let at_change = world.run(at(100), changed(Style::Light, vec![removed("/l/only.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    // The slot keeps the deleted image until the tick.
    assert!(probed(&at_change).is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/d/only.jpg")]);
}

#[test]
fn with_keep_current_a_style_that_shrinks_to_no_image_keeps_its_slot() {
    let mut world = World::new(init(
        keep_current(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg"],
    ));
    world.start(at(0), Style::Light);
    let everything = vec![removed("/l/a.jpg"), removed("/l/b.jpg")];

    let at_change = world.run(at(100), changed(Style::Light, everything));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(armed(&at_change), [None]);
    assert!(at_deadline.is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn a_source_directory_that_disappears_drops_its_images_and_keeps_the_slot() {
    let mut world = World::new(init(
        keep_current(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg"],
    ));
    world.start(at(0), Style::Light);
    let slot_before = world.model.persisted().styles.light.slot;
    let unmounted = vec![removed("/l/a.jpg"), removed("/l/b.jpg")];

    let at_unmount = world.run(at(100), changed(Style::Light, unmounted));

    assert!(probed(&at_unmount).is_empty());
    assert_eq!(count(&at_unmount, is_set_keys), 0);
    assert_eq!(world.model.persisted().styles.light.slot, slot_before);
    assert_eq!(world.slots.light, slot_before);

    let at_mount = world.run(
        at(5000),
        changed(Style::Light, vec![found("/l/a.jpg"), found("/l/b.jpg")]),
    );

    assert_ne!(world.slots.light, slot_before);
    // Its images are back, and the one on screen is long overdue.
    assert_eq!(published(&at_mount), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn two_styles_without_images_keep_their_slots_and_idle() {
    let mut world = single_images(alphabetical());
    world.run(at(100), changed(Style::Light, vec![removed("/l/only.jpg")]));

    let trace = world.run(at(101), changed(Style::Dark, vec![removed("/d/only.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    let Some(Effect::Report(status)) = trace.last() else {
        panic!("no status when the last image went: {trace:?}");
    };
    assert_eq!(status.next_change, None);
    assert_eq!(status.current.light, Some(id("/l/only.jpg")));
    assert_eq!(status.current.dark, Some(id("/d/only.jpg")));
    assert!(probed(&trace).is_empty());
    assert!(at_deadline.is_empty());
}

#[test]
fn the_first_image_of_two_empty_styles_fills_both_slots_at_once() {
    let mut world = World::new(init(alphabetical(), &[], &[]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(100), changed(Style::Light, vec![found("/l/first.jpg")]));

    assert_eq!(
        published(&trace),
        [
            (Style::Light, "/l/first.jpg"),
            (Style::Dark, "/l/first.jpg")
        ]
    );
    assert_eq!(keys_pointed(&trace), [[Style::Light, Style::Dark]]);
}

#[test]
fn the_first_image_of_an_empty_style_waits_for_the_tick_of_a_borrowed_one() {
    let mut world = World::new(init(alphabetical(), &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Dark);
    assert_eq!(world.current(Style::Dark), Some(id("/l/a.jpg")));

    let at_change = world.run(at(100), changed(Style::Dark, vec![found("/d/own.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);
    let shown: Vec<ItemId> = world.current(Style::Dark).into_iter().collect();

    assert!(probed(&at_change).is_empty());
    assert_eq!(published(&at_deadline), [(Style::Dark, "/d/own.jpg")]);
    assert_eq!(shown, [id("/d/own.jpg")]);
}
