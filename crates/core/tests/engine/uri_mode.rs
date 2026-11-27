//! `apply_mode = "uri"`: a rotation writes its image's own URI to the key,
//! and no slot file is touched.

use gpaperd_core::catalog::{FileId, FsChange};
use gpaperd_core::config::{ApplyMode, Config, PictureOptions};
use gpaperd_core::control::{Command, Reply, StyleTarget};
use gpaperd_core::event::{Effect, Event, Method, Pause, SlotWrite};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, changed, control, count, found, id, init, is_ready,
    is_set_keys, keys_changed, keys_written, probed, published, removed, replies, reported,
    republished, running_world, slot_uris, started_with_locks, switch_to, the_reply,
    two_folders_with, version,
};

const PICKED: &str = "file:///home/u/Pictures/picked.jpg";

fn uri_mode() -> Config {
    let mut config = alphabetical();
    config.general.apply_mode = ApplyMode::Uri;
    config
}

/// A daemon in uri mode after its first run at second 0, light on screen.
fn running_in_uri_mode() -> World {
    let mut world = World::new(two_folders_with(uri_mode()));
    world.start(at(0), Style::Light);
    world
}

fn probe_passed(style: Style, path: &str) -> Event {
    Event::Probed {
        style,
        item: id(path),
        verdict: Ok(()),
    }
}

fn touches_a_slot(effect: &Effect) -> bool {
    matches!(effect, Effect::Publish { .. } | Effect::Republish(_))
}

/// One key write that names `light` and `dark`.
fn one_write<'a>(
    light: Option<&'a str>,
    dark: Option<&'a str>,
) -> [(PerStyle<Option<&'a str>>, Option<PictureOptions>); 1] {
    [(PerStyle { light, dark }, None)]
}

#[test]
fn a_first_run_writes_both_image_uris_in_one_write() {
    let mut world = World::new(two_folders_with(uri_mode()));

    let trace = world.start(at(0), Style::Light);

    assert_eq!(
        probed(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/d/a.jpg")]
    );
    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/a.jpg"), Some("file:///d/a.jpg"))
    );
    assert_eq!(count(&trace, touches_a_slot), 0);
    assert_eq!(count(&trace, is_ready), 1);
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn a_rotation_writes_the_new_images_uri_to_its_key() {
    let mut world = running_in_uri_mode();

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/b.jpg"), None)
    );
    assert_eq!(count(&trace, touches_a_slot), 0);
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
    assert!(trace.contains(&Effect::Persist));
    assert_eq!(armed(&trace), [Some(at(1800))]);
}

#[test]
fn the_daemons_own_rotation_is_no_external_change() {
    let mut world = running_in_uri_mode();
    world.run(at(900), DEADLINE);

    let trace = world.run(at(1800), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(reported(&trace).paused, PerStyle::default());
}

#[test]
fn an_image_that_fails_its_probe_is_skipped() {
    let mut world = running_in_uri_mode();
    world.broken.insert(id("/l/b.jpg"));

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/c.jpg"), None)
    );
}

#[test]
fn a_path_is_percent_encoded_in_its_uri() {
    let odd = init(uri_mode(), &["/l/a b#1.jpg"], &[]);
    let mut world = World::new(odd);

    world.start(at(0), Style::Light);

    assert_eq!(world.keys.picture_uri, "file:///l/a%20b%231.jpg");
}

#[test]
fn a_cached_image_uri_found_without_a_state_file_is_not_saved_as_the_original() {
    let mut world = World::new(two_folders_with(uri_mode()));
    world.keys.picture_uri = "file:///cache/img.test/a%20b.jpg/v1.jpg".to_owned();

    world.start(at(0), Style::Light);

    assert_eq!(world.model.persisted().original_keys, None);
}

#[test]
fn a_uri_beside_the_cache_directory_is_saved_as_the_original() {
    let mut world = World::new(two_folders_with(uri_mode()));
    world.keys.picture_uri = "file:///cache-old/day.jpg".to_owned();

    world.start(at(0), Style::Light);

    let saved = world.model.persisted().original_keys.expect("originals");
    assert_eq!(saved.picture_uri, "file:///cache-old/day.jpg");
}

#[test]
fn a_key_that_no_longer_holds_the_last_uri_is_an_external_change() {
    let mut world = running_in_uri_mode();

    let at_change = world.set_key(at(100), Style::Light, PICKED);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(
        reported(&at_change).paused.light,
        Some(Pause::ExternalChange)
    );
    assert!(probed(&at_deadline).is_empty());
    assert_eq!(world.keys.picture_uri, PICKED);
}

#[test]
fn a_pause_ends_when_the_key_holds_the_last_uri_again() {
    let mut world = running_in_uri_mode();
    world.set_key(at(100), Style::Light, PICKED);

    let trace = world.set_key(at(200), Style::Light, "file:///l/a.jpg");

    assert_eq!(reported(&trace).paused, PerStyle::default());
}

#[test]
fn a_pause_from_the_last_run_ends_when_the_key_holds_the_last_uri_again() {
    let mut first = running_in_uri_mode();
    first.set_key(at(100), Style::Light, PICKED);

    let mut second = first.restart(two_folders_with(uri_mode()));
    let at_start = second.start(at(200), Style::Light);
    let at_return = second.set_key(at(300), Style::Light, "file:///l/a.jpg");

    assert_eq!(
        reported(&at_start).paused.light,
        Some(Pause::ExternalChange)
    );
    assert_eq!(reported(&at_return).paused, PerStyle::default());
    assert!(!second.model.persisted().styles.light.paused);
}

#[test]
fn a_user_paused_key_that_still_names_the_last_image_stays_the_daemons() {
    let mut first = running_in_uri_mode();
    first.ask(at(100), 1, Command::Pause(StyleTarget::Light));

    let mut second = first.restart(two_folders_with(uri_mode()));
    second.start(at(200), Style::Light);
    second.set_key(at(300), Style::Light, PICKED);

    // The key named the daemon's image, so a change to it is noticed.
    assert!(second.model.persisted().styles.light.paused);
}

#[test]
fn both_styles_that_rotate_together_share_one_key_write() {
    let mut config = uri_mode();
    config.general.rotate_hidden = true;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);

    let trace = world.run(at(900), DEADLINE);

    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/b.jpg"), Some("file:///d/b.jpg"))
    );
}

#[test]
fn a_restart_checks_each_image_again_and_keeps_its_interval() {
    let first = running_in_uri_mode();

    let mut second = first.restart(two_folders_with(uri_mode()));
    let trace = second.start(at(60), Style::Light);

    assert_eq!(
        probed(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/d/a.jpg")]
    );
    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/a.jpg"), Some("file:///d/a.jpg"))
    );
    assert_eq!(count(&trace, touches_a_slot), 0);
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn a_restart_into_uri_mode_shows_each_image_by_its_uri() {
    // The last run was in slots mode: both slot files are intact.
    let first = running_world();

    let mut second = first.restart(two_folders_with(uri_mode()));
    let trace = second.start(at(60), Style::Light);

    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/a.jpg"), Some("file:///d/a.jpg"))
    );
    assert_eq!(count(&trace, touches_a_slot), 0);
}

#[test]
fn a_restart_whose_image_is_gone_shows_the_next_one() {
    let first = running_in_uri_mode();
    let mut second = first.restart(two_folders_with(uri_mode()));
    second.broken.insert(id("/l/a.jpg"));

    let trace = second.start(at(60), Style::Light);

    assert_eq!(second.keys.picture_uri, "file:///l/b.jpg");
    // Another image: its interval starts now.
    assert_eq!(armed(&trace), [Some(at(960))]);
}

#[test]
fn a_style_switch_republishes_nothing() {
    let mut world = running_in_uri_mode();

    let at_switch = world.run(at(300), switch_to(Style::Dark));
    let later = world.run(at(301), DEADLINE);

    // Nothing is planned but dark's next rotation, and the switch itself
    // writes no slot and no key.
    assert_eq!(armed(&at_switch), [Some(at(1200))]);
    assert_eq!(count(&at_switch, touches_a_slot), 0);
    assert_eq!(count(&at_switch, is_set_keys), 0);
    assert_eq!(count(&later, touches_a_slot), 0);
    assert_eq!(count(&later, is_set_keys), 0);
}

#[test]
fn an_edited_image_is_checked_again_and_no_key_is_written() {
    let mut world = running_in_uri_mode();
    let edited = vec![FsChange::Found(version("/l/a.jpg", 1))];

    let trace = world.run(at(100), changed(Style::Light, edited));

    // The shell watches the file that its key names.
    assert_eq!(probed(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(count(&trace, is_set_keys), 0);
    assert_eq!(count(&trace, touches_a_slot), 0);
    // The image is the same: its interval goes on.
    assert_eq!(armed(&trace).last(), Some(&Some(at(900))));
}

#[test]
fn an_image_on_screen_that_is_deleted_is_replaced_at_once() {
    let mut world = running_in_uri_mode();

    let trace = world.run(at(100), changed(Style::Light, vec![removed("/l/a.jpg")]));

    // The key named the deleted file, so the shell has nothing to show.
    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/b.jpg"), None)
    );
    assert_eq!(armed(&trace).last(), Some(&Some(at(1000))));
}

#[test]
fn a_deleted_image_of_a_hidden_style_is_replaced_too() {
    let mut world = running_in_uri_mode();

    let trace = world.run(at(100), changed(Style::Dark, vec![removed("/d/a.jpg")]));

    assert_eq!(
        keys_written(&trace),
        one_write(None, Some("file:///d/b.jpg"))
    );
}

#[test]
fn a_lost_slot_file_is_not_written_again() {
    let mut world = running_in_uri_mode();

    let trace = world.run(at(100), Event::SlotLost(Style::Light));

    assert_eq!(count(&trace, touches_a_slot), 0);
    assert_eq!(count(&trace, is_set_keys), 0);
}

#[test]
fn a_locked_key_idles_its_style_whatever_it_holds() {
    // The last run was in slots mode, so the key is locked on the slot URI.
    // Slots mode can live with that; uri mode cannot.
    let mut second = running_world().restart(two_folders_with(uri_mode()));
    let locked = PerStyle {
        light: true,
        dark: false,
    };
    let event = started_with_locks(Style::Light, second.keys.clone(), second.slots, locked);

    let trace = second.run(at(60), event);

    assert_eq!(reported(&trace).paused.light, Some(Pause::LockedKey));
    assert_eq!(probed(&trace), [(Style::Dark, "/d/a.jpg")]);
}

#[test]
fn a_reload_to_uri_mode_points_the_keys_at_the_images() {
    let mut world = running_world();

    let trace = world.run(at(100), Event::ConfigChanged(Ok(uri_mode())));

    assert_eq!(
        keys_written(&trace),
        one_write(Some("file:///l/a.jpg"), Some("file:///d/a.jpg"))
    );
    assert_eq!(count(&trace, touches_a_slot), 0);
    // The images are the same: light's interval goes on.
    assert_eq!(armed(&trace).last(), Some(&Some(at(900))));
}

#[test]
fn a_reload_to_slots_mode_fills_the_slots_and_points_the_keys_at_them() {
    let mut world = running_in_uri_mode();

    let trace = world.run(at(100), Event::ConfigChanged(Ok(alphabetical())));

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/d/a.jpg")]
    );
    let slots = slot_uris();
    assert_eq!(
        keys_written(&trace),
        one_write(Some(&slots.light), Some(&slots.dark))
    );
    assert!(republished(&trace).is_empty());
}

#[test]
fn a_slot_write_that_a_reload_to_uri_mode_overtakes_is_followed_by_the_images_uri() {
    let mut world = running_world();
    let probe_passed = probe_passed(Style::Light, "/l/b.jpg");
    let slot_written = Event::Published {
        style: Style::Light,
        item: id("/l/b.jpg"),
        result: Ok(SlotWrite {
            method: Method::Copy,
            slot: FileId { dev: 2, ino: 77 },
        }),
    };
    // Light's next image is on its way into the slot when the mode changes.
    step(&mut world.model, at(900), DEADLINE);
    step(&mut world.model, at(900), probe_passed);
    world.run(at(900), Event::ConfigChanged(Ok(uri_mode())));

    let trace = world.run(at(901), slot_written);

    assert_eq!(probed(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(world.keys.picture_uri, "file:///l/b.jpg");
}

#[test]
fn a_reload_forgets_what_the_other_mode_showed() {
    let mut world = running_in_uri_mode();
    // Light's next image is being probed when the mode changes to slots,
    // so it goes into the slot. The rotation after it is in slots mode too.
    step(&mut world.model, at(900), DEADLINE);
    world.run(at(900), Event::ConfigChanged(Ok(alphabetical())));
    world.run(at(901), probe_passed(Style::Light, "/l/b.jpg"));
    world.run(at(1801), DEADLINE);

    world.run(at(1900), Event::ConfigChanged(Ok(uri_mode())));

    // Not the image that uri mode showed last.
    assert_eq!(world.keys.picture_uri, "file:///l/c.jpg");
}

#[test]
fn a_dirty_slot_file_is_not_refreshed_in_uri_mode() {
    // A run in slots mode left the dark slot dirty. The next run is in uri
    // mode, shows dark, and has no image to put in its place.
    let mut second = running_world().restart(init(uri_mode(), &[], &[]));

    let at_start = second.start(at(60), Style::Dark);
    let later = second.run(at(61), DEADLINE);

    assert!(armed(&at_start).is_empty());
    assert_eq!(count(&at_start, touches_a_slot), 0);
    assert_eq!(count(&at_start, is_set_keys), 0);
    assert_eq!(count(&later, touches_a_slot), 0);
    assert_eq!(count(&later, is_set_keys), 0);
}

#[test]
fn a_reload_to_slots_mode_does_not_trust_a_slot_file_from_an_earlier_run() {
    // A run in slots mode left `/l/a.jpg` in the light slot. The next run
    // is in uri mode and moves on to `/l/b.jpg`.
    let mut world = running_world().restart(two_folders_with(uri_mode()));
    world.start(at(60), Style::Light);
    world.run(at(900), DEADLINE);

    let trace = world.run(at(1000), Event::ConfigChanged(Ok(alphabetical())));

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Dark, "/d/a.jpg")]
    );
}

#[test]
fn a_borrowed_image_stays_until_the_tick_when_its_style_gets_an_image_of_its_own() {
    let mut world = World::new(init(uri_mode(), &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Dark);
    assert_eq!(world.current(Style::Dark), Some(id("/l/a.jpg")));

    let at_change = world.run(at(100), changed(Style::Dark, vec![found("/d/own.jpg")]));
    let at_deadline = world.run(at(900), DEADLINE);

    assert!(probed(&at_change).is_empty());
    assert_eq!(count(&at_change, is_set_keys), 0);
    assert_eq!(probed(&at_deadline), [(Style::Dark, "/d/own.jpg")]);
    assert_eq!(world.current(Style::Dark), Some(id("/d/own.jpg")));
}

#[test]
fn a_lock_at_start_keeps_the_last_uri_and_the_pause_of_the_last_run() {
    let mut first = running_in_uri_mode();
    first.set_key(at(100), Style::Light, PICKED);
    let mut second = first.restart(two_folders_with(uri_mode()));
    let locked = PerStyle {
        light: true,
        dark: false,
    };
    let event = started_with_locks(Style::Light, second.keys.clone(), second.slots, locked);
    second.run(at(200), event);

    let saved = second.model.persisted().styles.light;

    assert!(saved.paused);
    assert_eq!(saved.key_uri.as_deref(), Some("file:///l/a.jpg"));
}

#[test]
fn a_planned_key_write_does_not_announce_the_image() {
    let mut world = running_in_uri_mode();
    step(&mut world.model, at(900), DEADLINE);

    let planned = step(
        &mut world.model,
        at(901),
        probe_passed(Style::Light, "/l/b.jpg"),
    );
    let failed = step(
        &mut world.model,
        at(902),
        Event::KeyWriteFailed {
            uris: PerStyle {
                light: Some("file:///l/b.jpg".to_owned()),
                dark: None,
            },
            picture_options: None,
        },
    );

    let announced = |effect: &Effect| matches!(effect, Effect::Changed { .. });
    assert!(planned.iter().any(is_set_keys));
    assert!(!planned.iter().any(announced));
    assert!(!failed.iter().any(announced));
}

#[test]
fn next_is_not_done_before_the_uri_key_write() {
    let mut world = running_in_uri_mode();
    step(
        &mut world.model,
        at(10),
        control(1, Command::Next(StyleTarget::Visible)),
    );

    let planned = step(
        &mut world.model,
        at(11),
        probe_passed(Style::Light, "/l/b.jpg"),
    );
    assert!(planned.iter().any(is_set_keys));
    assert!(replies(&planned).is_empty());

    world.keys.picture_uri = "file:///l/b.jpg".to_owned();
    let landed = step(&mut world.model, at(12), keys_changed(world.keys.clone()));

    assert!(landed.contains(&Effect::Changed {
        style: Style::Light,
        item: id("/l/b.jpg"),
    }));
    assert_eq!(the_reply(&landed), &Reply::Done);
}

#[test]
fn set_is_not_done_before_the_uri_key_write() {
    let mut world = running_in_uri_mode();
    step(
        &mut world.model,
        at(10),
        control(1, Command::Set(StyleTarget::Light, id("/l/c.jpg"))),
    );

    let planned = step(
        &mut world.model,
        at(11),
        probe_passed(Style::Light, "/l/c.jpg"),
    );
    assert!(planned.iter().any(is_set_keys));
    assert!(replies(&planned).is_empty());

    world.keys.picture_uri = "file:///l/c.jpg".to_owned();
    let landed = step(&mut world.model, at(12), keys_changed(world.keys.clone()));

    assert!(landed.contains(&Effect::Changed {
        style: Style::Light,
        item: id("/l/c.jpg"),
    }));
    assert_eq!(the_reply(&landed), &Reply::Done);
}

/// GNOME refused the write of `/l/b.jpg` to the light key.
fn light_b_refused() -> Event {
    Event::KeyWriteFailed {
        uris: PerStyle {
            light: Some("file:///l/b.jpg".to_owned()),
            dark: None,
        },
        picture_options: None,
    }
}

/// A uri-mode daemon whose write of `/l/b.jpg` to the light key, planned at
/// second 901 after `rotate` at second 900, was refused at second 902. The
/// key still holds `/l/a.jpg`.
fn light_b_write_refused(mut world: World, rotate: Event) -> World {
    step(&mut world.model, at(900), rotate);
    let planned = step(
        &mut world.model,
        at(901),
        probe_passed(Style::Light, "/l/b.jpg"),
    );
    assert!(planned.iter().any(is_set_keys));
    step(&mut world.model, at(902), light_b_refused());
    world
}

#[test]
fn a_user_change_while_a_refused_uri_write_waits_is_an_external_change() {
    let mut world = light_b_write_refused(running_in_uri_mode(), DEADLINE);

    world.set_key(at(910), Style::Light, PICKED);
    let at_retry = world.run(at(962), DEADLINE);

    assert!(world.model.persisted().styles[Style::Light].paused);
    assert!(keys_written(&at_retry).is_empty());
    assert_eq!(world.keys.picture_uri, PICKED);
}

#[test]
fn a_report_of_the_unchanged_key_does_not_pull_a_refused_write_forward() {
    let mut world = light_b_write_refused(running_in_uri_mode(), DEADLINE);

    let report = world.run(at(910), keys_changed(world.keys.clone()));
    let at_retry = world.run(at(962), DEADLINE);

    assert!(keys_written(&report).is_empty());
    assert_eq!(
        keys_written(&at_retry),
        one_write(Some("file:///l/b.jpg"), None)
    );
}

#[test]
fn a_refused_write_of_a_user_paused_style_is_still_tried_again() {
    let mut world = running_in_uri_mode();
    world.ask(at(100), 1, Command::Pause(StyleTarget::Light));
    let next = control(2, Command::Next(StyleTarget::Light));
    let mut world = light_b_write_refused(world, next);

    let saved = world.model.persisted().styles.light;
    let at_retry = world.run(at(962), DEADLINE);

    assert_eq!(saved.key_uri.as_deref(), Some("file:///l/a.jpg"));
    assert_eq!(
        keys_written(&at_retry),
        one_write(Some("file:///l/b.jpg"), None)
    );
}

#[test]
fn a_user_change_while_a_due_retry_waits_for_a_rotation_is_an_external_change() {
    let mut world = light_b_write_refused(running_in_uri_mode(), DEADLINE);
    let next = control(1, Command::Next(StyleTarget::Visible));
    let busy = step(&mut world.model, at(950), next);
    assert_eq!(probed(&busy).len(), 1);
    step(&mut world.model, at(962), DEADLINE);

    world.keys.picture_uri = PICKED.to_owned();
    step(&mut world.model, at(970), keys_changed(world.keys.clone()));

    assert!(world.model.persisted().styles[Style::Light].paused);
}

#[test]
fn a_retry_that_is_refused_again_still_saves_the_uri_the_key_holds() {
    let mut world = light_b_write_refused(running_in_uri_mode(), DEADLINE);
    let retried = step(&mut world.model, at(962), DEADLINE);
    assert!(retried.iter().any(is_set_keys));

    step(&mut world.model, at(963), light_b_refused());

    let saved = world.model.persisted().styles.light;
    assert_eq!(saved.key_uri.as_deref(), Some("file:///l/a.jpg"));
}
