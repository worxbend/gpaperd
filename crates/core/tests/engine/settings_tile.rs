//! gpaperd's tile in the wallpaper picker of GNOME Settings: offered in
//! slots mode, withdrawn in uri mode, and the keys it writes when picked.

use std::time::Duration;

use gpaperd_core::config::{ApplyMode, Config, PictureOptions};
use gpaperd_core::event::{BackgroundKeys, Effect, Event, Pause, Tile};
use gpaperd_core::model::ModelInit;
use gpaperd_core::style::Style;

use crate::support::{
    World, alphabetical, at, keys_changed, reported, running_world, slot_uris, two_folders_with,
    unscanned,
};

/// Every `Tile` effect in `trace`, in order.
fn tiles(trace: &[Effect]) -> Vec<Tile> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Tile(tile) => Some(*tile),
            _ => None,
        })
        .collect()
}

fn reloaded(config: Config) -> Event {
    Event::ConfigChanged(Ok(config))
}

/// The running config of [`running_world`], with a change.
fn changed(change: impl FnOnce(&mut Config)) -> Config {
    let mut config = alphabetical();
    change(&mut config);
    config
}

fn started_on(config: Config) -> Vec<Effect> {
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light)
}

#[test]
fn a_start_in_slots_mode_offers_the_tile_with_zoom_when_no_options_are_set() {
    let trace = started_on(alphabetical());

    assert_eq!(tiles(&trace), [Tile::Offer(PictureOptions::Zoom)]);
}

#[test]
fn a_start_in_slots_mode_offers_the_tile_with_the_configured_options() {
    let trace = started_on(changed(|config| {
        config.general.picture_options = Some(PictureOptions::Spanned);
    }));

    assert_eq!(tiles(&trace), [Tile::Offer(PictureOptions::Spanned)]);
}

#[test]
fn a_start_in_uri_mode_withdraws_the_tile() {
    let trace = started_on(changed(|config| config.general.apply_mode = ApplyMode::Uri));

    assert_eq!(tiles(&trace), [Tile::Withdraw]);
}

#[test]
fn a_start_without_a_config_file_leaves_the_tile_alone() {
    let mut world = World::new(ModelInit {
        config: None,
        ..unscanned(Config::default())
    });

    let trace = world.start(at(0), Style::Light);

    assert_eq!(tiles(&trace), []);
}

#[test]
fn the_first_config_after_a_start_without_one_offers_the_tile() {
    let mut world = World::new(ModelInit {
        config: None,
        ..unscanned(Config::default())
    });
    world.start(at(0), Style::Light);

    let trace = world.run(at(10), reloaded(alphabetical()));

    assert_eq!(tiles(&trace), [Tile::Offer(PictureOptions::Zoom)]);
}

#[test]
fn a_reload_into_uri_mode_withdraws_the_tile() {
    let mut world = running_world();

    let trace = world.run(
        at(10),
        reloaded(changed(|config| config.general.apply_mode = ApplyMode::Uri)),
    );

    assert_eq!(tiles(&trace), [Tile::Withdraw]);
}

#[test]
fn a_reload_back_into_slots_mode_offers_the_tile_again() {
    let uri_mode = changed(|config| config.general.apply_mode = ApplyMode::Uri);
    let mut world = World::new(two_folders_with(uri_mode));
    world.start(at(0), Style::Light);

    let trace = world.run(at(10), reloaded(alphabetical()));

    assert_eq!(tiles(&trace), [Tile::Offer(PictureOptions::Zoom)]);
}

#[test]
fn a_reload_with_new_picture_options_offers_the_tile_with_them() {
    let mut world = running_world();

    let trace = world.run(
        at(10),
        reloaded(changed(|config| {
            config.general.picture_options = Some(PictureOptions::Centered);
        })),
    );

    assert_eq!(tiles(&trace), [Tile::Offer(PictureOptions::Centered)]);
}

#[test]
fn a_reload_that_keeps_the_tile_as_it_is_leaves_it_alone() {
    let mut world = running_world();

    let trace = world.run(
        at(10),
        reloaded(changed(|config| {
            config.styles.light.interval = Duration::from_secs(3600);
            // Unset already offers zoom.
            config.general.picture_options = Some(PictureOptions::Zoom);
        })),
    );

    assert_eq!(tiles(&trace), []);
}

#[test]
fn a_reload_in_uri_mode_with_new_picture_options_leaves_the_tile_alone() {
    let uri_mode = changed(|config| config.general.apply_mode = ApplyMode::Uri);
    let mut world = World::new(two_folders_with(uri_mode));
    world.start(at(0), Style::Light);

    let trace = world.run(
        at(10),
        reloaded(changed(|config| {
            config.general.apply_mode = ApplyMode::Uri;
            config.general.picture_options = Some(PictureOptions::Scaled);
        })),
    );

    assert_eq!(tiles(&trace), []);
}

#[test]
fn picking_the_tile_in_settings_ends_the_external_change_pause_of_both_styles() {
    let mut world = running_world();
    let foreign = BackgroundKeys {
        picture_uri: "file:///home/u/Pictures/mine.jpg".to_owned(),
        picture_uri_dark: Some("file:///home/u/Pictures/mine-dark.jpg".to_owned()),
        picture_options: "scaled".to_owned(),
    };
    world.keys = foreign.clone();
    let paused = world.run(at(10), keys_changed(foreign));
    let slots = slot_uris();
    // Settings writes both keys and the tile's options in one apply.
    world.keys = BackgroundKeys {
        picture_uri: slots.light.clone(),
        picture_uri_dark: Some(slots.dark.clone()),
        picture_options: "zoom".to_owned(),
    };

    let picked = world.run(at(20), keys_changed(world.keys.clone()));

    let paused_status = reported(&paused);
    assert_eq!(paused_status.paused.light, Some(Pause::ExternalChange));
    assert_eq!(paused_status.paused.dark, Some(Pause::ExternalChange));
    let picked_status = reported(&picked);
    assert_eq!(picked_status.paused.light, None);
    assert_eq!(picked_status.paused.dark, None);
}
