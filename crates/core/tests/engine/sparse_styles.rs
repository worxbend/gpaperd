//! Styles with no image or a single one.

use gpaperd_core::config::{Config, EmptyStylePolicy, PictureOptions};
use gpaperd_core::event::{Effect, Status, Tile};
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, count, foreign_slot, init, is_ready, is_set_keys,
    keys_written, probed, published, slot_uris,
};

#[test]
fn an_empty_style_shows_the_other_styles_images() {
    let mut world = World::new(init(alphabetical(), &["/l/a.jpg", "/l/b.jpg"], &[]));

    let at_start = world.start(at(0), Style::Dark);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(
        published(&at_start),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/l/a.jpg")]
    );
    assert_eq!(published(&at_deadline), [(Style::Dark, "/l/b.jpg")]);
}

#[test]
fn keep_current_leaves_an_empty_styles_slot_alone() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.slots.dark = foreign_slot();

    let trace = world.start(at(0), Style::Dark);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(count(&trace, is_set_keys), 1);
    // The visible style has nothing to rotate through.
    assert!(armed(&trace).is_empty());
}

#[test]
fn a_single_image_is_published_once_and_never_rotates() {
    let single = init(alphabetical(), &["/l/only.jpg"], &["/d/only.jpg"]);
    let mut world = World::new(single);

    let at_start = world.start(at(0), Style::Light);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(
        published(&at_start),
        [(Style::Light, "/l/only.jpg"), (Style::Dark, "/d/only.jpg")]
    );
    assert!(armed(&at_start).is_empty());
    assert!(at_deadline.is_empty());
}

#[test]
fn a_single_shuffled_image_never_rotates_either() {
    let single = init(Config::default(), &["/l/only.jpg"], &["/d/only.jpg"]);
    let mut world = World::new(single);

    let at_start = world.start(at(0), Style::Light);
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(published(&at_start).len(), 2);
    assert!(armed(&at_start).is_empty());
    assert!(at_deadline.is_empty());
}

#[test]
fn two_empty_styles_keep_their_slots_and_idle() {
    let mut world = World::new(init(Config::default(), &[], &[]));
    world.slots = PerStyle {
        light: foreign_slot(),
        dark: foreign_slot(),
    };

    let trace = world.start(at(0), Style::Light);

    let uris = slot_uris();
    let both = PerStyle {
        light: Some(uris.light.as_str()),
        dark: Some(uris.dark.as_str()),
    };
    assert!(probed(&trace).is_empty());
    assert!(published(&trace).is_empty());
    assert!(armed(&trace).is_empty());
    assert_eq!(keys_written(&trace), [(both, None)]);
    assert_eq!(count(&trace, is_ready), 1);
}

#[test]
fn two_empty_styles_on_a_first_run_leave_the_users_wallpaper_alone() {
    let mut world = World::new(init(Config::default(), &[], &[]));

    let trace = world.start(at(0), Style::Light);

    let idle = Status {
        visible: Style::Light,
        current: PerStyle::default(),
        next_change: None,
        paused: PerStyle::default(),
    };
    let watch = |style| Effect::Watch {
        style,
        sources: Vec::new(),
    };
    // The Settings tile changes no key.
    let nothing_but_the_watches = [
        watch(Style::Light),
        watch(Style::Dark),
        Effect::Tile(Tile::Offer(PictureOptions::Zoom)),
        Effect::Ready,
        Effect::Report(idle),
    ];
    assert_eq!(trace, nothing_but_the_watches);
}
