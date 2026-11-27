//! Remote images: the cache at start, fetches and their outcomes, and what
//! the catalogs show of them.

use std::time::Duration;

use gpaperd_core::catalog::{BadReason, RemoteBlob};
use gpaperd_core::config::{ApplyMode, Config, RemoteRefresh, Source};
use gpaperd_core::event::{Connectivity, Effect, Event, FetchOutcome, GnomeEvent};
use gpaperd_core::step::{GC_PERIOD, MAX_FETCHES_IN_FLIGHT};
use gpaperd_core::style::Style;

use crate::support::{
    CLOCK_CHANGED, DEADLINE, World, alphabetical, armed, at, blob, cache_loaded, collected, count,
    dropped, fetched, fetches, id, init, is_ready, probed, probed_paths, published, remote_config,
    scanned, stored, unscanned,
};

const DAWN: &str = "https://img.test/dawn.jpg";
const DUSK: &str = "https://img.test/dusk.jpg";
const NOON: &str = "https://img.test/noon.jpg";

fn connectivity(level: Connectivity) -> Event {
    Event::Gnome(GnomeEvent::ConnectivityChanged(level))
}

fn failed(secs: u64) -> FetchOutcome {
    FetchOutcome::Failed {
        retry_in: Duration::from_secs(secs),
    }
}

fn unreachable(secs: u64) -> FetchOutcome {
    FetchOutcome::Unreachable {
        retry_in: Duration::from_secs(secs),
    }
}

/// A daemon whose light style lists `light` URLs, whose dark style has two
/// local images, and whose cache held `cached` at start. Light is on screen.
fn remote_world(config: Config, cached: &[RemoteBlob]) -> World {
    let mut world = World::new(init(config, &[], &["/d/a.jpg", "/d/b.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(cached));
    world
}

#[test]
fn a_cached_image_is_shown_as_soon_as_the_cache_is_loaded() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(0), cache_loaded(&[blob(DAWN, 1)]));

    assert_eq!(published(&trace), [(Style::Light, DAWN)]);
    assert_eq!(probed_paths(&trace), [blob(DAWN, 1).blob]);
}

#[test]
fn ready_waits_for_the_cached_images_when_the_config_lists_urls() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &["/d/a.jpg"]));

    let at_start = world.start(at(0), Style::Light);
    let at_load = world.run(at(0), cache_loaded(&[blob(DAWN, 1)]));

    assert!(!at_start.iter().any(is_ready), "{at_start:?}");
    assert_eq!(count(&at_load, is_ready), 1);
    // The cached image was applied before READY.
    let ready_at = at_load.iter().position(is_ready).expect("READY");
    let published_at = at_load
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::Publish {
                    style: Style::Light,
                    ..
                }
            )
        })
        .expect("light published");
    assert!(published_at < ready_at, "{at_load:?}");
}

#[test]
fn a_remote_only_style_keeps_its_slot_until_its_first_blob_lands() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &["/d/a.jpg"]));

    let at_start = world.start(at(0), Style::Light);
    let at_load = world.run(at(0), cache_loaded(&[]));
    let at_first_blob = world.run(at(5), stored(&blob(DAWN, 1)));

    // It does not borrow the dark image meanwhile.
    assert_eq!(published(&at_start), [(Style::Dark, "/d/a.jpg")]);
    assert!(published(&at_load).is_empty());
    assert_eq!(published(&at_first_blob), [(Style::Light, DAWN)]);
}

#[test]
fn a_remote_only_style_whose_urls_are_gone_borrows_the_other_styles_images() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(&[]));

    let trace = world.run(at(5), fetched(DAWN, FetchOutcome::Gone));

    assert_eq!(published(&trace), [(Style::Light, "/d/a.jpg")]);
}

#[test]
fn a_remote_only_style_whose_uncached_urls_are_refused_borrows_the_other_styles_images() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    world.run(at(0), cache_loaded(&[]));

    // A 401, 403 or rejected body keeps the missing blob missing.
    let trace = world.run(at(5), fetched(DAWN, FetchOutcome::NotModified));

    assert_eq!(published(&trace), [(Style::Light, "/d/a.jpg")]);
}

#[test]
fn every_listed_url_is_fetched_at_start_once_even_when_both_styles_list_it() {
    let mut world = World::new(init(remote_config(&[DAWN, NOON], &[DAWN]), &[], &[]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(0), cache_loaded(&[blob(DAWN, 1)]));

    assert_eq!(fetches(&trace), [DAWN, NOON]);
}

#[test]
fn nothing_is_fetched_before_the_cache_is_loaded() {
    let mut world = World::new(init(remote_config(&[DAWN], &[]), &[], &[]));

    let trace = world.start(at(0), Style::Light);

    assert!(fetches(&trace).is_empty());
    assert!(collected(&trace).is_empty());
}

#[test]
fn with_remote_refresh_never_only_what_is_missing_is_fetched() {
    let mut config = remote_config(&[DAWN, NOON], &[]);
    config.general.remote_refresh = RemoteRefresh::Never;
    let mut world = World::new(init(config, &[], &[]));
    world.start(at(0), Style::Light);

    let trace = world.run(at(0), cache_loaded(&[blob(DAWN, 1)]));

    assert_eq!(fetches(&trace), [NOON]);
}

#[test]
fn with_a_refresh_period_each_url_is_fetched_again_a_period_after_its_answer() {
    let mut config = remote_config(&[DAWN], &[]);
    config.general.remote_refresh = RemoteRefresh::Every(Duration::from_secs(3600));
    let mut world = remote_world(config, &[blob(DAWN, 1)]);
    world.run(at(10), fetched(DAWN, FetchOutcome::NotModified));

    let early = world.run(at(3609), DEADLINE);
    let due = world.run(at(3610), DEADLINE);

    assert!(fetches(&early).is_empty());
    assert_eq!(fetches(&due), [DAWN]);
}

#[test]
fn with_remote_refresh_startup_an_answered_url_is_not_fetched_again() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);
    world.run(at(10), fetched(DAWN, FetchOutcome::NotModified));

    let trace = world.run(at(100_000), DEADLINE);

    assert!(fetches(&trace).is_empty());
}

#[test]
fn at_most_four_fetches_are_in_flight_and_the_rest_wait_their_turn() {
    let urls = [
        "https://img.test/1.jpg",
        "https://img.test/2.jpg",
        "https://img.test/3.jpg",
        "https://img.test/4.jpg",
        "https://img.test/5.jpg",
        "https://img.test/6.jpg",
    ];
    let mut world = World::new(init(remote_config(&urls, &[]), &[], &[]));
    world.start(at(0), Style::Light);

    let at_load = world.run(at(0), cache_loaded(&[]));
    let after_one = world.run(at(1), fetched(urls[2], failed(30)));
    let after_two = world.run(at(2), fetched(urls[0], FetchOutcome::NotModified));

    assert_eq!(MAX_FETCHES_IN_FLIGHT, 4);
    assert_eq!(fetches(&at_load), urls[..4]);
    assert_eq!(fetches(&after_one), [urls[4]]);
    assert_eq!(fetches(&after_two), [urls[5]]);
}

#[test]
fn a_new_version_of_the_image_on_screen_is_published_again() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);

    let trace = world.run(at(10), stored(&blob(DAWN, 2)));

    assert_eq!(probed_paths(&trace), [blob(DAWN, 2).blob]);
    assert_eq!(published(&trace), [(Style::Light, DAWN)]);
}

#[test]
fn not_modified_changes_nothing() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);

    let trace = world.run(at(10), fetched(DAWN, FetchOutcome::NotModified));

    assert!(trace.is_empty(), "{trace:?}");
}

#[test]
fn a_gone_url_keeps_its_stale_blob_in_the_rotation() {
    let mut world = remote_world(
        remote_config(&[DAWN, NOON], &[]),
        &[blob(DAWN, 1), blob(NOON, 1)],
    );

    let answer = world.run(at(10), fetched(DAWN, FetchOutcome::Gone));
    world.run(at(900), DEADLINE);
    let back_to_dawn = world.run(at(1800), DEADLINE);

    assert!(answer.is_empty(), "{answer:?}");
    assert_eq!(published(&back_to_dawn), [(Style::Light, DAWN)]);
}

#[test]
fn a_failed_fetch_is_tried_again_when_its_backoff_is_over() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);

    let answer = world.run(at(5), fetched(DAWN, failed(30)));
    let early = world.run(at(34), DEADLINE);
    let due = world.run(at(35), DEADLINE);

    assert_eq!(armed(&answer), [Some(at(35))]);
    assert!(fetches(&early).is_empty());
    assert_eq!(fetches(&due), [DAWN]);
}

#[test]
fn a_backoff_ends_no_later_than_its_whole_delay_after_the_clock_went_back() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);
    world.run(at(100), fetched(DAWN, failed(60)));

    let trace = world.run(at(0), CLOCK_CHANGED);

    assert_eq!(armed(&trace), [Some(at(60))]);
}

#[test]
fn a_rejected_body_keeps_the_old_blob_on_screen() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);

    let trace = world.run(at(5), fetched(DAWN, failed(30)));

    assert!(probed(&trace).is_empty());
    assert_eq!(world.current(Style::Light), Some(id(DAWN)));
}

#[test]
fn full_connectivity_fetches_again_what_failed() {
    let mut world = remote_world(remote_config(&[DAWN, NOON], &[]), &[]);
    world.run(at(5), fetched(DAWN, unreachable(3600)));
    world.run(at(5), fetched(NOON, FetchOutcome::NotModified));

    let limited = world.run(at(10), connectivity(Connectivity::Limited));
    let full = world.run(at(20), connectivity(Connectivity::Full));

    assert!(fetches(&limited).is_empty());
    assert_eq!(fetches(&full), [DAWN]);
}

#[test]
fn full_connectivity_keeps_the_backoff_the_server_asked_for() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);
    world.run(at(5), fetched(DAWN, failed(3600)));

    let flaps: Vec<_> = (1..=5)
        .flat_map(|n| {
            let mut trace = world.run(at(5 + n * 4), connectivity(Connectivity::Limited));
            trace.extend(world.run(at(7 + n * 4), connectivity(Connectivity::Full)));
            trace
        })
        .collect();
    let due = world.run(at(3605), DEADLINE);

    assert!(fetches(&flaps).is_empty(), "{flaps:?}");
    assert_eq!(fetches(&due), [DAWN]);
}

#[test]
fn full_connectivity_does_not_fetch_a_url_twice() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);

    let trace = world.run(at(10), connectivity(Connectivity::Full));

    assert!(fetches(&trace).is_empty());
}

#[test]
fn the_connectivity_reported_at_start_changes_nothing_and_blocks_nothing() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);

    let limited = world.run(at(0), connectivity(Connectivity::Limited));
    let full = world.run(at(0), connectivity(Connectivity::Full));
    world.run(at(5), fetched(DAWN, failed(30)));
    world.run(at(6), connectivity(Connectivity::Limited));
    let due = world.run(at(35), DEADLINE);

    assert!(limited.is_empty(), "{limited:?}");
    assert!(full.is_empty(), "{full:?}");
    assert_eq!(fetches(&due), [DAWN]);
}

#[test]
fn a_reload_fetches_the_urls_it_adds() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);

    let trace = world.run(
        at(10),
        Event::ConfigChanged(Ok(remote_config(&[DAWN, DUSK], &[]))),
    );

    assert_eq!(fetches(&trace), [DUSK]);
    assert!(dropped(&trace).is_empty());
}

#[test]
fn a_reload_drops_the_urls_it_removes_and_they_are_never_shown_again() {
    let mut world = remote_world(
        remote_config(&[DAWN, NOON], &[]),
        &[blob(DAWN, 1), blob(NOON, 1)],
    );

    let reload = world.run(
        at(10),
        Event::ConfigChanged(Ok(remote_config(&[NOON], &[]))),
    );
    let ticks: Vec<_> = (1..=3)
        .flat_map(|tick| world.run(at(10 + tick * 900), DEADLINE))
        .collect();
    let late_answer = world.run(at(3000), fetched(DAWN, FetchOutcome::NotModified));

    assert_eq!(dropped(&reload), [DAWN]);
    assert_eq!(published(&ticks), [(Style::Light, NOON)]);
    assert!(late_answer.is_empty(), "{late_answer:?}");
}

#[test]
fn a_style_whose_new_sources_are_still_to_be_scanned_does_not_borrow_meanwhile() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[blob(DAWN, 1)]);
    let mut local_only = remote_config(&[], &[]);
    local_only.styles.light.sources = vec![Source::Path("/l".into())];

    let reload = world.run(at(10), Event::ConfigChanged(Ok(local_only)));
    let tick_before_the_scan = world.run(at(900), DEADLINE);
    let scan = world.run(at(901), scanned(Style::Light, &["/l/a.jpg"]));

    // DAWN left the rotation, and light did not turn to dark's images
    // while its new folder was being scanned: it kept its slot until the
    // overdue turn could pick from that folder.
    assert_eq!(dropped(&reload), [DAWN]);
    assert!(published(&reload).is_empty(), "{reload:?}");
    assert!(
        published(&tick_before_the_scan).is_empty(),
        "{tick_before_the_scan:?}"
    );
    assert_eq!(published(&scan), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn a_url_that_stays_in_the_other_style_is_not_dropped() {
    let mut world = remote_world(remote_config(&[DAWN], &[DAWN]), &[blob(DAWN, 1)]);

    let trace = world.run(
        at(10),
        Event::ConfigChanged(Ok(remote_config(&[], &[DAWN]))),
    );

    assert!(dropped(&trace).is_empty());
}

#[test]
fn garbage_is_collected_once_the_cache_is_loaded_and_then_daily() {
    let mut world = World::new(init(remote_config(&[DAWN], &[NOON]), &[], &[]));
    world.start(at(0), Style::Light);

    let at_load = world.run(at(0), cache_loaded(&[]));
    let a_day_later = world.run(at(GC_PERIOD.as_secs()), DEADLINE);

    assert_eq!(collected(&at_load), [vec![DAWN, NOON]]);
    assert_eq!(collected(&a_day_later), [vec![DAWN, NOON]]);
}

#[test]
fn without_any_remote_image_garbage_is_collected_only_at_start() {
    let mut world = World::new(init(alphabetical(), &["/l/a.jpg"], &[]));
    world.start(at(0), Style::Light);

    let at_load = world.run(at(0), cache_loaded(&[]));

    assert_eq!(collected(&at_load), [Vec::<&str>::new()]);
    assert!(armed(&at_load).is_empty());
}

#[test]
fn a_blob_that_vanished_is_fetched_again_and_the_slot_keeps_its_image() {
    let mut world = remote_world(
        remote_config(&[DAWN, NOON], &[]),
        &[blob(DAWN, 1), blob(NOON, 1)],
    );
    world.run(at(1), fetched(DAWN, FetchOutcome::NotModified));
    world.run(at(1), fetched(NOON, FetchOutcome::NotModified));
    world.rejected.insert(id(NOON), BadReason::Missing);

    let tick = world.run(at(900), DEADLINE);

    // current() is live state. The next tick publishes NOON.
    assert_eq!(fetches(&tick), [NOON]);
    assert!(published(&tick).is_empty());
    assert_eq!(world.current(Style::Light), Some(id(DAWN)));

    world.rejected.clear();
    let refetched = world.run(at(905), stored(&blob(NOON, 1)));
    let next_tick = world.run(at(1800), DEADLINE);

    assert!(published(&refetched).is_empty());
    assert_eq!(published(&next_tick), [(Style::Light, NOON)]);
}

#[test]
fn a_blob_lost_while_its_fetch_is_in_flight_is_fetched_again_after_a_not_modified() {
    let mut world = remote_world(
        remote_config(&[DAWN, NOON], &[]),
        &[blob(DAWN, 1), blob(NOON, 1)],
    );
    world.run(at(1), fetched(DAWN, FetchOutcome::NotModified));
    world.rejected.insert(id(NOON), BadReason::Missing);

    let tick = world.run(at(900), DEADLINE);
    world.rejected.clear();
    let answer = world.run(at(905), fetched(NOON, FetchOutcome::NotModified));
    let refetched = world.run(at(910), stored(&blob(NOON, 2)));
    let next_tick = world.run(at(1800), DEADLINE);

    // The answer in flight was asked for while the blob was still there.
    assert!(fetches(&tick).is_empty(), "{tick:?}");
    assert_eq!(fetches(&answer), [NOON]);
    assert!(published(&refetched).is_empty());
    assert_eq!(published(&next_tick), [(Style::Light, NOON)]);
}

#[test]
fn with_remote_refresh_never_a_url_listed_again_after_a_garbage_collection_is_fetched() {
    let mut config = remote_config(&[DAWN], &[]);
    config.general.remote_refresh = RemoteRefresh::Never;
    let mut world = remote_world(config.clone(), &[blob(DAWN, 1)]);
    let mut without = config.clone();
    without.styles.light.sources.clear();
    world.run(at(10), Event::ConfigChanged(Ok(without.clone())));
    let back_before_gc = world.run(at(20), Event::ConfigChanged(Ok(config.clone())));
    world.run(at(30), Event::ConfigChanged(Ok(without)));

    let gc = world.run(at(GC_PERIOD.as_secs()), DEADLINE);
    let back_after_gc = world.run(
        at(GC_PERIOD.as_secs() + 10),
        Event::ConfigChanged(Ok(config)),
    );

    assert!(fetches(&back_before_gc).is_empty(), "{back_before_gc:?}");
    assert_eq!(collected(&gc), [Vec::<&str>::new()]);
    assert_eq!(fetches(&back_after_gc), [DAWN]);
}

#[test]
fn a_tick_rotates_while_a_remote_url_is_still_being_fetched() {
    let config = remote_config(&[DAWN], &[]);
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Light);
    let at_load = world.run(at(0), cache_loaded(&[]));

    let tick = world.run(at(900), DEADLINE);

    assert_eq!(fetches(&at_load), [DAWN]);
    assert_eq!(published(&tick), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn an_answer_nobody_asked_for_is_ignored() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);

    let trace = world.run(at(5), fetched(NOON, FetchOutcome::NotModified));

    assert!(trace.is_empty(), "{trace:?}");
}

#[test]
fn a_second_cache_report_is_ignored() {
    let mut world = remote_world(remote_config(&[DAWN], &[]), &[]);

    let trace = world.run(at(5), cache_loaded(&[blob(DAWN, 1)]));

    assert!(trace.is_empty(), "{trace:?}");
}

#[test]
fn a_saved_remote_image_is_restored_from_its_blob_before_the_scan() {
    let config = remote_config(&[DAWN], &[]);
    let mut first = remote_world(config.clone(), &[blob(DAWN, 1)]);
    first.run(at(1), fetched(DAWN, FetchOutcome::NotModified));
    let mut second = first.restart(unscanned(config));
    second.slots.light = None;

    let at_start = second.start(at(100), Style::Light);
    let at_load = second.run(at(100), cache_loaded(&[blob(DAWN, 1)]));

    assert!(probed(&at_start).is_empty());
    assert_eq!(probed_paths(&at_load), [blob(DAWN, 1).blob]);
    assert_eq!(published(&at_load), [(Style::Light, DAWN)]);
}

#[test]
fn in_uri_mode_the_key_names_the_blob() {
    let mut config = remote_config(&[DAWN], &[]);
    config.general.apply_mode = ApplyMode::Uri;
    let world = remote_world(config, &[blob(DAWN, 1)]);

    let key = world.keys.picture_uri.clone();

    assert_eq!(key, "file:///cache/dawn.jpg/v1.jpg");
}
