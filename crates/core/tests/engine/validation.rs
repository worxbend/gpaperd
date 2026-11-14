//! Probe verdicts: what is remembered, and the probes a truncated file gets.

use gpaperd_core::catalog::{BadReason, FsChange};
use gpaperd_core::config::EmptyStylePolicy;
use gpaperd_core::event::{Effect, Event};
use gpaperd_core::step::{MAX_PROBE_RETRIES, step};
use gpaperd_core::style::Style;

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, changed, count, id, init, is_publish,
    is_ready, keys_pointed, probed, published, removed, running_world, scanned, two_folders_with,
    version,
};

/// A daemon over two light images with `/l/a.jpg` on screen, whose other
/// image, `/l/b.jpg`, is still being copied in.
fn second_image_incomplete() -> World {
    let mut world = World::new(init(
        alphabetical(),
        &["/l/a.jpg", "/l/b.jpg"],
        &["/d/only.jpg"],
    ));
    world.start(at(0), Style::Light);
    world.rejected.insert(id("/l/b.jpg"), BadReason::Truncated);
    world
}

/// Sends a deadline at every interval from `first_tick` on, and one ten
/// seconds after each, for `ticks` intervals. Returns how often `path` was
/// probed.
fn probes_over_time(world: &mut World, path: &str, first_tick: u64, ticks: u64) -> usize {
    let mut probes = 0;
    for tick in first_tick..first_tick + ticks {
        for secs in [900 * tick, 900 * tick + 10] {
            let trace = world.run(at(secs), DEADLINE);
            probes += probed(&trace)
                .iter()
                .filter(|probe| probe.1 == path)
                .count();
        }
    }
    probes
}

#[test]
fn a_probe_carries_the_configured_max_pixels() {
    let mut config = alphabetical();
    config.general.max_pixels = 2_000_000;

    let trace = World::new(two_folders_with(config)).start(at(0), Style::Light);

    let limits: Vec<u64> = trace
        .iter()
        .filter_map(|effect| match effect {
            Effect::Probe { max_pixels, .. } => Some(*max_pixels),
            _ => None,
        })
        .collect();
    assert_eq!(limits, [2_000_000, 2_000_000]);
}

#[test]
fn an_image_above_max_pixels_is_skipped_from_then_on() {
    let mut world = running_world();
    let too_large = BadReason::TooLarge {
        width: 20_000,
        height: 10_000,
    };
    world.rejected.insert(id("/l/b.jpg"), too_large);

    let first_lap = world.run(at(900), DEADLINE);
    world.run(at(1800), DEADLINE);
    let second_lap = world.run(at(2700), DEADLINE);

    assert_eq!(published(&first_lap), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(probed(&second_lap), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_rescan_keeps_the_verdicts_of_unchanged_files() {
    let mut world = running_world();
    world.broken.insert(id("/l/b.jpg"));
    world.run(at(900), DEADLINE);

    world.run(
        at(1000),
        scanned(Style::Light, &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"]),
    );
    world.run(at(1800), DEADLINE);
    let trace = world.run(at(2700), DEADLINE);

    assert_eq!(probed(&trace), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_truncated_image_may_be_probed_again_ten_seconds_later() {
    let mut world = second_image_incomplete();

    let at_deadline = world.run(at(900), DEADLINE);
    world.rejected.clear();
    let at_retry = world.run(at(910), DEADLINE);
    let at_next_deadline = world.run(at(1800), DEADLINE);

    assert_eq!(count(&at_deadline, is_publish), 0);
    assert_eq!(armed(&at_deadline), [Some(at(910))]);
    // The image is a candidate again, so there is something to rotate to.
    assert_eq!(armed(&at_retry), [Some(at(1800))]);
    assert_eq!(published(&at_next_deadline), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn a_truncated_image_gets_three_more_probes_and_no_fourth() {
    let mut world = second_image_incomplete();

    let probes = probes_over_time(&mut world, "/l/b.jpg", 1, 8);

    assert_eq!(probes, 1 + MAX_PROBE_RETRIES as usize);
}

#[test]
fn a_truncated_image_that_changes_gets_all_its_probes_again() {
    let mut world = second_image_incomplete();
    probes_over_time(&mut world, "/l/b.jpg", 1, 8);
    let grown = FsChange::Found(version("/l/b.jpg", 1));

    // The image on screen is long overdue, so the first probe is at once.
    let at_change = world.run(at(9000), changed(Style::Light, vec![grown]));
    let later_probes = probes_over_time(&mut world, "/l/b.jpg", 11, 8);

    assert_eq!(probed(&at_change), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(later_probes, MAX_PROBE_RETRIES as usize);
}

#[test]
fn a_slot_is_filled_once_its_only_image_is_complete() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/only.jpg"], &[]));
    world
        .rejected
        .insert(id("/l/only.jpg"), BadReason::Truncated);

    let at_start = world.start(at(0), Style::Light);
    world.rejected.clear();
    let at_retry = world.run(at(10), DEADLINE);

    // Readiness does not wait for the copy to end.
    assert_eq!(count(&at_start, is_ready), 1);
    assert_eq!(armed(&at_start), [Some(at(10))]);
    assert_eq!(published(&at_retry), [(Style::Light, "/l/only.jpg")]);
    assert_eq!(keys_pointed(&at_retry), [[Style::Light]]);
}

#[test]
fn the_image_on_screen_is_published_again_once_its_edit_is_complete() {
    let mut world = running_world();
    world.rejected.insert(id("/l/a.jpg"), BadReason::Truncated);
    let half_saved = FsChange::Found(version("/l/a.jpg", 1));

    let at_edit = world.run(at(100), changed(Style::Light, vec![half_saved]));
    world.rejected.clear();
    let at_retry = world.run(at(110), DEADLINE);

    // No other image takes its place while it is being saved.
    assert_eq!(probed(&at_edit), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(count(&at_edit, is_publish), 0);
    assert_eq!(published(&at_retry), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn a_second_truncated_verdict_during_the_wait_is_not_another_probe_spent() {
    let shared = ["/s/a.jpg", "/s/b.jpg"];
    let mut config = alphabetical();
    config.general.rotate_hidden = true;
    let mut world = World::new(init(config, &shared, &shared));
    world.start(at(0), Style::Light);
    world.rejected.insert(id("/s/b.jpg"), BadReason::Truncated);

    // Both styles probe the same file at the same time.
    world.run(at(900), DEADLINE);
    let later_probes = probes_over_time(&mut world, "/s/b.jpg", 2, 8);

    // Each style probes it once per retry: three retries, not one and a half.
    assert_eq!(later_probes, 2 * MAX_PROBE_RETRIES as usize);
}

#[test]
fn a_passed_probe_ends_the_wait_of_a_truncated_image_in_both_styles() {
    let shared = ["/s/a.jpg", "/s/b.jpg"];
    let mut world = World::new(init(alphabetical(), &shared, &shared));
    world.start(at(0), Style::Light);
    let b = id("/s/b.jpg");
    let verdict = |style, verdict| Event::Probed {
        style,
        item: b.clone(),
        verdict,
    };
    // Light probes b at its deadline, and dark, whose slot was lost, too.
    step(&mut world.model, at(900), DEADLINE);
    step(
        &mut world.model,
        at(900),
        changed(Style::Dark, vec![removed("/s/a.jpg")]),
    );
    step(&mut world.model, at(900), Event::SlotLost(Style::Dark));

    step(
        &mut world.model,
        at(901),
        verdict(Style::Light, Err(BadReason::Truncated)),
    );
    let after_pass = step(&mut world.model, at(902), verdict(Style::Dark, Ok(())));
    let at_next_deadline = world.run(at(1801), DEADLINE);

    // The file is complete after all: light shows it at its next tick.
    assert_eq!(published(&after_pass), [(Style::Dark, "/s/b.jpg")]);
    assert_eq!(published(&at_next_deadline), [(Style::Light, "/s/b.jpg")]);
}

#[test]
fn the_status_names_the_next_change_and_not_the_next_probe() {
    let mut world = running_world();
    world.rejected.insert(id("/l/b.jpg"), BadReason::Truncated);

    let trace = world.run(at(900), DEADLINE);

    let Some(Effect::Report(status)) = trace.last() else {
        panic!("no status after a rotation: {trace:?}");
    };
    assert_eq!(published(&trace), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(armed(&trace), [Some(at(910))]);
    assert_eq!(status.next_change, Some(at(1800)));
}

#[test]
fn a_clock_that_went_back_does_not_put_off_the_next_probe() {
    let mut world = second_image_incomplete();
    world.run(at(900), DEADLINE);

    let trace = world.run(at(400), CLOCK_CHANGED);

    assert_eq!(armed(&trace), [Some(at(410))]);
}
