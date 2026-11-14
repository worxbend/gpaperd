//! The write of the background keys: each key once, when its slot exists.

use gpaperd_core::config::{Config, EmptyStylePolicy, PictureOptions};
use gpaperd_core::event::Effect;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, at, count, dark_folder_starts_broken, init, is_ready,
    is_set_keys, keys_pointed, published, running_world, slot_uris, started, two_folders,
    two_folders_with, user_keys,
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
