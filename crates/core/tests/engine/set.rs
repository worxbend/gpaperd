//! `gpaperctl set`: show one named image of a style's catalog now.

use gpaperd_core::catalog::{ItemId, RemoteBlob};
use gpaperd_core::config::Config;
use gpaperd_core::control::{Command, Reply, StyleTarget};
use gpaperd_core::event::{Effect, Event};
use gpaperd_core::step::step;
use gpaperd_core::style::Style;

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, blob, cache_loaded, control, id, init, probed,
    published, remote_config, running_world, scanned, the_reply, unscanned,
};

const DAWN: &str = "https://example.com/dawn.jpg";

fn set(target: StyleTarget, item: &str) -> Command {
    Command::Set(target, id(item))
}

fn history(world: &World, style: Style) -> Vec<ItemId> {
    world.model.persisted().styles[style]
        .history
        .iter()
        .cloned()
        .collect()
}

fn not_in_sources(style: Style) -> Reply {
    Reply::Rejected(format!("the image is not in the sources of {style}"))
}

#[test]
fn set_shows_the_named_image_and_replies_after_the_publish() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/c.jpg"));

    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
    let announced = Effect::Changed {
        style: Style::Light,
        item: id("/l/c.jpg"),
    };
    assert!(trace.contains(&announced));
}

#[test]
fn set_for_visible_means_the_style_on_screen() {
    let mut world = World::new(init(
        alphabetical(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/a.jpg", "/d/b.jpg"],
    ));
    world.start(at(0), Style::Dark);

    let trace = world.ask(at(10), 1, set(StyleTarget::Visible, "/d/b.jpg"));

    assert_eq!(published(&trace), [(Style::Dark, "/d/b.jpg")]);
}

#[test]
fn set_restarts_the_interval() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/c.jpg"));

    assert_eq!(armed(&trace).last(), Some(&Some(at(910))));
}

#[test]
fn set_records_the_replaced_image_so_that_previous_returns_to_it() {
    let mut world = running_world();
    world.ask(at(10), 1, set(StyleTarget::Light, "/l/c.jpg"));
    assert_eq!(history(&world, Style::Light), [id("/l/a.jpg")]);

    let trace = world.ask(at(20), 2, Command::Previous(StyleTarget::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn set_keeps_a_user_pause() {
    let mut world = running_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));

    let trace = world.ask(at(10), 2, set(StyleTarget::Light, "/l/c.jpg"));

    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert!(world.model.persisted().styles.light.user_paused);
}

#[test]
fn set_for_both_styles_shows_the_image_in_each() {
    let shared = ["/shared/a.jpg", "/shared/b.jpg"];
    let mut world = World::new(init(alphabetical(), &shared, &shared));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, set(StyleTarget::Both, "/shared/b.jpg"));

    assert_eq!(
        published(&trace),
        [
            (Style::Light, "/shared/b.jpg"),
            (Style::Dark, "/shared/b.jpg")
        ]
    );
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn set_of_an_image_outside_the_styles_sources_is_rejected() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/d/b.jpg"));

    assert!(probed(&trace).is_empty());
    assert_eq!(the_reply(&trace), &not_in_sources(Style::Light));
}

#[test]
fn set_for_both_styles_changes_nothing_when_one_style_lacks_the_image() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, set(StyleTarget::Both, "/l/c.jpg"));

    assert!(probed(&trace).is_empty());
    assert_eq!(the_reply(&trace), &not_in_sources(Style::Dark));
}

#[test]
fn set_of_an_image_that_fails_its_probe_fails_and_keeps_the_slot() {
    let mut world = running_world();
    world.broken.insert(id("/l/c.jpg"));

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/c.jpg"));

    assert_eq!(probed(&trace), [(Style::Light, "/l/c.jpg")]);
    assert!(published(&trace).is_empty());
    assert!(matches!(the_reply(&trace), Reply::Failed(_)));
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn set_whose_publish_fails_while_the_style_is_overdue_shows_nothing_else() {
    let mut world = running_world();
    world.unwritable.insert(id("/l/c.jpg"));
    step(
        &mut world.model,
        at(899),
        control(1, set(StyleTarget::Light, "/l/c.jpg")),
    );
    // Light falls due while its Set is in flight: the tick waits.
    step(&mut world.model, at(900), DEADLINE);

    let probed_fine = Event::Probed {
        style: Style::Light,
        item: id("/l/c.jpg"),
        verdict: Ok(()),
    };
    let trace = world.run(at(901), probed_fine);

    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
    assert!(probed(&trace).is_empty());
    assert!(matches!(the_reply(&trace), Reply::Failed(_)));
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
    assert_eq!(armed(&trace).last(), Some(&Some(at(1801))));
}

#[test]
fn set_of_an_image_known_to_be_bad_fails_without_a_probe() {
    let mut world = running_world();
    world.broken.insert(id("/l/c.jpg"));
    world.ask(at(10), 1, set(StyleTarget::Light, "/l/c.jpg"));

    let trace = world.ask(at(20), 2, set(StyleTarget::Light, "/l/c.jpg"));

    assert!(probed(&trace).is_empty());
    let reply =
        Reply::Failed("the light image cannot be shown: content is not an image".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn set_of_the_image_on_screen_restarts_the_interval_without_a_publish() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/a.jpg"));

    assert!(published(&trace).is_empty());
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert_eq!(armed(&trace).last(), Some(&Some(at(910))));
    assert!(history(&world, Style::Light).is_empty());
}

#[test]
fn set_before_the_styles_scan_fails() {
    let mut world = World::new(unscanned(alphabetical()));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/a.jpg"));

    assert!(probed(&trace).is_empty());
    let reply = Reply::Failed("the light sources are still being scanned".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn set_after_the_scan_finds_the_image() {
    let mut world = World::new(unscanned(alphabetical()));
    world.start(at(0), Style::Light);
    world.run(at(1), scanned(Style::Light, &["/l/a.jpg", "/l/b.jpg"]));

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/b.jpg"));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn set_of_a_cached_remote_image_publishes_its_blob() {
    let remote: RemoteBlob = blob(DAWN, 1);
    let mut world = World::new(init(
        remote_config(&[DAWN], &[]),
        &["/l/a.jpg"],
        &["/d/a.jpg"],
    ));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(std::slice::from_ref(&remote)));

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, DAWN));

    assert_eq!(published(&trace), [(Style::Light, DAWN)]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn set_of_a_listed_url_without_a_blob_fails() {
    let mut world = World::new(init(
        remote_config(&[DAWN], &[]),
        &["/l/a.jpg"],
        &["/d/a.jpg"],
    ));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(&[]));

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, DAWN));

    assert!(probed(&trace).is_empty());
    let reply = Reply::Failed("the light image is not downloaded yet".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}

#[test]
fn set_without_a_config_fails() {
    let mut world = World::new(gpaperd_core::model::ModelInit {
        config: None,
        ..init(Config::default(), &[], &[])
    });
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, set(StyleTarget::Light, "/l/a.jpg"));

    let reply = Reply::Failed("no config file is loaded".to_owned());
    assert_eq!(the_reply(&trace), &reply);
}
