//! `gpaperctl ban` and `unban`: take the image on screen out of rotation
//! for good, and put a banned image back.

use std::collections::BTreeSet;

use gpaperd_core::catalog::ItemId;
use gpaperd_core::config::{Config, EmptyStylePolicy};
use gpaperd_core::control::{Command, Reply, StyleTarget};
use gpaperd_core::model::ModelInit;
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, foreign_slot, id, init, light_showing, published,
    running_world, six_shuffled, the_reply,
};

fn ban(target: StyleTarget) -> Command {
    Command::Ban(target)
}

fn banned(world: &World) -> BTreeSet<ItemId> {
    world.model.persisted().banned
}

fn ids(paths: &[&str]) -> BTreeSet<ItemId> {
    paths.iter().map(|path| id(path)).collect()
}

/// Light shows `/l/a.jpg` of `light`, dark one of `/d/a.jpg` and `/d/b.jpg`.
fn world_with_light(config: Config, light: &[&str]) -> World {
    let mut world = World::new(init(config, light, &["/d/a.jpg", "/d/b.jpg"]));
    world.start(at(0), Style::Light);
    world
}

#[test]
fn ban_takes_the_image_on_screen_out_and_shows_the_next() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, ban(StyleTarget::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert_eq!(banned(&world), ids(&["/l/a.jpg"]));
}

#[test]
fn a_banned_image_is_never_drawn_again_by_next() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let to_c = world.ask(at(20), 2, Command::Next(StyleTarget::Light));
    let past_a = world.ask(at(30), 3, Command::Next(StyleTarget::Light));

    assert_eq!(published(&to_c), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(published(&past_a), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_banned_image_is_never_drawn_by_a_rotation() {
    let mut world = World::new(six_shuffled());
    world.start(at(0), Style::Light);
    let first = world.current(Style::Light).expect("light shows an image");
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let mut shown = Vec::new();
    for tick in 1..=12 {
        let trace = world.run(at(10 + 900 * tick), DEADLINE);
        shown.extend(published(&trace).into_iter().map(|(_, item)| id(item)));
    }

    assert_eq!(shown.len(), 12, "every tick rotated");
    assert!(!shown.contains(&first), "{first} came back: {shown:?}");
}

#[test]
fn previous_skips_a_banned_image() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let trace = world.ask(at(20), 2, Command::Previous(StyleTarget::Light));

    assert!(published(&trace).is_empty());
    let reply = Reply::Failed("no earlier light image to show".to_owned());
    assert_eq!(the_reply(&trace), &reply);
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
}

#[test]
fn set_of_a_banned_image_fails() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let trace = world.ask(at(20), 2, Command::Set(StyleTarget::Light, id("/l/a.jpg")));

    assert!(published(&trace).is_empty());
    let reply = Reply::Failed("the light image is banned".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn a_ban_holds_for_both_styles() {
    let shared = ["/s/a.jpg", "/s/b.jpg", "/s/c.jpg"];
    let mut world = World::new(init(alphabetical(), &shared, &shared));
    world.start(at(0), Style::Light);
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let trace = world.ask(at(20), 2, Command::Set(StyleTarget::Dark, id("/s/a.jpg")));

    let reply = Reply::Failed("the dark image is banned".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn ban_of_both_styles_bans_each_image_and_rotates_each_style() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, ban(StyleTarget::Both));

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Dark, "/d/b.jpg")]
    );
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert_eq!(banned(&world), ids(&["/d/a.jpg", "/l/a.jpg"]));
}

#[test]
fn ban_after_a_partial_next_rotates_the_style_that_moved() {
    let mut world = running_world();
    world.broken.insert(id("/d/b.jpg"));
    world.broken.insert(id("/d/c.jpg"));
    let partial = world.ask(at(10), 1, Command::Next(StyleTarget::Both));
    assert!(matches!(the_reply(&partial), Reply::Failed(_)));
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));

    let trace = world.ask(at(20), 2, ban(StyleTarget::Both));

    assert!(
        published(&trace).contains(&(Style::Light, "/l/c.jpg")),
        "{trace:?}"
    );
}

#[test]
fn ban_of_the_last_image_shows_the_other_styles_image_under_use_other_style() {
    let mut world = world_with_light(alphabetical(), &["/l/a.jpg"]);

    let trace = world.ask(at(10), 1, ban(StyleTarget::Light));

    assert_eq!(published(&trace), [(Style::Light, "/d/b.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn ban_of_the_last_image_keeps_it_on_screen_under_keep_current() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = world_with_light(config, &["/l/a.jpg"]);

    let trace = world.ask(at(10), 1, ban(StyleTarget::Light));

    assert!(published(&trace).is_empty());
    let reply = Reply::Failed("no other light image to show".to_owned());
    assert_eq!(the_reply(&trace), &reply);
    assert_eq!(banned(&world), ids(&["/l/a.jpg"]));
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
    let ticks_to_come = world.run(at(2000), DEADLINE);
    assert!(published(&ticks_to_come).is_empty());
}

#[test]
fn ban_with_nothing_on_screen_fails_and_bans_nothing() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = world_with_light(config, &[]);

    let trace = world.ask(at(10), 1, ban(StyleTarget::Light));

    let reply = Reply::Failed("no light image is shown".to_owned());
    assert_eq!(the_reply(&trace), &reply);
    assert!(banned(&world).is_empty());
}

#[test]
fn ban_under_an_external_change_fails_and_bans_nothing() {
    let mut world = running_world();
    world.set_key(at(5), Style::Light, "file:///home/u/own.jpg");

    let trace = world.ask(at(10), 1, ban(StyleTarget::Light));

    assert!(matches!(the_reply(&trace), Reply::Failed(_)));
    assert!(banned(&world).is_empty());
}

#[test]
fn a_restored_round_skips_a_banned_image() {
    let config = Config::default();
    let light = ["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"];
    let mut restored = light_showing("/l/a.jpg", &["/l/b.jpg", "/l/c.jpg"], &["/l/a.jpg"]);
    restored.banned = ids(&["/l/b.jpg"]);
    let mut world = World::new(ModelInit {
        restored: Some(restored),
        ..init(config, &light, &["/d/a.jpg"])
    });
    world.slots.light = foreign_slot();
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_banned_image_from_the_state_file_is_not_put_back_at_start() {
    let config = alphabetical();
    let light = ["/l/a.jpg", "/l/b.jpg"];
    let mut restored = light_showing("/l/a.jpg", &[], &[]);
    restored.banned = ids(&["/l/a.jpg"]);
    let world = World::new(ModelInit {
        restored: Some(restored),
        ..init(config, &light, &["/d/a.jpg"])
    });
    let mut world = world;
    // The slot is gone, so the first apply refills it.

    let trace = world.start(at(0), Style::Light);

    assert!(published(&trace).contains(&(Style::Light, "/l/b.jpg")));
    assert!(!published(&trace).contains(&(Style::Light, "/l/a.jpg")));
}

#[test]
fn bans_survive_a_restart() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let restarted = world.restart(init(
        alphabetical(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg"],
    ));

    assert_eq!(banned(&restarted), ids(&["/l/a.jpg"]));
}

#[test]
fn status_lists_the_banned_images() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Both));

    let trace = world.ask(at(20), 2, Command::Status);

    let Reply::Status(report) = the_reply(&trace) else {
        panic!("expected a status");
    };
    assert_eq!(report.banned, [id("/d/a.jpg"), id("/l/a.jpg")]);
    assert_eq!(report.styles.light.usable, 2);
}

#[test]
fn unban_puts_the_image_back_into_rotation() {
    let mut world = running_world();
    world.ask(at(10), 1, ban(StyleTarget::Light));

    let trace = world.ask(at(20), 2, Command::Unban(id("/l/a.jpg")));
    world.ask(at(30), 3, Command::Next(StyleTarget::Light));
    let back_to_a = world.ask(at(40), 4, Command::Next(StyleTarget::Light));

    assert_eq!(the_reply(&trace), &Reply::Done);
    assert!(banned(&world).is_empty());
    assert_eq!(published(&back_to_a), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn unban_of_the_only_image_lets_a_keep_current_style_rotate_again() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = world_with_light(config, &["/l/a.jpg", "/l/b.jpg"]);
    world.ask(at(10), 1, ban(StyleTarget::Light));
    world.ask(at(20), 2, ban(StyleTarget::Light));
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));

    let trace = world.ask(at(30), 3, Command::Unban(id("/l/a.jpg")));

    assert_eq!(armed(&trace).last(), Some(&Some(at(910))));
}

#[test]
fn unban_of_an_image_that_is_not_banned_is_rejected() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, Command::Unban(id("/l/b.jpg")));

    let reply = Reply::Rejected("the image is not banned".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}
