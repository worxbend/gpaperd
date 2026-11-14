//! Keys that the daemon cannot write: locked by an administrator, or missing
//! before GNOME 42.

use gpaperd_core::config::{ApplyMode, Config};
use gpaperd_core::event::{BackgroundKeys, Effect, Event, GnomeEvent, Pause};
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, changed, found, init, is_set_keys, keys_pointed,
    probed, published, reported, running_world, slot_uris, started, started_with_locks,
    two_folders, user_keys,
};

/// A wallpaper that the user picked in Settings.
const PICKED: &str = "file:///home/u/Pictures/picked.jpg";

const LIGHT_LOCKED: PerStyle<bool> = PerStyle {
    light: true,
    dark: false,
};

/// The keys of a GNOME release that has no dark style.
fn keys_before_gnome_42() -> BackgroundKeys {
    BackgroundKeys {
        picture_uri_dark: None,
        ..user_keys()
    }
}

/// Starts a first run on a GNOME release without a dark key.
fn start_before_gnome_42(world: &mut World) -> Vec<Effect> {
    let slots = world.slots;
    world.keys = keys_before_gnome_42();
    world.run(at(0), started(Style::Light, world.keys.clone(), slots))
}

/// Starts `world` with light's key locked on the value it holds.
fn start_with_light_locked(world: &mut World, now_secs: u64) -> Vec<Effect> {
    let locked = started_with_locks(Style::Light, world.keys.clone(), world.slots, LIGHT_LOCKED);
    world.run(at(now_secs), locked)
}

#[test]
fn a_locked_key_that_names_something_else_idles_its_style() {
    let mut world = World::new(two_folders());

    let at_start = start_with_light_locked(&mut world, 0);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(published(&at_start), [(Style::Dark, "/d/a.jpg")]);
    assert_eq!(keys_pointed(&at_start), [[Style::Dark]]);
    assert_eq!(reported(&at_start).paused.light, Some(Pause::LockedKey));
    assert!(armed(&at_start).is_empty());
    assert!(probed(&at_deadline).is_empty());
}

#[test]
fn a_style_with_a_locked_key_is_left_alone_whatever_happens() {
    let mut world = World::new(two_folders());
    start_with_light_locked(&mut world, 0);

    let at_new_image = world.run(at(100), changed(Style::Light, vec![found("/l/new.jpg")]));
    let at_lost_slot = world.run(at(200), Event::SlotLost(Style::Light));

    assert!(probed(&at_new_image).is_empty());
    assert!(probed(&at_lost_slot).is_empty());
}

#[test]
fn a_locked_key_that_names_the_slot_rotates_as_usual() {
    let mut second = running_world().restart(two_folders());

    // The first run pointed the key at the slot; then it was locked there.
    let at_start = start_with_light_locked(&mut second, 60);
    let at_deadline = second.run(at(900), DEADLINE);

    assert_eq!(reported(&at_start).paused, PerStyle::default());
    // The locked key is right as it is: only dark's is written.
    assert_eq!(keys_pointed(&at_start), [[Style::Dark]]);
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

/// GNOME's report that the keys of `locked` are locked now, and no other.
fn locks_changed(locked: PerStyle<bool>) -> Event {
    Event::Gnome(GnomeEvent::LocksChanged(locked))
}

fn reloaded(config: Config) -> Event {
    Event::ConfigChanged(Ok(config))
}

#[test]
fn a_lock_that_is_lifted_at_runtime_wakes_its_style() {
    let mut world = World::new(two_folders());
    start_with_light_locked(&mut world, 0);

    let trace = world.run(at(100), locks_changed(PerStyle::default()));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(keys_pointed(&trace), [[Style::Light]]);
    assert_eq!(reported(&trace).paused, PerStyle::default());
}

#[test]
fn a_key_locked_on_its_slot_at_runtime_rotates_as_usual() {
    let mut world = running_world();

    let at_lock = world.run(at(100), locks_changed(LIGHT_LOCKED));
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(!at_lock.iter().any(is_set_keys));
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_key_locked_elsewhere_at_runtime_idles_its_style() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);

    let at_lock = world.run(at(200), locks_changed(LIGHT_LOCKED));
    // A reload ends an external-change pause, but no lock.
    let at_reload = world.run(at(300), reloaded(alphabetical()));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(reported(&at_lock).paused.light, Some(Pause::LockedKey));
    assert!(!keys_pointed(&at_reload).concat().contains(&Style::Light));
    assert!(probed(&at_deadline).is_empty());
    assert_eq!(world.keys.picture_uri, PICKED);
}

#[test]
fn a_locked_key_that_is_set_to_the_slot_wakes_its_style() {
    let mut world = World::new(two_folders());
    start_with_light_locked(&mut world, 0);

    let slot_uri = slot_uris().light;
    let trace = world.set_key(at(100), Style::Light, &slot_uri);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(reported(&trace).paused, PerStyle::default());
    // The locked key is right as it is.
    assert!(!keys_pointed(&trace).concat().contains(&Style::Light));
}

#[test]
fn a_reload_to_uri_mode_idles_a_key_locked_on_its_slot() {
    let mut world = running_world();
    world.run(at(50), locks_changed(LIGHT_LOCKED));
    let mut uri_mode = alphabetical();
    uri_mode.general.apply_mode = ApplyMode::Uri;

    let trace = world.run(at(100), reloaded(uri_mode));

    assert_eq!(reported(&trace).paused.light, Some(Pause::LockedKey));
    assert!(!keys_pointed(&trace).concat().contains(&Style::Light));
}

#[test]
fn before_gnome_42_only_the_light_style_runs() {
    let mut world = World::new(two_folders());

    let at_start = start_before_gnome_42(&mut world);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(published(&at_start), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(keys_pointed(&at_start), [[Style::Light]]);
    assert_eq!(reported(&at_start).paused.dark, Some(Pause::NoKey));
    assert_eq!(published(&at_deadline), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(world.keys.picture_uri_dark, None);
}

#[test]
fn before_gnome_42_an_empty_light_style_still_borrows_the_dark_images() {
    let mut world = World::new(init(alphabetical(), &[], &["/d/a.jpg", "/d/b.jpg"]));

    let at_start = start_before_gnome_42(&mut world);

    assert_eq!(published(&at_start), [(Style::Light, "/d/a.jpg")]);
}

#[test]
fn a_lock_keeps_an_external_change_pause_across_a_restart() {
    let mut first = running_world();
    first.set_key(at(100), Style::Light, PICKED);
    first.run(at(200), locks_changed(LIGHT_LOCKED));
    let saved_while_locked = first.model.persisted().styles.light.paused;

    // The administrator lifts the lock while the daemon is stopped.
    let mut second = first.restart(two_folders());
    let at_start = second.start(at(300), Style::Light);

    assert!(saved_while_locked);
    assert_eq!(
        reported(&at_start).paused.light,
        Some(Pause::ExternalChange)
    );
    assert!(!keys_pointed(&at_start).concat().contains(&Style::Light));
    assert_eq!(second.keys.picture_uri, PICKED);
}

#[test]
fn a_saved_pause_survives_a_lock_at_start_that_is_lifted_at_runtime() {
    let mut first = running_world();
    first.set_key(at(100), Style::Light, PICKED);
    let mut second = first.restart(two_folders());
    start_with_light_locked(&mut second, 200);

    let at_unlock = second.run(at(300), locks_changed(PerStyle::default()));
    let at_return = second.set_key(at(400), Style::Light, &slot_uris().light);

    assert_eq!(
        reported(&at_unlock).paused.light,
        Some(Pause::ExternalChange)
    );
    assert!(!keys_pointed(&at_unlock).concat().contains(&Style::Light));
    assert_eq!(reported(&at_return).paused, PerStyle::default());
}

#[test]
fn a_reload_under_a_lock_ends_the_waiting_pause_so_the_key_is_written_once_free() {
    let mut world = running_world();
    world.set_key(at(100), Style::Light, PICKED);
    world.run(at(200), locks_changed(LIGHT_LOCKED));
    world.run(at(300), reloaded(alphabetical()));

    let at_unlock = world.run(at(400), locks_changed(PerStyle::default()));

    assert!(!world.model.persisted().styles.light.paused);
    assert_eq!(reported(&at_unlock).paused, PerStyle::default());
    assert_eq!(world.keys.picture_uri, slot_uris().light);
}
