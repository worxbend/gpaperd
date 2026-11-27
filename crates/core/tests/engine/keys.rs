//! The write of the background keys: each key once, when its slot exists.

use gpaperd_core::config::{Config, EmptyStylePolicy, ExternalChangePolicy, PictureOptions};
use gpaperd_core::control::{Command, Reply};
use gpaperd_core::event::{Effect, Event};
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, count, dark_folder_starts_broken,
    init, is_ready, is_set_keys, keys_changed, keys_pointed, keys_written, published,
    running_world, slot_uris, started, the_reply, two_folders, two_folders_with, user_keys,
};

fn set_keys_written(config: Config, picture_options: &str) -> Vec<Effect> {
    let mut keys = user_keys();
    keys.picture_options = picture_options.to_owned();
    let mut world = World::new(two_folders_with(config));
    let trace = world.run(at(0), started(Style::Light, keys, PerStyle::default()));
    trace.into_iter().filter(is_set_keys).collect()
}

/// The write that points both keys at their slots.
fn both_keys(picture_options: Option<PictureOptions>) -> Effect {
    slot_keys(&Style::ALL, picture_options)
}

/// The write that points the keys of `styles` at their slots.
fn slot_keys(styles: &[Style], picture_options: Option<PictureOptions>) -> Effect {
    let slot_uris = slot_uris();
    Effect::SetKeys {
        uris: PerStyle::from_fn(|style| styles.contains(&style).then(|| slot_uris[style].clone())),
        picture_options,
    }
}

#[test]
fn picture_options_are_left_alone_by_default() {
    let written = set_keys_written(alphabetical(), "scaled");

    assert_eq!(written, [both_keys(None)]);
}

#[test]
fn picture_options_none_is_replaced_by_zoom() {
    let written = set_keys_written(alphabetical(), "none");

    assert_eq!(written, [both_keys(Some(PictureOptions::Zoom))]);
}

#[test]
fn configured_picture_options_are_written_with_the_keys() {
    let mut config = alphabetical();
    config.general.picture_options = Some(PictureOptions::Spanned);

    let written = set_keys_written(config, "none");

    assert_eq!(written, [both_keys(Some(PictureOptions::Spanned))]);
}

#[test]
fn the_users_original_keys_are_saved_once() {
    let first = running_world();
    let mut keys_after_first_run = user_keys();
    keys_after_first_run.picture_uri = "file:///home/u/.local/share/gpaperd/slots/light".to_owned();

    let mut second = first.restart(two_folders());
    let slots = second.slots;
    second.run(at(60), started(Style::Light, keys_after_first_run, slots));

    assert_eq!(first.model.persisted().original_keys, Some(user_keys()));
    assert_eq!(second.model.persisted().original_keys, Some(user_keys()));
}

#[test]
fn slot_uris_found_without_a_state_file_are_not_saved_as_the_originals() {
    let first = running_world();
    let mut second = World::new(two_folders());
    second.keys = first.keys.clone();
    second.slots = first.slots;
    second.start(at(60), Style::Light);

    let trace = second.ask(at(70), 1, Command::Restore);

    assert_eq!(second.model.persisted().original_keys, None);
    let nothing_saved = Reply::Failed("no original keys are saved".to_owned());
    assert_eq!(the_reply(&trace), &nothing_saved);
}

#[test]
fn only_the_key_of_a_style_with_a_slot_is_written() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &[]));

    let trace = world.start(at(0), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    // Dark has no slot file: its key stays the user's.
    assert_eq!(keys_pointed(&trace), [[Style::Light]]);
    assert_eq!(count(&trace, is_ready), 1);
}

#[test]
fn a_first_apply_that_gave_up_does_not_hold_back_the_other_styles_key() {
    let mut world = dark_folder_starts_broken(alphabetical());

    let trace = world.start(at(0), Style::Light);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(keys_pointed(&trace), [[Style::Light]]);
}

#[test]
fn a_key_is_written_when_its_slot_appears_later() {
    let mut world = dark_folder_starts_broken(alphabetical());
    world.start(at(0), Style::Light);

    let trace = world.run(at(600), DEADLINE);

    assert_eq!(keys_pointed(&trace), [[Style::Dark]]);
}

#[test]
fn picture_options_are_written_with_the_first_key_only() {
    let mut config = alphabetical();
    config.general.picture_options = Some(PictureOptions::Spanned);
    let mut world = dark_folder_starts_broken(config);

    let mut trace = world.start(at(0), Style::Light);
    trace.extend(world.run(at(600), DEADLINE));

    let written: Vec<Effect> = trace.into_iter().filter(is_set_keys).collect();
    let light_key_and_options = slot_keys(&[Style::Light], Some(PictureOptions::Spanned));
    let dark_key = slot_keys(&[Style::Dark], None);
    assert_eq!(written, [light_key_and_options, dark_key]);
}

#[test]
fn a_refused_key_write_is_tried_again_after_a_minute() {
    let mut world = running_world();
    let slots = slot_uris();
    let failed = Event::KeyWriteFailed {
        uris: PerStyle {
            light: Some(slots.light),
            dark: Some(slots.dark),
        },
        picture_options: Some(PictureOptions::Zoom),
        unconfirmed: false,
    };

    let at_fail = world.run(at(100), failed);
    let too_soon = world.run(at(159), DEADLINE);
    let retried = world.run(at(160), DEADLINE);

    assert!(keys_pointed(&at_fail).is_empty());
    assert_eq!(armed(&at_fail), [Some(at(160))]);
    assert!(keys_pointed(&too_soon).is_empty());
    let zoom = keys_written(&retried).last().map(|(_, options)| *options);
    assert_eq!(zoom, Some(Some(PictureOptions::Zoom)));
}

#[test]
fn an_event_during_the_minute_does_not_pull_the_retry_forward() {
    let mut world = running_world();
    let slots = slot_uris();
    let failed = Event::KeyWriteFailed {
        uris: PerStyle {
            light: Some(slots.light),
            dark: Some(slots.dark),
        },
        picture_options: Some(PictureOptions::Zoom),
        unconfirmed: false,
    };

    world.run(at(100), failed);
    let during = world.run(at(120), CLOCK_CHANGED);
    let still_waiting = world.run(at(121), DEADLINE);
    let retried = world.run(at(160), DEADLINE);

    assert_eq!(armed(&during).last(), Some(&Some(at(160))));
    assert!(keys_pointed(&still_waiting).is_empty());
    assert!(!keys_pointed(&retried).is_empty());
}

#[test]
fn a_refused_picture_options_write_waits_a_minute() {
    let mut world = running_world();
    let failed = Event::KeyWriteFailed {
        uris: PerStyle {
            light: None,
            dark: None,
        },
        picture_options: Some(PictureOptions::Zoom),
        unconfirmed: false,
    };

    let at_fail = world.run(at(100), failed);
    let too_soon = world.run(at(159), DEADLINE);
    let retried = world.run(at(160), DEADLINE);

    assert!(keys_pointed(&at_fail).is_empty());
    assert_eq!(armed(&at_fail), [Some(at(160))]);
    assert!(keys_pointed(&too_soon).is_empty());
    let zoom = keys_written(&retried).last().map(|(_, options)| *options);
    assert_eq!(zoom, Some(Some(PictureOptions::Zoom)));
}

/// Settings did not answer the write of both slot URIs at second 100.
fn unconfirmed_slot_write() -> Event {
    let slots = slot_uris();
    Event::KeyWriteFailed {
        uris: PerStyle {
            light: Some(slots.light),
            dark: Some(slots.dark),
        },
        picture_options: None,
        unconfirmed: true,
    }
}

#[test]
fn an_unconfirmed_write_that_never_lands_is_tried_again_after_a_minute() {
    let mut world = running_world();

    let at_fail = world.run(at(100), unconfirmed_slot_write());
    let too_soon = world.run(at(159), DEADLINE);
    let retried = world.run(at(160), DEADLINE);

    assert_eq!(armed(&at_fail), [Some(at(160))]);
    assert!(keys_pointed(&too_soon).is_empty());
    assert_eq!(keys_pointed(&retried), [vec![Style::Light, Style::Dark]]);
}

#[test]
fn a_user_change_after_a_late_landing_is_an_external_change() {
    let mut config = alphabetical();
    config.general.on_external_change = ExternalChangePolicy::Pause;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    world.run(at(100), unconfirmed_slot_write());

    let landed_late = world.run(at(110), keys_changed(world.keys.clone()));
    world.set_key(at(120), Style::Light, "file:///home/u/Pictures/picked.jpg");
    let at_retry = world.run(at(160), DEADLINE);

    assert!(keys_pointed(&landed_late).is_empty());
    assert!(world.model.persisted().styles[Style::Light].paused);
    assert!(keys_pointed(&at_retry).is_empty());
}

#[test]
fn a_refused_write_of_one_style_still_retries_the_other() {
    let mut world = running_world();
    let slots = slot_uris();
    let failed = Event::KeyWriteFailed {
        uris: PerStyle {
            light: Some("file:///other".to_owned()),
            dark: Some(slots.dark),
        },
        picture_options: Some(PictureOptions::Zoom),
        unconfirmed: false,
    };

    let at_fail = world.run(at(100), failed);
    let too_soon = world.run(at(159), DEADLINE);
    let retried = world.run(at(160), DEADLINE);

    assert!(keys_pointed(&at_fail).is_empty());
    assert!(keys_pointed(&too_soon).is_empty());
    assert_eq!(keys_pointed(&retried), [vec![Style::Dark]]);
    let zoom = keys_written(&retried).last().map(|(_, options)| *options);
    assert_eq!(zoom, Some(Some(PictureOptions::Zoom)));
}

#[test]
fn a_user_change_while_a_refused_options_write_waits_is_an_external_change() {
    let mut config = alphabetical();
    config.general.on_external_change = ExternalChangePolicy::Pause;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    let options_failed = Event::KeyWriteFailed {
        uris: PerStyle {
            light: None,
            dark: None,
        },
        picture_options: Some(PictureOptions::Zoom),
        unconfirmed: false,
    };
    world.run(at(100), options_failed);

    world.set_key(at(110), Style::Light, "file:///home/u/Pictures/picked.jpg");
    world.run(at(160), DEADLINE);

    assert!(world.model.persisted().styles[Style::Light].paused);
    assert_eq!(world.keys.picture_uri, "file:///home/u/Pictures/picked.jpg");
}
