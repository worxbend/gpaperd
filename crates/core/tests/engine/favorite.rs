//! `gpaperctl favorite`: copy the image on screen into `favorites_dir`.

use std::path::PathBuf;

use gpaperd_core::config::Config;
use gpaperd_core::control::{Command, Reply, StyleTarget};
use gpaperd_core::event::{Effect, Event, Favorite};
use gpaperd_core::style::Style;

use crate::support::{
    World, alphabetical, at, blob, cache_loaded, id, init, remote_config, running_world, the_reply,
    two_folders_with,
};

const FAVORITES: &str = "/home/u/favorites";

fn with_favorites(mut config: Config) -> Config {
    config.general.favorites_dir = Some(PathBuf::from(FAVORITES));
    config
}

fn favorite_world() -> World {
    let mut world = World::new(two_folders_with(with_favorites(alphabetical())));
    world.start(at(0), Style::Light);
    world
}

fn favorite(source: &str, name: &str) -> Favorite {
    Favorite {
        source: PathBuf::from(source),
        name: name.to_owned(),
    }
}

/// The images of every `SaveFavorites` effect in `trace`, in order.
fn saved(trace: &[Effect]) -> Vec<Vec<Favorite>> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::SaveFavorites { dir, images, .. } => {
                assert_eq!(dir, &PathBuf::from(FAVORITES));
                Some(images.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn favorite_copies_the_image_on_screen_and_replies_once_it_is_saved() {
    let mut world = favorite_world();

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    assert_eq!(saved(&trace), [vec![favorite("/l/a.jpg", "a.jpg")]]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn favorite_without_a_favorites_dir_fails_and_copies_nothing() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    assert!(saved(&trace).is_empty());
    let reply = Reply::Failed("favorites_dir is not set in the config".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn a_failed_copy_fails_with_its_reason() {
    let mut world = favorite_world();
    world.favorite_error = Some("Permission denied (os error 13)".to_owned());

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    let reply =
        Reply::Failed("the favorite was not saved: Permission denied (os error 13)".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn favorite_of_both_styles_copies_each_image_in_one_go() {
    let mut world = favorite_world();

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Both));

    let both = vec![favorite("/l/a.jpg", "a.jpg"), favorite("/d/a.jpg", "a.jpg")];
    assert_eq!(saved(&trace), [both]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn favorite_of_both_styles_showing_one_image_copies_it_once() {
    let shared = ["/s/a.jpg", "/s/b.jpg"];
    let mut world = World::new(init(with_favorites(alphabetical()), &shared, &shared));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Both));

    assert_eq!(saved(&trace), [vec![favorite("/s/a.jpg", "a.jpg")]]);
}

#[test]
fn favorite_of_a_remote_image_copies_its_blob_named_after_the_url() {
    let dawn = "https://user:pw@example.com/walls/dawn.jpg?size=4k";
    let remote = blob(dawn, 1);
    let config = with_favorites(remote_config(&[dawn], &[]));
    let mut world = World::new(init(config, &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(std::slice::from_ref(&remote)));
    assert_eq!(world.current(Style::Light), Some(id(dawn)));

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    let source = remote.blob.to_str().expect("a UTF-8 blob path");
    assert_eq!(saved(&trace), [vec![favorite(source, "dawn.jpg")]]);
}

#[test]
fn favorite_of_a_url_without_a_path_segment_is_named_image() {
    let root = "https://example.com/";
    let remote = blob(root, 1);
    let config = with_favorites(remote_config(&[root], &[]));
    let mut world = World::new(init(config, &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(std::slice::from_ref(&remote)));

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    let source = remote.blob.to_str().expect("a UTF-8 blob path");
    assert_eq!(saved(&trace), [vec![favorite(source, "image")]]);
}

#[test]
fn favorite_of_a_remote_image_that_left_the_config_fails() {
    let dawn = "https://example.com/walls/dawn.jpg";
    let remote = blob(dawn, 1);
    let config = with_favorites(remote_config(&[dawn], &[]));
    let mut world = World::new(init(config.clone(), &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(std::slice::from_ref(&remote)));
    let mut without_dawn = config;
    without_dawn.styles.light.sources.clear();
    world.run(at(5), Event::ConfigChanged(Ok(without_dawn)));
    assert_eq!(world.current(Style::Light), Some(id(dawn)));

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    assert!(saved(&trace).is_empty());
    let reply = Reply::Failed("the light image is no longer available".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn favorite_with_nothing_on_screen_fails() {
    let mut config = with_favorites(alphabetical());
    config.general.empty_style = gpaperd_core::config::EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    assert!(saved(&trace).is_empty());
    let reply = Reply::Failed("no light image is shown".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn a_favorite_still_being_copied_fails_on_shutdown() {
    let mut world = favorite_world();
    let asked = gpaperd_core::step::step(
        &mut world.model,
        at(10),
        crate::support::control(1, Command::Favorite(StyleTarget::Light)),
    );
    assert_eq!(saved(&asked).len(), 1);

    let trace = world.run(at(11), Event::Shutdown);

    let reply = Reply::Failed("gpaperd is stopping".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn favorite_after_restore_fails_and_copies_nothing() {
    let mut world = favorite_world();
    world.ask(at(5), 1, Command::Restore);

    let trace = world.ask(at(10), 2, Command::Favorite(StyleTarget::Light));

    assert!(saved(&trace).is_empty());
    let reply = Reply::Failed(
        "the light background key holds your own wallpaper; run gpaperctl resume".to_owned(),
    );
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn favorite_under_an_external_change_fails_and_copies_nothing() {
    let mut world = favorite_world();
    world.set_key(at(5), Style::Light, "file:///home/u/own.jpg");

    let trace = world.ask(at(10), 1, Command::Favorite(StyleTarget::Light));

    assert!(saved(&trace).is_empty());
    let reply = Reply::Failed(
        "the light background key was changed outside gpaperd; run gpaperctl resume".to_owned(),
    );
    assert_eq!(the_reply(&trace), &reply);
}
