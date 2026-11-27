//! Background keys that something else changes: the `on_external_change`
//! policies.

use std::time::{Duration, SystemTime};

use gpaperd_core::config::{Config, ExternalChangePolicy, PictureOptions};
use gpaperd_core::event::{Effect, Pause};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, count, dark_folder_starts_broken,
    init, is_publish, is_set_keys, is_watch, keys_changed, keys_pointed, keys_written, probed,
    published, reported, running_world, slot_uris, started, two_folders, two_folders_with,
};

/// A wallpaper that the user picked in Settings.
const PICKED: &str = "file:///home/u/Pictures/picked.jpg";

fn policy(on_external_change: ExternalChangePolicy) -> Config {
    let mut config = alphabetical();
    config.general.on_external_change = on_external_change;
    config
}

/// A daemon with `on_external_change` set, after its first run at second 0
/// with light on screen.
fn running_with(on_external_change: ExternalChangePolicy) -> World {
    let mut world = World::new(two_folders_with(policy(on_external_change)));
    world.start(at(0), Style::Light);
    world
}

/// Something sets `style`'s key back to the slot URI.
fn point_back(world: &mut World, now: SystemTime, style: Style) -> Vec<Effect> {
    let slot_uri = slot_uris()[style].clone();
    world.set_key(now, style, &slot_uri)
}

fn is_paused(world: &World, style: Style) -> bool {
    world.model.persisted().styles[style].paused
}

#[test]
fn the_daemons_own_key_write_is_no_external_change() {
    let mut world = World::new(two_folders());

    let at_start = world.start(at(0), Style::Light);
    let at_report = world.run(at(1), keys_changed(world.keys.clone()));

    assert_eq!(reported(&at_start).paused, PerStyle::default());
    assert!(at_report.is_empty());
}

#[test]
fn under_pause_a_changed_key_stops_its_style() {
    let mut world = running_world();

    let at_change = world.set_key(at(100), Style::Light, PICKED);
    let at_deadline = world.run(at(900), DEADLINE);

    let paused = reported(&at_change).paused;
    assert_eq!(paused.light, Some(Pause::ExternalChange));
    assert_eq!(paused.dark, None);
    // Nothing is left to wait for.
    assert_eq!(armed(&at_change), [None]);
    assert!(probed(&at_deadline).is_empty());
}

#[test]
fn a_pause_is_saved() {
    let mut world = running_world();

    let trace = world.set_key(at(100), Style::Light, PICKED);

    assert!(trace.contains(&Effect::Persist));
    assert!(is_paused(&world, Style::Light));
}

#[test]
fn only_the_style_whose_key_changed_is_paused() {
    let mut world = running_world();

    world.set_key(at(100), Style::Dark, PICKED);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_paused_style_writes_no_key() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let later = world.set_key(at(200), Style::Light, "file:///another.jpg");
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(count(&later, is_set_keys), 0);
    assert_eq!(count(&at_deadline, is_set_keys), 0);
    assert_eq!(world.keys.picture_uri, "file:///another.jpg");
}

#[test]
fn a_paused_style_resumes_when_its_key_names_the_slot_again() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let at_return = point_back(&mut world, at(200), Style::Light);

    assert_eq!(reported(&at_return).paused, PerStyle::default());
    assert!(!is_paused(&world, Style::Light));
    // Not overdue: the deadline is the one it had.
    assert_eq!(armed(&at_return), [Some(at(900))]);
    assert!(probed(&at_return).is_empty());
}

#[test]
fn the_end_of_a_pause_is_saved() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let at_return = point_back(&mut world, at(200), Style::Light);

    assert!(at_return.contains(&Effect::Persist));
}

#[test]
fn a_style_that_is_overdue_when_it_resumes_rotates_at_once() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let at_return = point_back(&mut world, at(1000), Style::Light);

    assert_eq!(published(&at_return), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(armed(&at_return), [Some(at(1900))]);
}

#[test]
fn a_key_that_changes_again_while_paused_changes_nothing() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let trace = world.set_key(at(200), Style::Light, "file:///another.jpg");

    assert!(trace.is_empty());
}

#[test]
fn a_key_that_the_daemon_never_wrote_is_the_users() {
    let mut world = dark_folder_starts_broken(alphabetical());
    world.start(at(0), Style::Light);

    // Dark has no slot yet, so its key is still the user's own.
    let trace = world.set_key(at(100), Style::Dark, PICKED);

    assert!(trace.is_empty());
    assert!(!is_paused(&world, Style::Dark));
}

#[test]
fn a_pause_survives_a_restart() {
    let mut first = running_world();
    first.set_key(at(100), Style::Light, PICKED);

    let mut second = first.restart(two_folders());
    let at_start = second.start(at(5000), Style::Light);

    assert_eq!(
        reported(&at_start).paused.light,
        Some(Pause::ExternalChange)
    );
    assert!(probed(&at_start).is_empty());
    // Only dark's key is pointed at its slot again.
    assert_eq!(keys_pointed(&at_start), [[Style::Dark]]);
    assert_eq!(second.keys.picture_uri, PICKED);
}

#[test]
fn a_pause_from_the_last_run_ends_when_the_key_names_the_slot_again() {
    let mut first = running_world();
    first.set_key(at(100), Style::Light, PICKED);
    let mut second = first.restart(two_folders());
    second.start(at(200), Style::Light);

    let at_return = point_back(&mut second, at(300), Style::Light);

    assert_eq!(reported(&at_return).paused, PerStyle::default());
    assert_eq!(armed(&at_return), [Some(at(900))]);
}

#[test]
fn a_restart_that_finds_the_key_at_the_slot_again_is_not_paused() {
    let mut first = running_world();
    first.set_key(at(100), Style::Light, PICKED);
    let mut second = first.restart(two_folders());
    second.keys.picture_uri = slot_uris().light;

    // The start alone, before GNOME reports any key write of this run.
    let start = started(Style::Light, second.keys.clone(), second.slots);
    let at_start = step(&mut second.model, at(200), start);

    assert_eq!(reported(&at_start).paused, PerStyle::default());
    assert!(!is_paused(&second, Style::Light));
    // The scans are asked for first, as at every start.
    assert!(at_start[..2].iter().all(is_watch));
}

#[test]
fn reassert_now_writes_the_key_back_after_a_second() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);

    let at_change = world.set_key(at(100), Style::Light, PICKED);
    let a_second_later = world.run(at(101), DEADLINE);

    assert_eq!(count(&at_change, is_set_keys), 0);
    assert_eq!(armed(&at_change), [Some(at(101))]);
    assert_eq!(keys_pointed(&a_second_later), [[Style::Light]]);
    assert_eq!(world.keys.picture_uri, slot_uris().light);
    // The rotation goes on as before.
    assert_eq!(armed(&a_second_later), [Some(at(900))]);
}

#[test]
fn a_key_that_was_written_back_is_not_written_again_before_gnome_reports_it() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);
    world.set_key(at(100), Style::Light, PICKED);

    // GNOME's report of the write is still on its way, or never comes:
    // dconf drops a write that changes nothing.
    let written_back = step(&mut world.model, at(101), DEADLINE);
    let before_the_report = step(&mut world.model, at(102), CLOCK_CHANGED);

    assert_eq!(count(&written_back, is_set_keys), 1);
    assert_eq!(count(&before_the_report, is_set_keys), 0);
}

#[test]
fn reassert_now_does_not_stop_the_rotation() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);
    world.set_key(at(100), Style::Light, PICKED);
    world.run(at(101), DEADLINE);

    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(reported(&at_deadline).paused, PerStyle::default());
}

#[test]
fn reassert_now_writes_nothing_when_the_key_returns_by_itself() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);
    world.set_key(at(100), Style::Light, PICKED);

    let at_return = point_back(
        &mut world,
        at(100) + Duration::from_millis(500),
        Style::Light,
    );
    let a_second_later = world.run(at(101), DEADLINE);

    assert_eq!(armed(&at_return), [Some(at(900))]);
    assert_eq!(count(&a_second_later, is_set_keys), 0);
}

#[test]
fn more_than_three_flips_a_minute_pause_the_style() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);
    for flip_secs in [100, 110, 120] {
        world.set_key(at(flip_secs), Style::Light, PICKED);
        world.run(at(flip_secs + 1), DEADLINE);
    }

    let fourth = world.set_key(at(130), Style::Light, PICKED);
    let a_second_later = world.run(at(131), DEADLINE);

    assert_eq!(reported(&fourth).paused.light, Some(Pause::ExternalChange));
    assert!(is_paused(&world, Style::Light));
    assert_eq!(count(&a_second_later, is_set_keys), 0);
    assert_eq!(world.keys.picture_uri, PICKED);
}

#[test]
fn flips_that_are_more_than_a_minute_apart_are_written_back() {
    let mut world = running_with(ExternalChangePolicy::ReassertNow);
    for flip_secs in [100, 125, 150] {
        world.set_key(at(flip_secs), Style::Light, PICKED);
        world.run(at(flip_secs + 1), DEADLINE);
    }

    // The first flip is 75 s old by now.
    world.set_key(at(175), Style::Light, PICKED);
    let a_second_later = world.run(at(176), DEADLINE);

    assert!(!is_paused(&world, Style::Light));
    assert_eq!(keys_pointed(&a_second_later), [[Style::Light]]);
}

#[test]
fn reassert_next_tick_writes_the_key_back_after_the_next_rotation() {
    let mut world = running_with(ExternalChangePolicy::ReassertNextTick);

    let at_change = world.set_key(at(100), Style::Light, PICKED);
    let at_tick = world.run(at(900), DEADLINE);

    assert!(at_change.is_empty());
    assert_eq!(published(&at_tick), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(keys_pointed(&at_tick), [[Style::Light]]);
    let publish = at_tick.iter().position(is_publish);
    let set_keys = at_tick.iter().position(is_set_keys);
    assert!(publish < set_keys, "the key is back before the new image");
}

#[test]
fn a_tick_writes_back_every_key_that_waits_for_one() {
    let mut world = running_with(ExternalChangePolicy::ReassertNextTick);
    world.set_key(at(100), Style::Light, PICKED);
    world.set_key(at(101), Style::Dark, PICKED);

    // Only light is on screen, so only light has a tick.
    let at_tick = world.run(at(900), DEADLINE);

    let slots = slot_uris();
    let both = PerStyle {
        light: Some(slots.light.as_str()),
        dark: Some(slots.dark.as_str()),
    };
    assert_eq!(keys_written(&at_tick), [(both, None)]);
}

#[test]
fn reassert_next_tick_gives_a_style_with_one_image_a_tick() {
    let single = init(
        policy(ExternalChangePolicy::ReassertNextTick),
        &["/l/only.jpg"],
        &["/d/only.jpg"],
    );
    let mut world = World::new(single);
    world.start(at(0), Style::Light);

    let at_change = world.set_key(at(100), Style::Light, PICKED);
    let at_tick = world.run(at(1000), DEADLINE);
    let later = world.run(at(1900), DEADLINE);

    // One interval after the change.
    assert_eq!(armed(&at_change), [Some(at(1000))]);
    assert!(published(&at_tick).is_empty());
    assert_eq!(keys_pointed(&at_tick), [[Style::Light]]);
    // With its key back, the style has nothing to wait for again: the
    // timer stays spent.
    assert!(armed(&at_tick).is_empty());
    assert!(later.is_empty());
}

#[test]
fn reassert_next_tick_writes_a_hidden_one_image_style_back() {
    let single = init(
        policy(ExternalChangePolicy::ReassertNextTick),
        &["/l/only.jpg"],
        &["/d/only.jpg"],
    );
    let mut world = World::new(single);
    world.start(at(0), Style::Light);

    let at_change = world.set_key(at(100), Style::Dark, PICKED);
    let before = world.run(at(999), DEADLINE);
    let at_tick = world.run(at(1000), DEADLINE);

    assert_eq!(armed(&at_change), [Some(at(1000))]);
    assert!(published(&at_change).is_empty());
    assert!(keys_pointed(&before).is_empty());
    assert!(published(&at_tick).is_empty());
    assert_eq!(keys_pointed(&at_tick), [[Style::Dark]]);
}

#[test]
fn an_event_during_a_hidden_reassert_does_not_pull_it_forward() {
    let single = init(
        policy(ExternalChangePolicy::ReassertNextTick),
        &["/l/only.jpg"],
        &["/d/only.jpg"],
    );
    let mut world = World::new(single);
    world.start(at(0), Style::Light);
    world.set_key(at(100), Style::Dark, PICKED);

    let during = world.run(at(200), CLOCK_CHANGED);
    let too_soon = world.run(at(201), DEADLINE);

    assert_eq!(armed(&during).last(), Some(&Some(at(1000))));
    assert!(keys_pointed(&too_soon).is_empty());
}

#[test]
fn a_hidden_style_does_not_wait_on_a_stopped_clock() {
    let images = init(
        policy(ExternalChangePolicy::ReassertNextTick),
        &["/l/only.jpg"],
        &["/d/a.jpg", "/d/b.jpg"],
    );
    let mut world = World::new(images);
    world.start(at(0), Style::Light);

    let at_change = world.set_key(at(100), Style::Dark, PICKED);
    let before = world.run(at(999), DEADLINE);
    let at_due = world.run(at(1000), DEADLINE);

    assert_eq!(armed(&at_change), [Some(at(1000))]);
    assert!(keys_pointed(&before).is_empty());
    assert!(published(&at_due).is_empty());
    assert_eq!(keys_pointed(&at_due), [[Style::Dark]]);
}

#[test]
fn a_restart_with_every_style_paused_writes_no_picture_options() {
    let mut config = alphabetical();
    config.general.picture_options = Some(PictureOptions::Zoom);
    let mut first = World::new(two_folders_with(config.clone()));
    first.start(at(0), Style::Light);
    first.set_key(at(100), Style::Light, PICKED);
    first.set_key(at(101), Style::Dark, PICKED);
    first.keys.picture_options = "centered".to_owned();

    let mut second = first.restart(two_folders_with(config));
    let at_start = second.start(at(200), Style::Light);

    assert_eq!(count(&at_start, is_set_keys), 0);
    assert_eq!(second.keys.picture_options, "centered");
}
