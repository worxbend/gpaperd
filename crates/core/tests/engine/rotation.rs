//! One rotation: probe, publish, and the next candidate after a failure.

use gpaperd_core::catalog::FileId;
use gpaperd_core::config::{Config, DEFAULT_MAX_PIXELS, LinkPolicy};
use gpaperd_core::event::{Effect, Event, Method, SlotWrite};
use gpaperd_core::model::ModelInit;
use gpaperd_core::step::{MAX_PUBLISH_ATTEMPTS, step};
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, foreign_slot, id, init, light_showing, probed,
    published, running_world, six_shuffled, switch_to, two_folders_with,
};

/// A daemon restarted at second 0 over `light` images, with the light slot
/// intact and showing `current`. `bag` is the rest of its shuffle round, and
/// `shown` is what the round showed.
fn light_world(init: ModelInit, current: &str, bag: &[&str], shown: &[&str]) -> World {
    let mut world = World::new(ModelInit {
        restored: Some(light_showing(current, bag, shown)),
        ..init
    });
    world.slots.light = foreign_slot();
    world.start(at(0), Style::Light);
    world
}

#[test]
fn an_item_is_probed_before_it_is_published() {
    let mut world = running_world();
    let item = id("/l/b.jpg");

    let fired = step(&mut world.model, at(900), DEADLINE);
    let verdict = Event::Probed {
        style: Style::Light,
        item: item.clone(),
        verdict: Ok(()),
    };
    let probe_passed = step(&mut world.model, at(900), verdict);

    let probe = Effect::Probe {
        style: Style::Light,
        item: item.clone(),
        path: "/l/b.jpg".into(),
        max_pixels: DEFAULT_MAX_PIXELS,
    };
    let publish = Effect::Publish {
        style: Style::Light,
        item,
        path: "/l/b.jpg".into(),
        link: LinkPolicy::PreferLink,
    };
    assert_eq!(fired, [probe]);
    assert_eq!(probe_passed, [publish]);
}

#[test]
fn a_publish_carries_the_configured_link_policy() {
    let mut config = alphabetical();
    config.general.link = LinkPolicy::AlwaysCopy;
    let mut world = World::new(two_folders_with(config));

    let trace = world.start(at(0), Style::Light);

    let always_copies = |effect: &Effect| match effect {
        Effect::Publish { link, .. } => *link == LinkPolicy::AlwaysCopy,
        _ => true,
    };
    assert!(trace.iter().all(always_copies));
}

#[test]
fn a_failed_probe_moves_on_to_the_next_candidate() {
    let mut world = running_world();
    world.broken.insert(id("/l/b.jpg"));

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        probed(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Light, "/l/c.jpg")]
    );
    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_bad_item_is_not_probed_again() {
    let mut world = running_world();
    world.broken.insert(id("/l/b.jpg"));
    world.run(at(900), DEADLINE);
    world.run(at(1800), DEADLINE);
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));

    let trace = world.run(at(2700), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_bad_verdict_holds_for_both_styles() {
    let shared = ["/s/a.jpg", "/s/b.jpg", "/s/c.jpg"];
    let mut world = World::new(init(alphabetical(), &shared, &shared));
    world.broken.insert(id("/s/b.jpg"));
    world.start(at(0), Style::Light);
    world.run(at(900), DEADLINE);
    world.run(at(1000), switch_to(Style::Dark));

    let trace = world.run(at(1900), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Dark, "/s/c.jpg")]);
}

#[test]
fn a_failed_publish_moves_on_to_the_next_candidate() {
    let mut world = running_world();
    world.unwritable.insert(id("/l/b.jpg"));

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Light, "/l/c.jpg")]
    );
    assert_eq!(world.current(Style::Light), Some(id("/l/c.jpg")));
}

#[test]
fn a_failed_publish_does_not_mark_the_item_bad() {
    let mut world = running_world();
    world.unwritable.insert(id("/l/b.jpg"));
    world.run(at(900), DEADLINE);
    world.unwritable.clear();
    world.run(at(1800), DEADLINE);

    world.run(at(2700), DEADLINE);

    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
}

#[test]
fn a_rotation_gives_up_after_five_candidates_and_keeps_the_slot() {
    let light = [
        "/l/a.jpg", "/l/b.jpg", "/l/c.jpg", "/l/d.jpg", "/l/e.jpg", "/l/f.jpg", "/l/g.jpg",
    ];
    let mut world = World::new(init(alphabetical(), &light, &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.broken = light[1..].iter().map(|path| id(path)).collect();

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(probed(&trace).len(), MAX_PUBLISH_ATTEMPTS as usize);
    assert!(published(&trace).is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
    // A fresh interval starts, and it is saved.
    assert_eq!(armed(&trace), [Some(at(1800))]);
    assert!(trace.contains(&Effect::Persist));
}

#[test]
fn a_rotation_stops_early_when_no_other_candidate_is_left() {
    let mut world = World::new(init(
        alphabetical(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg"],
    ));
    world.start(at(0), Style::Light);
    world.broken.insert(id("/l/b.jpg"));

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Light, "/l/b.jpg")]);
    assert!(published(&trace).is_empty());
    // Only the image on screen is left, so there is nothing to wait for.
    assert!(armed(&trace).is_empty());
}

#[test]
fn a_shuffled_image_is_tried_once_per_rotation() {
    let two_images = init(
        Config::default(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/only.jpg"],
    );
    let mut world = light_world(two_images, "/l/a.jpg", &[], &["/l/b.jpg", "/l/a.jpg"]);
    world.unwritable.insert(id("/l/b.jpg"));

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn a_shuffled_rotation_moves_on_to_an_image_it_has_not_tried() {
    for seed in 0..8 {
        let three_images = init(
            Config::default(),
            &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"],
            &["/d/only.jpg"],
        );
        // Only b is left of the round, so the retry starts a new round.
        let last_of_round = ModelInit {
            seed,
            ..three_images
        };
        let mut world = light_world(
            last_of_round,
            "/l/a.jpg",
            &["/l/b.jpg"],
            &["/l/c.jpg", "/l/a.jpg"],
        );
        world.unwritable.insert(id("/l/b.jpg"));

        let trace = world.run(at(900), DEADLINE);

        assert_eq!(
            published(&trace),
            [(Style::Light, "/l/b.jpg"), (Style::Light, "/l/c.jpg")],
            "seed {seed}"
        );
    }
}

#[test]
fn a_rotation_does_not_walk_the_sorted_list_twice() {
    let light = ["/l/a.jpg", "/l/b.jpg", "/l/d.jpg"];
    let without_c = init(alphabetical(), &light, &["/d/only.jpg"]);
    // The image on screen was deleted, so the walk never meets it again.
    let mut world = light_world(without_c, "/l/c.jpg", &[], &[]);
    world.unwritable = light.iter().map(|path| id(path)).collect();

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        published(&trace),
        [
            (Style::Light, "/l/d.jpg"),
            (Style::Light, "/l/a.jpg"),
            (Style::Light, "/l/b.jpg")
        ]
    );
}

#[test]
fn a_result_nobody_waits_for_is_ignored() {
    let mut world = running_world();
    let stale_probe = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Ok(()),
    };
    let stale_publish = Event::Published {
        style: Style::Light,
        item: id("/l/b.jpg"),
        result: Ok(SlotWrite {
            method: Method::Copy,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };

    assert!(world.run(at(10), stale_probe).is_empty());
    assert!(world.run(at(10), stale_publish).is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn a_style_that_left_the_screen_during_its_probe_is_not_published() {
    let mut world = running_world();
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Ok(()),
    };

    let fired = step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), switch_to(Style::Dark));
    let after_probe = step(&mut world.model, at(902), probe_passed);

    assert_eq!(probed(&fired), [(Style::Light, "/l/b.jpg")]);
    assert!(published(&after_probe).is_empty());
}

#[test]
fn an_interrupted_rotation_resumes_when_the_style_is_shown_again() {
    let mut world = running_world();
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id("/l/b.jpg"),
        verdict: Ok(()),
    };
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(901), switch_to(Style::Dark));
    step(&mut world.model, at(902), probe_passed);

    let trace = world.run(at(2000), switch_to(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_interrupted_shuffled_rotation_resumes_with_the_image_it_had_picked() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let fired = step(&mut world.model, at(900), DEADLINE);
    let picked = probed(&fired)[0].1;
    let probe_passed = Event::Probed {
        style: Style::Light,
        item: id(picked),
        verdict: Ok(()),
    };
    step(&mut world.model, at(901), switch_to(Style::Dark));
    step(&mut world.model, at(902), probe_passed);

    let trace = world.run(at(2000), switch_to(Style::Light));

    assert_eq!(published(&trace), [(Style::Light, picked)]);
}

#[test]
fn a_shutdown_during_a_rotation_keeps_its_image_in_the_saved_round() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let fired = step(&mut world.model, at(900), DEADLINE);
    let picked = probed(&fired)[0].1;

    step(&mut world.model, at(901), Event::Shutdown);

    let saved_round = world.model.persisted().styles.light.bag;
    assert_eq!(saved_round.first(), Some(&id(picked)));
}

#[test]
fn a_shuffled_image_whose_publish_failed_stays_in_its_round() {
    let three_images = init(
        Config::default(),
        &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"],
        &["/d/only.jpg"],
    );
    let mut world = light_world(
        three_images,
        "/l/a.jpg",
        &["/l/b.jpg", "/l/c.jpg"],
        &["/l/a.jpg"],
    );
    world.unwritable.insert(id("/l/b.jpg"));

    world.run(at(900), DEADLINE);

    assert_eq!(world.current(Style::Light), Some(id("/l/c.jpg")));
    assert_eq!(world.model.persisted().styles.light.bag, [id("/l/b.jpg")]);
}
