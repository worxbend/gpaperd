//! The background keys and the style switch: the GNOME rows of the plan's
//! corner-case table, and `apply_mode = "uri"`.

use gpaperd_core::event::{BackgroundKeys, Pause};
use gpaperd_core::step::{HIDDEN_REFRESH_DELAY, MAX_KEY_FLIPS_PER_MINUTE, REASSERT_DELAY};
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_daemon::gnome::KeyWrite;

use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, SEED, World, eventually, let_the_daemon_run, scenario,
    start_time, users_keys,
};

/// What the user picks in Settings.
const PICKED_BY_THE_USER: &str = "file:///user/picked.png";

const REASSERT_NEXT_TICK: &str =
    "order = \"alphabetical\"\non_external_change = \"reassert-next-tick\"";
const REASSERT_NOW: &str = "order = \"alphabetical\"\non_external_change = \"reassert-now\"";
const URI_MODE: &str = "order = \"alphabetical\"\napply_mode = \"uri\"";

#[test]
fn an_external_key_change_pauses_the_style_until_the_key_is_back() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        world
            .gnome
            .set_key_from_outside(Style::Light, PICKED_BY_THE_USER);
        let paused = daemon.next_status().await;
        let alarm_while_paused = world.clock.armed();
        let was_saved_as_paused = world.saved_state().styles.light.paused;
        // The deadline passes, and nothing may rotate.
        world.clock.advance(LIGHT_INTERVAL);
        let_the_daemon_run().await;
        let shown_while_paused = world.shown(Style::Light);
        let key_while_paused = world.gnome.key(Style::Light);
        world
            .gnome
            .set_key_from_outside(Style::Light, &world.slot_uri(Style::Light));
        // The style is overdue by now, so it rotates at once.
        let resumed = world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(paused.paused.light, Some(Pause::ExternalChange));
        assert_eq!(paused.paused.dark, None);
        assert_eq!(paused.next_change, None);
        assert_eq!(alarm_while_paused, None);
        assert!(was_saved_as_paused);
        assert_eq!(shown_while_paused, "a.png");
        assert_eq!(key_while_paused.as_deref(), Some(PICKED_BY_THE_USER));
        assert_eq!(resumed.paused.light, None);
        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn reassert_next_tick_writes_the_key_back_with_the_next_rotation() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(REASSERT_NEXT_TICK), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        world
            .gnome
            .set_key_from_outside(Style::Light, PICKED_BY_THE_USER);
        let_the_daemon_run().await;
        let key_before_the_tick = world.gnome.key(Style::Light);
        world.clock.advance(LIGHT_INTERVAL);
        let after_the_tick = world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(key_before_the_tick.as_deref(), Some(PICKED_BY_THE_USER));
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
        assert_eq!(after_the_tick.paused, PerStyle::default());
    });
}

#[test]
fn reassert_now_writes_the_key_back_after_a_second() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(REASSERT_NOW), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        world
            .gnome
            .set_key_from_outside(Style::Light, PICKED_BY_THE_USER);
        let write_back_due = world.clock.time() + REASSERT_DELAY;
        eventually("the write-back to be planned", || {
            world.clock.armed() == Some(write_back_due)
        })
        .await;
        let key_before_the_delay = world.gnome.key(Style::Light);
        world.clock.advance(REASSERT_DELAY);
        eventually("the key to be written back", || {
            world.gnome.key(Style::Light) == Some(world.slot_uri(Style::Light))
        })
        .await;

        assert_eq!(key_before_the_delay.as_deref(), Some(PICKED_BY_THE_USER));
    });
}

#[test]
fn a_key_that_keeps_flipping_pauses_the_style() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(REASSERT_NOW), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        // Another tool takes the key back each time, a second apart.
        for _ in 0..MAX_KEY_FLIPS_PER_MINUTE {
            world
                .gnome
                .set_key_from_outside(Style::Light, PICKED_BY_THE_USER);
            let write_back_due = world.clock.time() + REASSERT_DELAY;
            eventually("the write-back to be planned", || {
                world.clock.armed() == Some(write_back_due)
            })
            .await;
            world.clock.advance(REASSERT_DELAY);
            eventually("the key to be written back", || {
                world.gnome.key(Style::Light) == Some(world.slot_uri(Style::Light))
            })
            .await;
        }
        world
            .gnome
            .set_key_from_outside(Style::Light, PICKED_BY_THE_USER);
        let after_one_flip_more = daemon.next_status().await;
        world.clock.advance(REASSERT_DELAY);
        let_the_daemon_run().await;

        assert_eq!(
            after_one_flip_more.paused.light,
            Some(Pause::ExternalChange)
        );
        // The daemon gave up the fight: the key stays with the other tool.
        assert_eq!(
            world.gnome.key(Style::Light).as_deref(),
            Some(PICKED_BY_THE_USER)
        );
    });
}

#[test]
fn a_switch_to_a_slot_written_while_hidden_republishes_it_once_after_500_ms() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        // The dark slot was filled while light was on screen.
        world.showing(&mut daemon, "a.png", "x.png").await;

        world.gnome.switch_style(Style::Dark);
        daemon.next_status().await;
        let refresh_due = world.clock.armed();
        let stale_slot = world.slot_inode(Style::Dark);
        world.clock.advance(HIDDEN_REFRESH_DELAY);
        eventually("the dark slot to get a new inode", || {
            world.slot_inode(Style::Dark) != stale_slot
        })
        .await;
        eventually("the refresh to be saved", || {
            !world.saved_state().styles.dark.dirty_while_hidden
        })
        .await;
        let fresh_slot = world.slot_inode(Style::Dark);
        // A second switch to the slot finds it clean.
        world.gnome.switch_style(Style::Light);
        daemon.next_status().await;
        world.gnome.switch_style(Style::Dark);
        daemon.next_status().await;
        let alarm_after_the_second_switch = world.clock.armed();
        world.clock.advance(HIDDEN_REFRESH_DELAY);
        let_the_daemon_run().await;

        assert_eq!(refresh_due, Some(start_time() + HIDDEN_REFRESH_DELAY));
        assert_eq!(world.shown(Style::Dark), "x.png");
        assert_eq!(alarm_after_the_second_switch, None);
        assert_eq!(world.slot_inode(Style::Dark), fresh_slot);
    });
}

#[test]
fn locked_keys_that_point_at_the_slots_still_rotate() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let locked_on_the_slots = BackgroundKeys {
            picture_uri: world.slot_uri(Style::Light),
            picture_uri_dark: Some(world.slot_uri(Style::Dark)),
            picture_options: "zoom".to_owned(),
        };
        world.gnome.start_with_keys(locked_on_the_slots.clone());
        world.gnome.lock_keys();
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        let at_start = world.showing(&mut daemon, "a.png", "x.png").await;

        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(at_start.paused, PerStyle::default());
        assert_eq!(world.shown(Style::Light), "b.png");
        assert_eq!(world.gnome.keys(), locked_on_the_slots);
        assert!(!world.logged("background keys not written"));
    });
}

#[test]
fn locked_keys_that_point_elsewhere_idle_the_daemon() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        // Locked on the user's own wallpapers.
        world.gnome.lock_keys();
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        let status = daemon.ready().await;
        eventually("both scans", || world.times_logged("sources scanned") == 2).await;
        let_the_daemon_run().await;

        let locked = Some(Pause::LockedKey);
        assert_eq!(
            status.paused,
            PerStyle {
                light: locked,
                dark: locked
            }
        );
        assert_eq!(status.next_change, None);
        assert_eq!(world.gnome.keys(), users_keys());
        assert!(world.gnome.key_writes().is_empty());
        assert!(!world.paths().slots_dir.exists());
    });
}

#[test]
fn a_session_without_the_dark_key_runs_the_light_style_only() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        // GNOME before 42.
        world.gnome.start_with_keys(BackgroundKeys {
            picture_uri_dark: None,
            ..users_keys()
        });
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        let at_start = world.showing(&mut daemon, "a.png", None).await;

        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", None).await;
        eventually("both scans", || world.times_logged("sources scanned") == 2).await;
        let_the_daemon_run().await;

        assert_eq!(
            at_start.paused,
            PerStyle {
                light: None,
                dark: Some(Pause::NoKey)
            }
        );
        assert_eq!(world.shown(Style::Light), "b.png");
        assert_eq!(
            world.gnome.keys(),
            BackgroundKeys {
                picture_uri: world.slot_uri(Style::Light),
                picture_uri_dark: None,
                picture_options: "zoom".to_owned(),
            }
        );
        assert!(!world.paths().slot(Style::Dark).exists());
    });
}

#[test]
fn picture_options_none_becomes_zoom() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        // With `none` the shell loads no file at all.
        world.gnome.start_with_keys(BackgroundKeys {
            picture_options: "none".to_owned(),
            ..users_keys()
        });
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        world.showing(&mut daemon, "a.png", "x.png").await;

        assert_eq!(world.gnome.keys().picture_options, "zoom");
    });
}

#[test]
fn uri_mode_writes_each_images_own_uri_and_touches_no_slot() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(URI_MODE), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let keys_at_start = world.gnome.keys();

        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(
            keys_at_start,
            BackgroundKeys {
                picture_uri: world.image_uri(Style::Light, "a.png"),
                picture_uri_dark: Some(world.image_uri(Style::Dark, "x.png")),
                picture_options: "zoom".to_owned(),
            }
        );
        // The rotation of light wrote light's key and no other.
        let rotation = KeyWrite {
            uris: PerStyle {
                light: Some(world.image_uri(Style::Light, "b.png")),
                dark: None,
            },
            picture_options: None,
        };
        assert_eq!(world.gnome.key_writes().last(), Some(&rotation));
        assert_eq!(
            world.gnome.key(Style::Dark),
            Some(world.image_uri(Style::Dark, "x.png"))
        );
        assert!(!world.paths().slots_dir.exists());
    });
}

#[test]
fn uri_mode_writes_both_keys_in_one_transaction() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut first_run = world.start_daemon(world.config(URI_MODE), SEED);
        world.showing(&mut first_run, "a.png", "x.png").await;
        first_run.terminate().await;
        let writes_of_the_first_run = world.gnome.key_writes().len();

        // A restart knows both images from the state file, so both are
        // checked before the scans and their keys go out together.
        let mut second_run = world.start_daemon(world.config(URI_MODE), SEED);
        second_run.ready().await;

        let both_keys = KeyWrite {
            uris: PerStyle {
                light: Some(world.image_uri(Style::Light, "a.png")),
                dark: Some(world.image_uri(Style::Dark, "x.png")),
            },
            picture_options: None,
        };
        assert_eq!(
            world.gnome.key_writes()[writes_of_the_first_run..],
            [both_keys]
        );
    });
}

#[test]
fn uri_mode_takes_a_key_that_differs_from_its_last_write_as_an_external_change() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(URI_MODE), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        // Another image of the daemon's own folder, but not its choice.
        world
            .gnome
            .set_key_from_outside(Style::Light, &world.image_uri(Style::Light, "b.png"));
        let status = daemon.next_status().await;

        assert_eq!(status.paused.light, Some(Pause::ExternalChange));
    });
}
