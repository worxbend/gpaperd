//! Config reloads: the new config is applied as a diff against the running
//! one, without a restart.

use std::path::PathBuf;
use std::time::Duration;

use gpaperd_core::catalog::BadReason;
use gpaperd_core::config::{
    Config, ConfigError, EmptyStylePolicy, ExternalChangePolicy, LinkPolicy, Order, PictureOptions,
    Source, StyleConfig,
};
use gpaperd_core::event::{Effect, Event};
use gpaperd_core::model::ModelInit;
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, count, id, init, is_set_keys, is_watch, keys_pointed,
    keys_written, probed, published, reported, running_world, scanned, six_shuffled, slot_uris,
    unscanned,
};

fn reloaded(config: Config) -> Event {
    Event::ConfigChanged(Ok(config))
}

fn folder(path: &str) -> Vec<Source> {
    vec![Source::Path(PathBuf::from(path))]
}

/// The running config of [`running_world`], with a change.
fn changed(change: impl FnOnce(&mut Config)) -> Config {
    let mut config = alphabetical();
    change(&mut config);
    config
}

#[test]
fn an_invalid_file_keeps_the_running_config() {
    let mut world = running_world();
    let invalid = ConfigError {
        line: 3,
        column: 12,
        message: "interval must be at least 1m".to_owned(),
    };

    let trace = world.run(at(100), Event::ConfigChanged(Err(invalid)));
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(trace.is_empty());
    assert_eq!(world.model.config(), &alphabetical());
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_reload_that_changes_nothing_has_no_effect() {
    let mut world = running_world();

    let trace = world.run(at(100), reloaded(alphabetical()));

    assert!(trace.is_empty());
}

#[test]
fn changed_sources_are_scanned_again_for_their_style_only() {
    let mut world = running_world();
    let new = changed(|config| config.styles.light.sources = folder("/new"));

    let trace = world.run(at(100), reloaded(new));

    let watch_light = Effect::Watch {
        style: Style::Light,
        sources: folder("/new"),
    };
    assert_eq!(trace, [watch_light]);
}

#[test]
fn the_scan_of_the_new_sources_replaces_the_catalog() {
    let mut world = running_world();
    let new = changed(|config| config.styles.light.sources = folder("/new"));
    world.run(at(100), reloaded(new));

    let at_scan = world.run(
        at(101),
        scanned(Style::Light, &["/new/x.jpg", "/new/y.jpg"]),
    );
    let at_deadline = world.run(at(900), DEADLINE);

    // The image on screen keeps its interval, although its file is no
    // longer among the sources.
    assert!(published(&at_scan).is_empty());
    assert_eq!(published(&at_deadline), [(Style::Light, "/new/x.jpg")]);
}

#[test]
fn a_new_interval_counts_from_the_last_change() {
    let mut world = running_world();
    let longer = changed(|config| config.styles.light.interval = Duration::from_secs(1800));

    let trace = world.run(at(300), reloaded(longer));

    assert!(probed(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(1800))]);
}

#[test]
fn a_new_interval_that_is_over_already_rotates_at_once() {
    let mut world = running_world();
    let shorter = changed(|config| config.styles.light.interval = Duration::from_secs(120));

    let trace = world.run(at(300), reloaded(shorter));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&trace).last(), Some(&Some(at(420))));
}

#[test]
fn a_style_that_is_no_longer_shuffled_drops_its_round() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let on_screen = world.current(Style::Light).expect("the first apply");
    let mut sorted = Config::default();
    sorted.styles.light.order = Order::Alphabetical;

    world.run(at(100), reloaded(sorted));
    let saved = world.model.persisted().styles.light;
    world.run(at(900), DEADLINE);

    assert!(saved.bag.is_empty());
    assert_eq!(saved.shown, Some(Vec::new()));
    // The cursor is the image on screen: the list goes on after it.
    let number: u32 = on_screen.as_str()[3..4].parse().expect("a numbered image");
    let next = format!("/l/{}.jpg", number % 6 + 1);
    assert_eq!(world.current(Style::Light), Some(id(&next)));
}

#[test]
fn a_style_that_becomes_shuffled_starts_a_round() {
    let mut sorted = Config::default();
    sorted.styles.light.order = Order::Alphabetical;
    let mut world = World::new(ModelInit {
        config: Some(sorted),
        ..six_shuffled()
    });
    world.start(at(0), Style::Light);

    world.run(at(100), reloaded(Config::default()));
    world.run(at(900), DEADLINE);

    // The new round holds all six images, and showed one of them.
    let saved = world.model.persisted().styles.light;
    assert_eq!(saved.shown.map(|shown| shown.len()), Some(1));
    assert_eq!(saved.bag.len(), 5);
}

#[test]
fn changed_picture_options_are_written_alone() {
    let mut world = running_world();
    let spanned = changed(|config| config.general.picture_options = Some(PictureOptions::Spanned));

    let trace = world.run(at(100), reloaded(spanned));

    let no_uri = PerStyle::default();
    assert_eq!(
        keys_written(&trace),
        [(no_uri, Some(PictureOptions::Spanned))]
    );
    assert_eq!(world.keys.picture_options, "spanned");
}

#[test]
fn picture_options_that_stay_are_not_written_again() {
    let spanned = changed(|config| config.general.picture_options = Some(PictureOptions::Spanned));
    let mut world = World::new(init(spanned.clone(), &["/l/a.jpg"], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    let slower = Config {
        styles: PerStyle::from_fn(|_| StyleConfig {
            interval: Duration::from_secs(1800),
            ..spanned.styles.light.clone()
        }),
        ..spanned
    };

    let trace = world.run(at(100), reloaded(slower));

    assert_eq!(count(&trace, is_set_keys), 0);
}

#[test]
fn picture_options_that_left_the_config_leave_the_key_alone() {
    let spanned = changed(|config| config.general.picture_options = Some(PictureOptions::Spanned));
    let mut world = World::new(init(spanned, &["/l/a.jpg"], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(100), reloaded(alphabetical()));

    assert_eq!(count(&trace, is_set_keys), 0);
    assert_eq!(world.keys.picture_options, "spanned");
}

#[test]
fn a_reload_ends_a_pause_and_writes_the_key_back() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, "file:///picked.jpg");

    let trace = world.run(at(200), reloaded(alphabetical()));

    assert_eq!(keys_pointed(&trace), [[Style::Light]]);
    assert_eq!(reported(&trace).paused, PerStyle::default());
    assert_eq!(world.keys.picture_uri, slot_uris().light);
    assert!(!world.model.persisted().styles.light.paused);
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn the_end_of_a_pause_is_saved_although_the_key_write_has_to_wait() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, "file:///picked.jpg");
    // Dark's slot is being filled again: no key is written meanwhile.
    step(&mut world.model, at(200), Event::SlotLost(Style::Dark));

    let trace = step(&mut world.model, at(201), reloaded(alphabetical()));

    assert_eq!(count(&trace, is_set_keys), 0);
    assert!(trace.contains(&Effect::Persist));
    assert!(!world.model.persisted().styles.light.paused);
}

#[test]
fn a_reload_writes_back_a_key_that_waits_for_its_tick() {
    let waiting = changed(|config| {
        config.general.on_external_change = ExternalChangePolicy::ReassertNextTick
    });
    let mut world = World::new(init(waiting.clone(), &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Light);
    world.set_key(at(100), Style::Light, "file:///picked.jpg");

    let trace = world.run(at(200), reloaded(waiting));

    assert_eq!(keys_pointed(&trace), [[Style::Light]]);
}

#[test]
fn a_new_policy_for_external_changes_is_used_from_the_reload_on() {
    let mut world = running_world();
    let reassert =
        changed(|config| config.general.on_external_change = ExternalChangePolicy::ReassertNow);
    world.run(at(100), reloaded(reassert));

    world.set_key(at(200), Style::Light, "file:///picked.jpg");
    let a_second_later = world.run(at(201), DEADLINE);

    assert_eq!(keys_pointed(&a_second_later), [[Style::Light]]);
}

#[test]
fn a_daemon_without_a_config_idles_until_one_arrives() {
    let mut world = World::new(unscanned(Config::default()));
    let mut idle = world.start(at(0), Style::Light);
    idle.extend(world.run(at(1), scanned(Style::Light, &[])));
    idle.extend(world.run(at(1), scanned(Style::Dark, &[])));
    let mut first_config = Config::default();
    first_config.styles.light.sources = folder("/l");
    first_config.styles.dark.sources = folder("/d");

    let at_reload = world.run(at(100), reloaded(first_config));
    let mut at_scans = world.run(at(101), scanned(Style::Light, &["/l/a.jpg", "/l/b.jpg"]));
    at_scans.extend(world.run(at(101), scanned(Style::Dark, &["/d/a.jpg"])));

    assert!(probed(&idle).is_empty());
    assert_eq!(count(&idle, is_set_keys), 0);
    assert_eq!(count(&at_reload, is_watch), 2);
    assert_eq!(published(&at_scans).len(), 2);
    assert_eq!(
        keys_pointed(&at_scans).concat(),
        [Style::Light, Style::Dark]
    );
    assert_eq!(armed(&at_scans), [Some(at(1001))]);
}

#[test]
fn rotate_hidden_that_is_turned_on_starts_the_hidden_clock() {
    let mut world = running_world();
    let both = changed(|config| config.general.rotate_hidden = true);

    world.run(at(300), reloaded(both));
    let at_light_deadline = world.run(at(900), DEADLINE);
    let at_dark_deadline = world.run(at(1200), DEADLINE);

    assert_eq!(world.model.persisted().styles.dark.remaining, None);
    assert_eq!(published(&at_light_deadline), [(Style::Light, "/l/b.jpg")]);
    // Dark had its whole interval left when its clock started.
    assert_eq!(published(&at_dark_deadline), [(Style::Dark, "/d/b.jpg")]);
}

#[test]
fn rotate_hidden_that_is_turned_off_stops_the_hidden_clock() {
    let both = changed(|config| config.general.rotate_hidden = true);
    let mut world = World::new(init(
        both,
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg", "/d/b.jpg"],
    ));
    world.start(at(0), Style::Light);

    world.run(at(300), reloaded(alphabetical()));
    let at_deadline = world.run(at(900), DEADLINE);

    let time_left = world.model.persisted().styles.dark.remaining;
    assert_eq!(time_left, Some(Duration::from_secs(600)));
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_empty_style_that_may_borrow_now_is_filled_at_once() {
    let keep = changed(|config| config.general.empty_style = EmptyStylePolicy::KeepCurrent);
    let mut world = World::new(init(keep, &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(100), reloaded(alphabetical()));

    assert_eq!(published(&trace), [(Style::Dark, "/l/a.jpg")]);
    assert_eq!(keys_pointed(&trace), [[Style::Dark]]);
}

#[test]
fn a_higher_max_pixels_lets_an_image_that_was_too_large_back_in() {
    let mut world = World::new(init(
        alphabetical(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg"],
    ));
    let too_large = BadReason::TooLarge {
        width: 20_000,
        height: 10_000,
    };
    world.rejected.insert(id("/l/b.jpg"), too_large);
    world.start(at(0), Style::Light);
    world.run(at(900), DEADLINE);
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
    world.rejected.clear();
    let roomier = changed(|config| config.general.max_pixels = 300_000_000);

    world.run(at(1000), reloaded(roomier));
    let at_deadline = world.run(at(1800), DEADLINE);

    let probe = Effect::Probe {
        style: Style::Light,
        item: id("/l/b.jpg"),
        path: PathBuf::from("/l/b.jpg"),
        max_pixels: 300_000_000,
    };
    assert!(at_deadline.contains(&probe));
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
}

#[test]
fn a_new_link_policy_is_used_by_the_next_publish() {
    let mut world = running_world();
    let copy = changed(|config| config.general.link = LinkPolicy::AlwaysCopy);
    world.run(at(100), reloaded(copy));

    let at_deadline = world.run(at(900), DEADLINE);

    let publish = Effect::Publish {
        style: Style::Light,
        item: id("/l/b.jpg"),
        path: PathBuf::from("/l/b.jpg"),
        link: LinkPolicy::AlwaysCopy,
    };
    assert!(at_deadline.contains(&publish));
}

#[test]
fn a_daemon_without_a_config_leaves_the_slots_and_keys_of_its_last_run_alone() {
    let mut first = running_world();
    first.slots.light = None;
    first.keys.picture_uri = "file:///home/u/Pictures/mine.jpg".to_owned();
    let no_config = ModelInit {
        config: None,
        ..unscanned(Config::default())
    };
    let mut second = first.restart(no_config);

    let mut idle = second.start(at(100), Style::Light);
    idle.extend(second.run_all(
        at(101),
        [scanned(Style::Light, &[]), scanned(Style::Dark, &[])],
    ));
    let at_reload = second.run(at(200), reloaded(two_folders_config()));
    let at_scans = second.run_all(
        at(201),
        [
            scanned(Style::Light, &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"]),
            scanned(Style::Dark, &["/d/a.jpg", "/d/b.jpg", "/d/c.jpg"]),
        ],
    );

    assert!(probed(&idle).is_empty());
    assert_eq!(count(&idle, is_set_keys), 0);
    assert!(probed(&at_reload).is_empty());
    assert_eq!(published(&at_scans), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(second.keys.picture_uri, slot_uris().light);
}

/// The config of [`running_world`], with the folders it scans.
fn two_folders_config() -> Config {
    changed(|config| {
        config.styles.light.sources = folder("/l");
        config.styles.dark.sources = folder("/d");
    })
}
