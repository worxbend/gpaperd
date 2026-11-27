//! The whole daemon without GNOME: the real interpreter, reducer, source
//! watches, image check, slot files, config watch and state file in a temp
//! directory, behind a fake `GnomePort` and a clock that the test moves by
//! hand.
//!
//! This file holds the start, the rotation and the restart. The other
//! scenarios follow the plan's corner-case table: `files` has its Local and
//! Slots rows, `keys` its GNOME rows, `reload` its Config rows, `remote`
//! its Remote rows and `time` its Time rows. `control` drives the D-Bus
//! control interface on a private bus, and `screen_lock` a fake GNOME
//! screen saver on one.

#[path = "end_to_end/bus.rs"]
mod bus;
#[path = "end_to_end/control.rs"]
mod control;
#[path = "end_to_end/files.rs"]
mod files;
#[path = "end_to_end/keys.rs"]
mod keys;
#[path = "end_to_end/reload.rs"]
mod reload;
#[path = "end_to_end/remote.rs"]
mod remote;
#[path = "end_to_end/screen_lock.rs"]
mod screen_lock;
#[path = "end_to_end/time.rs"]
mod time;
#[path = "end_to_end/world.rs"]
mod world;

use std::fs;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpaperd_core::config::Config;
use gpaperd_core::event::{BackgroundKeys, Status};
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_daemon::config_watch::ConfigWatch;
use gpaperd_daemon::gnome::GnomeError;
use gpaperd_daemon::interp::{Interpreter, RunError};
use tokio::sync::oneshot;
use tokio::time::timeout;
use url::Url;

use bus::PrivateBus;
use world::{
    ALPHABETICAL, DARK_INTERVAL, LIGHT_INTERVAL, MINUTE, PATIENCE, SEED, World, eventually,
    has_a_deadline, scenario, start_time, users_keys,
};

/// The `[general]` table of a config with the default order.
const RANDOM_ORDER: &str = "";

fn both_slots_are_filled(status: &Status) -> bool {
    status.current.light.is_some() && status.current.dark.is_some()
}

#[test]
fn both_styles_get_a_slot_and_the_keys_point_at_the_slots() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["day.png"]);
        world.add_images(Style::Dark, &["night.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        world.showing(&mut daemon, "day.png", "night.png").await;

        assert_eq!(world.shown(Style::Light), "day.png");
        assert_eq!(world.shown(Style::Dark), "night.png");
        assert_eq!(
            world.gnome.keys(),
            BackgroundKeys {
                picture_uri: world.slot_uri(Style::Light),
                picture_uri_dark: Some(world.slot_uri(Style::Dark)),
                picture_options: "zoom".to_owned(),
            }
        );
    });
}

#[test]
fn on_a_first_run_ready_comes_before_the_scans() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["day.png"]);
        world.add_images(Style::Dark, &["night.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        let at_ready = daemon.ready().await;
        world.showing(&mut daemon, "day.png", "night.png").await;

        // No state file names an image, so the first apply had nothing to
        // show, and readiness did not wait for the folders.
        assert_eq!(at_ready.current, PerStyle::default());
    });
}

#[test]
fn a_style_without_images_keeps_the_users_key() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["day.png"]);
        let keep_current = r#"empty_style = "keep-current""#;
        let mut daemon = world.start_daemon(world.config(keep_current), SEED);

        world.showing(&mut daemon, "day.png", None).await;

        assert_eq!(world.shown(Style::Light), "day.png");
        assert_eq!(
            world.gnome.keys(),
            BackgroundKeys {
                picture_uri: world.slot_uri(Style::Light),
                ..users_keys()
            }
        );
    });
}

#[test]
fn the_keys_are_written_only_once_the_users_keys_are_saved() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        // A file where the state directory belongs: nothing can be saved.
        let blocker = world.root().join("state");
        fs::write(&blocker, "").expect("block the state directory");
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        world.showing(&mut daemon, "a.png", "x.png").await;
        let keys_while_unsaved = world.gnome.keys();
        fs::remove_file(&blocker).expect("unblock the state directory");
        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(keys_while_unsaved, users_keys());
        assert_eq!(world.gnome.keys().picture_uri, world.slot_uri(Style::Light));
    });
}

#[test]
fn a_fired_deadline_rotates_only_the_visible_style() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let dark_slot = world.slot_inode(Style::Dark);

        world.clock.advance(LIGHT_INTERVAL);
        let status = daemon.next_status().await;

        assert_eq!(world.shown(Style::Light), "b.png");
        assert_eq!(world.shown(Style::Dark), "x.png");
        assert_eq!(world.slot_inode(Style::Dark), dark_slot);
        assert_eq!(status.next_change, Some(start_time() + 2 * LIGHT_INTERVAL));
    });
}

#[test]
fn a_style_switch_moves_the_timer_and_changes_no_image() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        let at_start = world.showing(&mut daemon, "a.png", "x.png").await;
        assert_eq!(at_start.next_change, Some(start_time() + LIGHT_INTERVAL));

        world.clock.advance(5 * MINUTE);
        world.gnome.switch_style(Style::Dark);
        let on_dark = daemon.next_status().await;
        world.clock.advance(MINUTE);
        world.gnome.switch_style(Style::Light);
        let on_light = daemon.next_status().await;

        // Dark was never on screen, so its whole interval was left.
        let dark_deadline = start_time() + 5 * MINUTE + DARK_INTERVAL;
        assert_eq!(on_dark.next_change, Some(dark_deadline));
        // Light was on screen for 5 of its 15 minutes before it was hidden.
        let light_deadline = start_time() + 6 * MINUTE + 10 * MINUTE;
        assert_eq!(on_light.next_change, Some(light_deadline));
        assert_eq!(world.clock.armed(), Some(light_deadline));
        assert_eq!(world.shown(Style::Light), "a.png");
        assert_eq!(world.shown(Style::Dark), "x.png");
    });
}

#[test]
fn a_failed_candidate_falls_through_to_the_next() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "c.png"]);
        world.add_file_that_is_no_image(Style::Light, "b.png");
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        world.clock.advance(LIGHT_INTERVAL);
        daemon.next_status().await;

        assert_eq!(world.shown(Style::Light), "c.png");
    });
}

#[test]
fn a_restart_continues_the_round_and_leaves_the_slot_alone() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png", "c.png", "d.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut first_run = world.start_daemon(world.config(RANDOM_ORDER), SEED);
        first_run
            .status_where("both slots to be filled", both_slots_are_filled)
            .await;
        let mut shown = vec![world.shown(Style::Light)];
        shown.push(world.shown_after_next_rotation(&mut first_run).await);
        first_run.terminate().await;
        let slot_before = world.slot_inode(Style::Light);

        // Another seed: only the state file can carry the round over.
        let mut second_run = world.start_daemon(world.config(RANDOM_ORDER), SEED + 1);
        second_run
            .status_where("the scan to bring a deadline", has_a_deadline)
            .await;
        let slot_after = world.slot_inode(Style::Light);
        let shown_after_restart = world.shown(Style::Light);
        shown.push(world.shown_after_next_rotation(&mut second_run).await);
        shown.push(world.shown_after_next_rotation(&mut second_run).await);

        assert_eq!(slot_after, slot_before);
        assert_eq!(shown_after_restart, shown[1]);
        shown.sort();
        assert_eq!(shown, ["a.png", "b.png", "c.png", "d.png"]);
    });
}

#[test]
fn an_image_added_while_the_daemon_was_off_joins_the_restored_round() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png", "c.png", "d.png", "e.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut first_run = world.start_daemon(world.config(RANDOM_ORDER), SEED);
        first_run
            .status_where("both slots to be filled", both_slots_are_filled)
            .await;
        let mut shown = vec![world.shown(Style::Light)];
        shown.push(world.shown_after_next_rotation(&mut first_run).await);
        first_run.terminate().await;

        world.add_images(Style::Light, &["f.png"]);
        let mut second_run = world.start_daemon(world.config(RANDOM_ORDER), SEED + 1);
        second_run
            .status_where("the scan to bring a deadline", has_a_deadline)
            .await;
        for _ in 0..4 {
            shown.push(world.shown_after_next_rotation(&mut second_run).await);
        }

        // The round had three images left. With the new one that is four,
        // and no image comes twice.
        shown.sort();
        assert_eq!(
            shown,
            ["a.png", "b.png", "c.png", "d.png", "e.png", "f.png"]
        );
    });
}

#[test]
fn a_restart_fills_a_lost_slot_from_the_saved_state_before_it_is_ready() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut first_run = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut first_run, "a.png", "x.png").await;
        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut first_run, "b.png", "x.png").await;
        first_run.terminate().await;
        fs::remove_file(world.paths().slot(Style::Light)).expect("delete the slot file");

        let mut second_run = world.start_daemon(world.config(ALPHABETICAL), SEED);
        let at_ready = second_run.ready().await;

        // Not the first image of the folder: the one that the state names.
        assert_eq!(world.shown(Style::Light), "b.png");
        assert!(both_slots_are_filled(&at_ready), "{at_ready:?}");
    });
}

#[test]
fn shutdown_saves_what_changed_since_the_last_rotation() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        // A switch stops light's clock, and nothing is saved on a switch.
        world.clock.advance(5 * MINUTE);
        world.gnome.switch_style(Style::Dark);
        daemon.next_status().await;

        daemon.terminate().await;

        let saved = world.saved_state();
        assert_eq!(saved.styles.light.remaining, Some(10 * MINUTE));
        assert_eq!(saved.original_keys, Some(users_keys()));
    });
}

#[test]
fn a_daemon_without_sources_gets_ready_and_changes_nothing() {
    scenario(async {
        let world = World::new();
        let mut daemon = world.start_daemon(Config::default(), SEED);

        let status = daemon.ready().await;

        assert_eq!(status.next_change, None);
        assert_eq!(world.gnome.keys(), users_keys());
        assert!(!world.paths().slots_dir.exists());
    });
}

#[test]
fn a_stop_request_is_served_while_the_saved_state_loads() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        let state_file = world.paths().state_file;
        fs::create_dir_all(state_file.parent().expect("a state directory"))
            .expect("create the state directory");
        let made = std::process::Command::new("mkfifo")
            .arg(&state_file)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo");

        let progressed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&progressed);
        tokio::task::spawn_local(async move {
            tokio::task::yield_now().await;
            flag.store(true, Ordering::SeqCst);
        });
        let saw = Arc::clone(&progressed);
        let watchdog = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            saw.load(Ordering::SeqCst)
        });

        let (ports, _notices) = world.ports();
        let config = world.config(ALPHABETICAL);
        let start = Interpreter::start(Some(config), world.paths(), SEED, ports);
        let mut start = pin!(start);
        // A read of the fifo on this thread never reaches the timeout.
        let early = timeout(Duration::from_millis(200), &mut start).await;
        assert!(
            early.is_err(),
            "start finished before the state fifo was written"
        );
        assert!(
            progressed.load(Ordering::SeqCst),
            "state load blocked the runtime thread"
        );

        let path = state_file.clone();
        std::thread::spawn(move || {
            fs::write(&path, b"{").expect("unblock the state fifo");
        })
        .join()
        .expect("the writer thread");
        let started = timeout(PATIENCE, start)
            .await
            .expect("start unblocks once the fifo is written");
        assert!(started.is_ok(), "corrupt state is not a startup failure");
        assert!(
            watchdog.join().expect("the watchdog thread"),
            "the runtime made no progress while the state fifo was unread"
        );
    });
}

#[test]
fn io_on_an_existing_state_file_fails_startup_and_leaves_the_file() {
    scenario(async {
        let world = World::new();
        let state_file = world.paths().state_file;
        fs::create_dir_all(&state_file).expect("a directory at the state path");
        fs::write(state_file.join("marker"), b"stay").expect("a marker inside it");
        let (ports, _notices) = world.ports();

        let started = Interpreter::start(None, world.paths(), SEED, ports).await;

        assert!(started.is_err(), "I/O on an existing state file must fail start");
        assert_eq!(fs::read(state_file.join("marker")).expect("the marker"), b"stay");
    });
}

#[test]
fn a_corrupt_state_file_lets_start_succeed_and_leaves_the_bytes() {
    scenario(async {
        let world = World::new();
        let state_file = world.paths().state_file;
        fs::create_dir_all(state_file.parent().expect("a state directory"))
            .expect("create the state directory");
        fs::write(&state_file, b"not json").expect("corrupt state");
        let (ports, _notices) = world.ports();

        let started = Interpreter::start(None, world.paths(), SEED, ports).await;

        assert!(started.is_ok(), "{started:?}");
        assert_eq!(fs::read(&state_file).expect("the state file"), b"not json");
    });
}

#[test]
fn a_later_save_does_not_flush_a_key_write_the_failed_save_held() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let inbox = world.gnome.connect();
        let (ports, _notices) = world.ports();
        let interpreter =
            Interpreter::start(Some(world.config(ALPHABETICAL)), world.paths(), SEED, ports)
                .await
                .expect("start");
        let state_dir = world
            .paths()
            .state_file
            .parent()
            .expect("a state dir")
            .to_path_buf();
        if state_dir.exists() {
            fs::remove_dir_all(&state_dir).expect("clear the state directory");
        }
        fs::create_dir_all(state_dir.parent().expect("the state parent"))
            .expect("create the state parent");
        fs::write(&state_dir, b"not a directory").expect("block the save");

        let (stop, stopped) = oneshot::channel::<()>();
        let config_watch = ConfigWatch::start(world.config_file());
        let run = tokio::task::spawn_local(async move {
            interpreter
                .run(inbox, config_watch, async move {
                    let _ = stopped.await;
                })
                .await
        });
        world
            .log_line("background keys not written: the user's keys are not saved yet")
            .await;
        assert!(
            world.gnome.key_writes().is_empty(),
            "the held write was sent to GNOME"
        );

        fs::remove_file(&state_dir).expect("allow a later save");
        stop.send(()).expect("stop");
        let outcome = timeout(PATIENCE, run)
            .await
            .expect("the daemon to stop")
            .expect("the task");
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            world.gnome.key_writes().is_empty(),
            "a later save flushed the key write the failed save held"
        );
        assert_eq!(world.gnome.keys(), users_keys());
    });
}

#[test]
fn a_failed_shutdown_disarm_still_saves() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        // A switch stops light's clock, and nothing is saved on a switch.
        world.clock.advance(5 * MINUTE);
        world.gnome.switch_style(Style::Dark);
        daemon.next_status().await;

        world.clock.fail_the_next_disarm();
        daemon.terminate().await;

        let saved = world.saved_state();
        assert_eq!(saved.styles.light.remaining, Some(10 * MINUTE));
        assert_eq!(saved.original_keys, Some(users_keys()));
    });
}

#[test]
fn a_url_only_style_does_not_warn_that_no_local_image_was_found() {
    scenario(async {
        let world = World::new();
        let text = world.config_text_with_light_sources(
            ALPHABETICAL,
            &["https://images.example/wall.jpg"],
        );
        world.save_config_file(&text);
        let mut daemon = world.start_daemon_from_config_file(SEED);
        daemon.ready().await;
        eventually("both scans", || {
            world.times_logged("sources scanned") == 2
        })
        .await;

        assert!(
            !world.logged("no image found"),
            "a URL-only style was reported as an empty folder"
        );
        daemon.terminate().await;
    });
}

/// The log a second process writes when it refuses to sweep a cache it does
/// not own.
const NOT_THE_OWNER: &str = "does not own the bus name";

#[test]
fn a_daemon_without_the_bus_name_does_not_sweep_the_cache() {
    scenario(async {
        let world = World::new();
        let planted = Url::parse("https://images.example/wall.jpg").expect("a URL");
        let blob = world.cache_remote_image(&planted, "kept", "etag").await;
        // A listed URL makes Ready wait for the cache, so the sweep decision
        // is already logged when ready returns. The planted blob is not that
        // URL: a sweep would be allowed to delete it.
        let text = world.config_text_with_light_sources(
            ALPHABETICAL,
            &["https://images.example/other.jpg"],
        );
        world.save_config_file(&text);
        let mut daemon = world.start_daemon_from_config_file(SEED);
        daemon.ready().await;

        assert!(world.logged(NOT_THE_OWNER));
        assert!(blob.exists(), "the cache was swept without the bus name");
        daemon.terminate().await;
    });
}

#[test]
fn a_daemon_that_owns_the_bus_name_does_not_skip_the_sweep() {
    scenario(async {
        let bus = PrivateBus::start();
        let world = World::new();
        let planted = Url::parse("https://images.example/wall.jpg").expect("a URL");
        world.cache_remote_image(&planted, "kept", "etag").await;
        let text = world.config_text_with_light_sources(
            ALPHABETICAL,
            &["https://images.example/other.jpg"],
        );
        world.save_config_file(&text);
        let mut daemon = world.start_daemon_on_bus(bus.address(), SEED).await;
        daemon.ready().await;

        assert!(!world.logged(NOT_THE_OWNER));
        daemon.terminate().await;
    });
}

#[test]
fn the_daemon_fails_when_gnome_goes_away() {
    scenario(async {
        let world = World::new();
        let mut daemon = world.start_daemon(Config::default(), SEED);
        daemon.ready().await;

        world.gnome.disconnect();
        let outcome = daemon.outcome().await;

        assert!(
            matches!(outcome, Err(RunError::Gnome(GnomeError::Stopped))),
            "{outcome:?}"
        );
    });
}
