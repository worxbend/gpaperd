//! `on_change`: the command that runs when the image on screen changes.

use std::path::PathBuf;

use gpaperd_core::catalog::FsChange;
use gpaperd_core::config::{ApplyMode, Config, HookCommand};
use gpaperd_core::control::{Command, StyleTarget};
use gpaperd_core::event::{Effect, Event, HookEvent, HookRun};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, at, blob, cache_loaded, changed, control, id, init,
    keys_changed, published, remote_config, running_world, switch_to, two_folders_with, version,
};

const DAWN: &str = "https://example.com/dawn.jpg";

fn hook_command() -> HookCommand {
    HookCommand {
        program: PathBuf::from("/home/u/bin/wallpaper-hook"),
        args: vec!["--quiet".to_owned()],
    }
}

fn with_hook(mut config: Config) -> Config {
    config.general.on_change = Some(hook_command());
    config
}

/// Three light and three dark images in alphabetical order, with the hook
/// set, after a first run at second 0 with light on screen.
fn hooked_world() -> World {
    let mut world = World::new(two_folders_with(with_hook(alphabetical())));
    world.start(at(0), Style::Light);
    world
}

fn run(event: HookEvent, style: Style, path: &str, visible: bool) -> HookRun {
    HookRun {
        command: hook_command(),
        event,
        style,
        item: id(path),
        path: Some(PathBuf::from(path)),
        visible,
    }
}

/// Every `RunHook` effect in `trace`, in order.
fn hook_runs(trace: &[Effect]) -> Vec<&HookRun> {
    trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::RunHook(run) => Some(run),
            _ => None,
        })
        .collect()
}

#[test]
fn a_rotation_of_the_style_on_screen_runs_the_hook_with_its_new_image() {
    let mut world = hooked_world();

    let trace = world.run(at(900), DEADLINE);

    let expected = run(HookEvent::Changed, Style::Light, "/l/b.jpg", true);
    assert_eq!(hook_runs(&trace), [&expected]);
}

#[test]
fn the_hook_runs_after_the_changed_signal() {
    let mut world = hooked_world();

    let trace = world.run(at(900), DEADLINE);

    let signal = trace
        .iter()
        .position(|effect| matches!(effect, Effect::Changed { .. }));
    let hook = trace
        .iter()
        .position(|effect| matches!(effect, Effect::RunHook(_)));
    assert!(signal < hook, "{trace:?}");
}

#[test]
fn without_on_change_no_hook_runs() {
    let mut world = running_world();

    let mut trace = world.run(at(900), DEADLINE);
    trace.extend(world.run(at(910), switch_to(Style::Dark)));

    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_first_run_runs_the_hook_for_each_style_it_fills() {
    let mut world = World::new(two_folders_with(with_hook(alphabetical())));

    let trace = world.start(at(0), Style::Light);

    let light = run(HookEvent::Changed, Style::Light, "/l/a.jpg", true);
    let dark = run(HookEvent::Changed, Style::Dark, "/d/a.jpg", false);
    assert_eq!(hook_runs(&trace), [&light, &dark]);
}

#[test]
fn a_change_of_the_hidden_style_runs_the_hook_as_not_visible() {
    let mut world = hooked_world();

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Dark));

    let expected = run(HookEvent::Changed, Style::Dark, "/d/b.jpg", false);
    assert_eq!(hook_runs(&trace), [&expected]);
}

#[test]
fn a_style_switch_runs_the_hook_with_the_image_of_the_style_now_on_screen() {
    let mut world = hooked_world();

    let trace = world.run(at(10), switch_to(Style::Dark));

    let expected = run(HookEvent::Switched, Style::Dark, "/d/a.jpg", true);
    assert_eq!(hook_runs(&trace), [&expected]);
}

#[test]
fn a_switch_to_the_style_already_on_screen_runs_no_hook() {
    let mut world = hooked_world();

    let trace = world.run(at(10), switch_to(Style::Light));

    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn set_previous_and_ban_each_run_the_hook() {
    let mut world = hooked_world();

    let set = world.ask(at(10), 1, Command::Set(StyleTarget::Light, id("/l/c.jpg")));
    let previous = world.ask(at(20), 2, Command::Previous(StyleTarget::Light));
    let ban = world.ask(at(30), 3, Command::Ban(StyleTarget::Light));

    let shown =
        |trace: &[Effect]| -> Vec<HookRun> { hook_runs(trace).into_iter().cloned().collect() };
    assert_eq!(
        shown(&set),
        [run(HookEvent::Changed, Style::Light, "/l/c.jpg", true)]
    );
    assert_eq!(
        shown(&previous),
        [run(HookEvent::Changed, Style::Light, "/l/a.jpg", true)]
    );
    assert_eq!(
        shown(&ban),
        [run(HookEvent::Changed, Style::Light, "/l/b.jpg", true)]
    );
}

#[test]
fn a_set_of_the_image_on_screen_runs_no_hook() {
    let mut world = hooked_world();

    let trace = world.ask(at(10), 1, Command::Set(StyleTarget::Light, id("/l/a.jpg")));

    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_new_version_of_the_image_on_screen_runs_no_hook() {
    let mut world = hooked_world();

    let trace = world.run(
        at(10),
        changed(Style::Light, vec![FsChange::Found(version("/l/a.jpg", 1))]),
    );

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_restart_that_finds_its_slots_intact_runs_no_hook() {
    let mut world = hooked_world();
    world.run(at(900), DEADLINE);
    let mut restarted = world.restart(two_folders_with(with_hook(alphabetical())));

    let trace = restarted.start(at(1000), Style::Light);

    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_restart_that_refills_a_lost_slot_with_the_same_image_runs_no_hook() {
    let world = hooked_world();
    let mut restarted = world.restart(two_folders_with(with_hook(alphabetical())));
    restarted.slots = PerStyle::default();

    let trace = restarted.start(at(10), Style::Light);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/a.jpg"), (Style::Dark, "/d/a.jpg")]
    );
    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_restart_whose_image_is_gone_runs_the_hook_for_the_one_that_replaces_it() {
    let world = hooked_world();
    let mut restarted = world.restart(init(
        with_hook(alphabetical()),
        &["/l/b.jpg", "/l/c.jpg"],
        &["/d/a.jpg", "/d/b.jpg", "/d/c.jpg"],
    ));
    restarted.slots = PerStyle::default();

    let trace = restarted.start(at(10), Style::Light);

    let expected = run(HookEvent::Changed, Style::Light, "/l/b.jpg", true);
    assert_eq!(hook_runs(&trace), [&expected]);
}

#[test]
fn a_reload_that_sets_on_change_runs_no_hook_for_the_images_already_shown() {
    let mut world = running_world();

    let trace = world.run(at(10), Event::ConfigChanged(Ok(with_hook(alphabetical()))));

    assert!(hook_runs(&trace).is_empty());
}

#[test]
fn a_remote_image_gives_the_hook_its_cached_blob() {
    let dawn = blob(DAWN, 1);
    let mut world = World::new(init(with_hook(remote_config(&[DAWN], &[])), &[], &[]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(1), cache_loaded(std::slice::from_ref(&dawn)));

    let light = hook_runs(&trace)
        .into_iter()
        .find(|run| run.style == Style::Light)
        .expect("a hook run for light");
    assert_eq!(light.item, id(DAWN));
    assert_eq!(light.path.as_ref(), Some(&dawn.blob));
}

#[test]
fn in_uri_mode_the_hook_runs_once_the_shell_reports_the_new_uri() {
    let mut config = with_hook(alphabetical());
    config.general.apply_mode = ApplyMode::Uri;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    step(
        &mut world.model,
        at(10),
        control(1, Command::Next(StyleTarget::Light)),
    );

    let planned = step(
        &mut world.model,
        at(11),
        Event::Probed {
            style: Style::Light,
            item: id("/l/b.jpg"),
            verdict: Ok(()),
        },
    );
    world.keys.picture_uri = "file:///l/b.jpg".to_owned();
    let landed = step(&mut world.model, at(12), keys_changed(world.keys.clone()));

    assert!(hook_runs(&planned).is_empty());
    let expected = run(HookEvent::Changed, Style::Light, "/l/b.jpg", true);
    assert_eq!(hook_runs(&landed), [&expected]);
}
