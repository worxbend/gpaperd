//! The config file while the daemon runs: the Config rows of the plan's
//! corner-case table. These scenarios start the daemon the way `main` does,
//! on the file.

use std::fs;

use gpaperd_core::event::Pause;
use gpaperd_core::style::{PerStyle, Style};

use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, MINUTE, SEED, World, has_a_deadline, scenario, start_time,
    users_keys, write_image,
};

#[test]
fn a_daemon_without_a_config_file_idles_until_the_file_appears() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon_from_config_file(SEED);
        let idle = daemon.ready().await;
        let keys_while_idle = world.gnome.keys();

        world.save_config_file(&world.config_text(ALPHABETICAL));
        world.showing(&mut daemon, "a.png", "x.png").await;

        assert_eq!(idle.current, PerStyle::default());
        assert_eq!(idle.next_change, None);
        assert_eq!(keys_while_idle, users_keys());
        assert_eq!(world.shown(Style::Light), "a.png");
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
    });
}

#[test]
fn a_config_edit_changes_the_interval_and_the_sources() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let other_folder = world.root().join("images/other");
        fs::create_dir(&other_folder).expect("create the other folder");
        write_image(&other_folder.join("b.png"), "b.png");
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        let before = world.showing(&mut daemon, "a.png", "x.png").await;

        let five_minutes = "order = \"alphabetical\"\ninterval = \"5m\"";
        world.save_config_file(&world.config_text_reading(five_minutes, &other_folder));
        // The image on screen is not in the new folder, so there is one to
        // rotate to as soon as that folder is scanned.
        let reloaded = daemon
            .status_where("a deadline by the new interval", has_a_deadline)
            .await;
        world.clock.advance(5 * MINUTE);
        daemon.next_status().await;

        assert_eq!(before.next_change, None);
        assert_eq!(reloaded.next_change, Some(start_time() + 5 * MINUTE));
        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn an_invalid_config_edit_keeps_the_running_config() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        // Below the shortest interval, and without any source.
        world.save_config_file("[general]\ninterval = \"30s\"\n");
        world.log_line("config file is invalid").await;
        world.clock.advance(LIGHT_INTERVAL);
        let after_the_tick = world.showing(&mut daemon, "b.png", "x.png").await;

        assert!(world.logged("line 2, column 12"));
        assert_eq!(
            after_the_tick.next_change,
            Some(start_time() + 2 * LIGHT_INTERVAL)
        );
    });
}

#[test]
fn a_deleted_config_file_keeps_the_running_config() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        fs::remove_file(world.paths().config_file).expect("delete the config file");
        world.log_line("config file is gone").await;
        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn a_config_reload_resumes_a_paused_style() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        world
            .gnome
            .set_key_from_outside(Style::Light, "file:///user/picked.png");
        let paused = daemon.next_status().await;

        let twenty_minutes = "order = \"alphabetical\"\ninterval = \"20m\"";
        world.save_config_file(&world.config_text(twenty_minutes));
        let resumed = daemon
            .status_where("the pause to end", |status| status.paused.light.is_none())
            .await;

        assert_eq!(paused.paused.light, Some(Pause::ExternalChange));
        assert_eq!(resumed.next_change, Some(start_time() + 20 * MINUTE));
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
    });
}

#[test]
fn a_reload_reads_the_key_locks_again() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let picked = "file:///user/picked.png";
        world.gnome.set_key_from_outside(Style::Light, picked);
        daemon.next_status().await;
        let writes_before = world.gnome.key_writes().len();

        // The lock comes without a report, as some backends do it.
        world.gnome.lock_keys();
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let locked = daemon
            .status_where("light locked", |status| {
                status.paused.light == Some(Pause::LockedKey)
            })
            .await;

        // A reload ends an external-change pause, but not a lock: the key
        // is not written back.
        assert_eq!(locked.paused.light, Some(Pause::LockedKey));
        assert_eq!(world.gnome.key_writes().len(), writes_before);
        assert_eq!(world.gnome.key(Style::Light).as_deref(), Some(picked));
        daemon.terminate().await;
    });
}
