//! Start-up: the first apply, restarts and restored state.

use std::collections::BTreeSet;
use std::time::Duration;

use gpaperd_core::catalog::ItemId;
use gpaperd_core::config::{Config, Source};
use gpaperd_core::event::{Effect, Event, Status};
use gpaperd_core::model::{Model, ModelInit, Persisted, STATE_VERSION, StylePersisted};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, changed, count, dark_folder_starts_broken,
    foreign_slot, found, id, init, is_publish, is_ready, is_set_keys, is_watch, keys_pointed,
    light_showing, probed, published, reported, running_world, scanned, six_shuffled, started,
    switch_to, two_folders, unscanned, user_keys,
};

const LIGHT: [&str; 3] = ["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"];
const SIX_LIGHT: [&str; 6] = [
    "/l/1.jpg", "/l/2.jpg", "/l/3.jpg", "/l/4.jpg", "/l/5.jpg", "/l/6.jpg",
];

/// Sends `ticks` deadlines, one interval apart from `first_secs` on, and
/// returns the light image each one put on screen.
fn light_images_shown(world: &mut World, first_secs: u64, ticks: u64) -> Vec<ItemId> {
    let deadlines = (0..ticks).map(|tick| at(first_secs + 900 * tick));
    deadlines
        .filter_map(|deadline| {
            world.run(deadline, DEADLINE);
            world.current(Style::Light)
        })
        .collect()
}

#[test]
fn a_first_run_fills_both_slots() {
    let trace = World::new(two_folders()).start(at(0), Style::Light);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/d/a.jpg")]
    );
}

#[test]
fn a_first_run_writes_the_keys_once_after_both_slots_exist() {
    let trace = World::new(two_folders()).start(at(0), Style::Light);

    let last_publish = trace
        .iter()
        .rposition(is_publish)
        .expect("a slot is published");
    let set_keys = trace.iter().position(is_set_keys).expect("keys are written");
    assert_eq!(count(&trace, is_set_keys), 1);
    assert!(last_publish < set_keys);
}

#[test]
fn the_state_is_saved_before_the_keys_are_written() {
    let trace = World::new(two_folders()).start(at(0), Style::Light);

    let set_keys = trace
        .iter()
        .position(is_set_keys)
        .expect("keys are written");
    assert!(trace[..set_keys].contains(&Effect::Persist));
}

#[test]
fn readiness_is_signalled_once_after_the_first_apply() {
    let mut world = World::new(two_folders());

    let mut trace = world.start(at(0), Style::Light);
    let last_publish = trace
        .iter()
        .rposition(is_publish)
        .expect("a slot is published");
    let ready = trace.iter().position(is_ready).expect("ready");
    trace.extend(world.run(at(900), DEADLINE));

    assert!(last_publish < ready);
    assert_eq!(count(&trace, is_ready), 1);
}

#[test]
fn the_first_status_names_both_images_and_the_next_change() {
    let trace = World::new(two_folders()).start(at(0), Style::Light);

    let status = Status {
        visible: Style::Light,
        current: PerStyle {
            light: Some(id("/l/a.jpg")),
            dark: Some(id("/d/a.jpg")),
        },
        next_change: Some(at(900)),
        paused: PerStyle::default(),
    };
    assert_eq!(trace.last(), Some(&Effect::Report(status)));
}

#[test]
fn a_restart_with_intact_slots_publishes_nothing() {
    let first = running_world();

    let trace = first.restart(two_folders()).start(at(60), Style::Light);

    assert!(published(&trace).is_empty());
    assert_eq!(count(&trace, is_set_keys), 1);
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn a_restart_refills_a_missing_slot_with_the_same_image() {
    let mut second = running_world().restart(two_folders());
    second.slots.light = None;

    let trace = second.start(at(60), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    // The image did not change, so its interval keeps running.
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn a_restart_refills_a_slot_that_someone_replaced() {
    let mut second = running_world().restart(two_folders());
    second.slots.dark = foreign_slot();

    let trace = second.start(at(60), Style::Light);

    assert_eq!(published(&trace), [(Style::Dark, "/d/a.jpg")]);
}

#[test]
fn a_restart_rotates_once_when_the_visible_style_is_overdue() {
    let first = running_world();

    let trace = first.restart(two_folders()).start(at(5000), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace), [Some(at(5900))]);
}

#[test]
fn an_overdue_style_with_a_lost_slot_is_refilled_then_rotates_once() {
    let mut second = running_world().restart(two_folders());
    second.slots.light = None;

    let trace = second.start(at(5000), Style::Light);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Light, "/l/b.jpg")]
    );
}

#[test]
fn a_lost_slot_is_refilled_although_the_overdue_rotation_fails() {
    let mut second = running_world().restart(two_folders());
    second.slots.light = None;
    second.broken = [id("/l/b.jpg"), id("/l/c.jpg")].into();

    let trace = second.start(at(5000), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(count(&trace, is_set_keys), 1);
}

#[test]
fn a_slot_that_could_not_be_filled_is_tried_again_while_its_style_is_hidden() {
    let mut world = dark_folder_starts_broken(alphabetical());
    let at_start = world.start(at(0), Style::Light);

    let at_dark_deadline = world.run(at(600), DEADLINE);

    assert_eq!(published(&at_start), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(published(&at_dark_deadline), [(Style::Dark, "/d/f.jpg")]);
}

#[test]
fn an_overdue_rotation_is_not_published_once_its_style_left_the_screen() {
    let mut second = running_world().restart(two_folders());
    let slots = second.slots;
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Ok(()),
    };

    let at_start = step(
        &mut second.model,
        at(5000),
        started(Style::Light, user_keys(), slots),
    );
    step(&mut second.model, at(5001), switch_to(Style::Dark));
    let after_probe = step(&mut second.model, at(5002), probe_passed);

    assert_eq!(probed(&at_start), [(Style::Light, "/l/b.jpg")]);
    assert!(published(&after_probe).is_empty());
}

#[test]
fn a_saved_image_without_a_start_time_counts_as_overdue() {
    let hand_edited = Persisted {
        version: STATE_VERSION,
        styles: PerStyle {
            light: StylePersisted {
                current: Some(id("/l/a.jpg")),
                slot: foreign_slot(),
                ..StylePersisted::default()
            },
            dark: StylePersisted::default(),
        },
        original_keys: None,
    };
    let mut world = World::new(ModelInit {
        restored: Some(hand_edited),
        ..two_folders()
    });
    world.slots.light = foreign_slot();

    let trace = world.start(at(60), Style::Light);

    assert!(published(&trace).contains(&(Style::Light, "/l/b.jpg")));
}

#[test]
fn a_restart_does_not_rotate_an_overdue_hidden_style() {
    // Light was on screen when the daemon stopped. Now dark is.
    let first = running_world();

    let trace = first.restart(two_folders()).start(at(5000), Style::Dark);

    assert!(published(&trace).is_empty());
    // Dark was hidden with its whole interval left. The timer is armed for
    // something sooner: the refresh of dark's slot.
    assert_eq!(reported(&trace).next_change, Some(at(5900)));
}

#[test]
fn a_style_that_fell_due_while_hidden_and_off_rotates_when_it_is_shown() {
    // Light was on screen when the daemon stopped. It comes back hidden.
    let mut second = running_world().restart(two_folders());
    second.start(at(5000), Style::Dark);

    let trace = second.run(at(5100), switch_to(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_restart_continues_the_shuffle_round_whatever_the_seed() {
    let shuffled = || {
        init(
            Config::default(),
            &[
                "/l/1.jpg", "/l/2.jpg", "/l/3.jpg", "/l/4.jpg", "/l/5.jpg", "/l/6.jpg",
            ],
            &["/d/only.jpg"],
        )
    };
    let mut first = World::new(shuffled());
    first.start(at(0), Style::Light);
    first.run(at(900), DEADLINE);
    first.run(at(1800), DEADLINE);
    let rest_of_round = first.model.persisted().styles.light.bag;
    assert_eq!(rest_of_round.len(), 3);

    for seed in [1, 99] {
        let mut second = first.restart(ModelInit { seed, ..shuffled() });
        second.start(at(1900), Style::Light);
        let mut shown: Vec<ItemId> = Vec::new();
        for tick in [2700, 3600, 4500] {
            second.run(at(tick), DEADLINE);
            shown.extend(second.current(Style::Light));
        }

        assert_eq!(shown, rest_of_round, "seed {seed}");
    }
}

#[test]
fn alphabetical_order_resumes_after_its_current_image_was_deleted() {
    let mut first = running_world();
    first.run(at(900), DEADLINE);
    assert_eq!(first.current(Style::Light), Some(id("/l/b.jpg")));
    let without_b = init(
        alphabetical(),
        &["/l/a.jpg", "/l/c.jpg"],
        &["/d/a.jpg", "/d/b.jpg", "/d/c.jpg"],
    );

    let mut second = first.restart(without_b);
    let at_start = second.start(at(1000), Style::Light);
    let at_deadline = second.run(at(1800), DEADLINE);

    // The slot still holds the deleted image's data.
    assert!(published(&at_start).is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn saved_state_holds_what_each_slot_shows() {
    let world = running_world();

    let saved = world.model.persisted();

    assert_eq!(saved.version, STATE_VERSION);
    assert_eq!(saved.styles.light.current, Some(id("/l/a.jpg")));
    assert_eq!(saved.styles.light.slot, world.slots.light);
    assert_eq!(saved.styles.light.last_change, Some(at(0)));
    assert_eq!(saved.styles.light.remaining, None);
    assert_eq!(saved.styles.dark.current, Some(id("/d/a.jpg")));
    // Dark is hidden: its clock is stopped with the whole interval left.
    assert_eq!(saved.styles.dark.remaining, Some(Duration::from_secs(900)));
}

#[test]
fn saved_state_restores_to_the_same_state() {
    let mut world = running_world();
    world.run(at(900), DEADLINE);
    world.run(at(1000), switch_to(Style::Dark));
    let saved = world.model.persisted();

    let restored = Model::new(ModelInit {
        restored: Some(saved.clone()),
        ..two_folders()
    });

    assert_eq!(restored.persisted(), saved);
}

#[test]
fn an_alphabetical_style_saves_no_bag() {
    let world = running_world();

    assert!(world.model.persisted().styles.light.bag.is_empty());
}

#[test]
fn a_start_asks_for_a_scan_and_a_watch_of_each_styles_sources() {
    let mut config = alphabetical();
    config.styles.light.sources = vec![Source::Path("/l".into())];
    config.styles.dark.sources = vec![Source::Path("/d".into()), Source::File("/x.jpg".into())];
    let watch = |style| Effect::Watch {
        style,
        sources: config.styles[style].sources.clone(),
    };

    let trace = World::new(unscanned(config.clone())).start(at(0), Style::Light);

    let watches: Vec<Effect> = trace.into_iter().filter(is_watch).collect();
    assert_eq!(watches, [watch(Style::Light), watch(Style::Dark)]);
}

#[test]
fn readiness_does_not_wait_for_the_scans() {
    let first = running_world();

    let trace = first
        .restart(unscanned(alphabetical()))
        .start(at(60), Style::Light);

    assert!(probed(&trace).is_empty());
    assert_eq!(count(&trace, is_set_keys), 1);
    assert_eq!(count(&trace, is_ready), 1);
}

#[test]
fn the_timer_is_armed_once_the_scan_says_what_there_is_to_rotate_to() {
    let mut second = running_world().restart(unscanned(alphabetical()));

    let at_start = second.start(at(60), Style::Light);
    let at_scan = second.run(at(61), scanned(Style::Light, &LIGHT));

    assert!(armed(&at_start).is_empty());
    assert_eq!(armed(&at_scan), [Some(at(900))]);
}

#[test]
fn a_first_run_fills_each_slot_when_its_scan_arrives() {
    let mut world = World::new(unscanned(alphabetical()));

    let at_start = world.start(at(0), Style::Light);
    let at_light_scan = world.run(at(1), scanned(Style::Light, &LIGHT));
    let at_dark_scan = world.run(at(2), scanned(Style::Dark, &["/d/a.jpg"]));

    assert!(probed(&at_start).is_empty());
    assert_eq!(count(&at_start, is_ready), 1);
    assert_eq!(published(&at_light_scan), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(published(&at_dark_scan), [(Style::Dark, "/d/a.jpg")]);
    assert_eq!(keys_pointed(&at_light_scan), [[Style::Light]]);
    assert_eq!(keys_pointed(&at_dark_scan), [[Style::Dark]]);
}

#[test]
fn scans_that_arrive_together_get_one_key_write() {
    let mut world = World::new(unscanned(alphabetical()));
    world.start(at(0), Style::Light);
    let both_scans = [
        scanned(Style::Light, &LIGHT),
        scanned(Style::Dark, &["/d/a.jpg"]),
    ];

    let trace = world.run_all(at(1), both_scans);

    assert_eq!(published(&trace).len(), 2);
    assert_eq!(keys_pointed(&trace), [[Style::Light, Style::Dark]]);
}

#[test]
fn a_lost_slot_is_refilled_from_the_saved_item_before_the_scan() {
    let mut second = running_world().restart(unscanned(alphabetical()));
    second.slots.light = None;

    let trace = second.start(at(60), Style::Light);

    let refill = trace.iter().position(is_publish);
    let ready = trace.iter().position(is_ready);
    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert!(refill < ready);
}

#[test]
fn a_saved_image_that_the_scan_does_not_find_stays_until_the_next_tick() {
    let mut first = running_world();
    first.run(at(900), DEADLINE);
    assert_eq!(first.current(Style::Light), Some(id("/l/b.jpg")));

    let mut second = first.restart(unscanned(alphabetical()));
    second.start(at(1000), Style::Light);
    let at_scan = second.run(at(1001), scanned(Style::Light, &["/l/a.jpg", "/l/c.jpg"]));
    let at_deadline = second.run(at(1800), DEADLINE);

    // The slot still holds the deleted image's data.
    assert!(probed(&at_scan).is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn an_overdue_style_rotates_once_its_scan_arrives() {
    let mut second = running_world().restart(unscanned(alphabetical()));

    let at_start = second.start(at(5000), Style::Light);
    let at_scan = second.run(at(5001), scanned(Style::Light, &LIGHT));

    assert!(probed(&at_start).is_empty());
    assert_eq!(published(&at_scan), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_style_does_not_turn_to_the_other_styles_images_before_its_own_scan() {
    let mut world = World::new(unscanned(alphabetical()));
    world.start(at(0), Style::Dark);

    let at_light_scan = world.run(at(1), scanned(Style::Light, &LIGHT));
    let at_empty_dark_scan = world.run(at(2), scanned(Style::Dark, &[]));

    assert_eq!(published(&at_light_scan), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(published(&at_empty_dark_scan), [(Style::Dark, "/l/a.jpg")]);
}

#[test]
fn a_slot_of_unknown_content_is_replaced_once_the_scan_finds_an_image() {
    let mut world = World::new(unscanned(alphabetical()));
    world.slots.light = foreign_slot();

    let at_start = world.start(at(0), Style::Light);
    let at_scan = world.run(at(1), scanned(Style::Light, &LIGHT));

    assert!(probed(&at_start).is_empty());
    assert_eq!(published(&at_scan), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn a_scan_that_finds_what_is_known_changes_nothing() {
    let mut world = running_world();

    let trace = world.run(at(100), scanned(Style::Light, &LIGHT));

    assert!(trace.is_empty());
}

#[test]
fn a_restored_round_goes_on_when_its_scan_arrives() {
    let mut first = World::new(six_shuffled());
    first.start(at(0), Style::Light);
    light_images_shown(&mut first, 900, 2);
    let rest_of_round = first.model.persisted().styles.light.bag;
    assert_eq!(rest_of_round.len(), 3);

    for seed in [1, 99] {
        let still_to_scan = ModelInit {
            seed,
            ..unscanned(Config::default())
        };
        let mut second = first.restart(still_to_scan);
        second.start(at(1900), Style::Light);
        second.run(at(1901), scanned(Style::Light, &SIX_LIGHT));

        let shown = light_images_shown(&mut second, 2700, 3);

        assert_eq!(shown, rest_of_round, "seed {seed}");
    }
}

#[test]
fn an_image_that_is_new_since_the_last_run_joins_the_restored_round() {
    let mut first = World::new(six_shuffled());
    first.start(at(0), Style::Light);
    light_images_shown(&mut first, 900, 2);
    let rest_of_round = first.model.persisted().styles.light.bag;
    let new = id("/l/new.jpg");
    let mut with_new = SIX_LIGHT.to_vec();
    with_new.push("/l/new.jpg");

    for seed in 0..8 {
        let scanned_up_front = ModelInit {
            seed,
            ..init(Config::default(), &with_new, &["/d/only.jpg"])
        };
        let mut second = first.restart(scanned_up_front);
        second.start(at(1900), Style::Light);

        let shown = light_images_shown(&mut second, 2700, 4);

        let old_in_order: Vec<ItemId> = shown.iter().filter(|&id| *id != new).cloned().collect();
        assert!(shown.contains(&new), "seed {seed}");
        assert_eq!(old_in_order, rest_of_round, "seed {seed}");
    }
}

#[test]
fn an_image_that_a_later_scan_finds_new_joins_the_restored_round_too() {
    let mut first = World::new(six_shuffled());
    first.start(at(0), Style::Light);
    light_images_shown(&mut first, 900, 2);
    let rest_of_round = first.model.persisted().styles.light.bag;
    let new = id("/l/new.jpg");
    let mut with_new = SIX_LIGHT.to_vec();
    with_new.push("/l/new.jpg");
    let mut places_of_new = BTreeSet::new();

    for seed in 0..40 {
        let still_to_scan = ModelInit {
            seed,
            ..unscanned(Config::default())
        };
        let mut second = first.restart(still_to_scan);
        second.start(at(1900), Style::Light);
        second.run(at(1901), scanned(Style::Light, &with_new));

        let shown = light_images_shown(&mut second, 2700, 4);

        let distinct: BTreeSet<&ItemId> = shown.iter().collect();
        assert_eq!(distinct.len(), 4, "seed {seed}");
        assert!(rest_of_round.iter().all(|old| shown.contains(old)));
        places_of_new.insert(shown.iter().position(|shown| *shown == new));
    }

    // The new image is not always the last of its round.
    assert_eq!(places_of_new.len(), 4);
}

#[test]
fn the_image_on_screen_gets_no_new_place_in_the_restored_round() {
    let three_images = || init(Config::default(), &LIGHT, &["/d/only.jpg"]);
    // The round reached a.jpg while it was on screen, so it holds it no more.
    let saved = light_showing("/l/a.jpg", &["/l/b.jpg"], &["/l/c.jpg"]);

    for seed in 0..8 {
        let restored = Model::new(ModelInit {
            seed,
            restored: Some(saved.clone()),
            ..three_images()
        });

        let rest_of_round = restored.persisted().styles.light.bag;
        assert_eq!(rest_of_round, [id("/l/b.jpg")], "seed {seed}");
    }
}

#[test]
fn a_round_restored_from_before_m2_gives_what_it_showed_no_second_turn() {
    // A state file from before M2 holds no `shown`: whatever the round does
    // not hold, it showed.
    let mut saved = light_showing("/l/3.jpg", &["/l/1.jpg", "/l/2.jpg"], &[]);
    saved.styles.light.shown = None;

    for seed in 0..8 {
        let restored = Model::new(ModelInit {
            seed,
            restored: Some(saved.clone()),
            ..six_shuffled()
        });

        let rest_of_round = restored.persisted().styles.light.bag;
        assert_eq!(
            rest_of_round,
            [id("/l/1.jpg"), id("/l/2.jpg")],
            "seed {seed}"
        );
    }
}

#[test]
fn an_image_found_after_a_restore_from_before_m2_joins_the_round() {
    let mut saved = light_showing("/l/3.jpg", &["/l/1.jpg", "/l/2.jpg"], &[]);
    saved.styles.light.shown = None;
    let mut world = World::new(ModelInit {
        restored: Some(saved),
        ..six_shuffled()
    });
    world.slots.light = foreign_slot();
    world.start(at(100), Style::Light);

    world.run(at(200), changed(Style::Light, vec![found("/l/new.jpg")]));

    let rest_of_round = world.model.persisted().styles.light.bag;
    assert_eq!(rest_of_round.len(), 3);
    assert!(rest_of_round.contains(&id("/l/new.jpg")));
}
