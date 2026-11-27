//! Image files and slot files that change while the daemon runs: the Local
//! and Slots rows of the plan's corner-case table.

use std::fs;

use gpaperd_core::step::PROBE_RETRY_DELAY;
use gpaperd_core::style::Style;

use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, SEED, World, eventually, has_a_deadline, inode, scenario,
    start_time, users_keys, write_image,
};

/// What the daemon logs when one image file left light's sources.
const ONE_LIGHT_FILE_REMOVED: &str = "image files changed style=light found=0 removed=1";

/// A `[general]` table with a predictable order, in which a style without
/// images does not turn to the other style's.
const ALPHABETICAL_KEEP_CURRENT: &str = "order = \"alphabetical\"\nempty_style = \"keep-current\"";

#[test]
fn an_image_added_while_running_joins_the_rotation() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        let with_one_image = world.showing(&mut daemon, "a.png", "x.png").await;

        world.add_images(Style::Light, &["b.png"]);
        let with_two_images = daemon.next_status().await;
        world.clock.advance(LIGHT_INTERVAL);
        daemon.next_status().await;

        assert_eq!(with_one_image.next_change, None);
        assert_eq!(
            with_two_images.next_change,
            Some(start_time() + LIGHT_INTERVAL)
        );
        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn the_image_on_screen_deleted_changes_nothing_until_the_next_tick() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let slot = world.slot_inode(Style::Light);

        fs::remove_file(world.image(Style::Light, "a.png")).expect("delete the image");
        world.log_line(ONE_LIGHT_FILE_REMOVED).await;
        let slot_after_the_loss = world.slot_inode(Style::Light);
        let shown_after_the_loss = world.shown(Style::Light);
        world.clock.advance(LIGHT_INTERVAL);
        let after_the_tick = daemon.next_status().await;

        assert_eq!(slot_after_the_loss, slot);
        assert_eq!(shown_after_the_loss, "a.png");
        assert_eq!(world.shown(Style::Light), "b.png");
        // The deleted image left the catalog: with one image nothing rotates.
        assert_eq!(after_the_tick.next_change, None);
    });
}

#[test]
fn the_image_on_screen_edited_in_place_is_published_again_on_a_new_inode() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let image = world.image(Style::Light, "a.png");
        let slot_before = world.slot_inode(Style::Light);
        let image_before = inode(&image);

        // Truncates and writes the same file.
        write_image(&image, "a.png, edited");
        eventually("the slot to get a new inode", || {
            world.slot_inode(Style::Light) != slot_before
        })
        .await;

        assert_eq!(inode(&image), image_before);
        assert_ne!(world.slot_inode(Style::Light), image_before);
        assert_eq!(world.shown(Style::Light), "a.png, edited");
    });
}

#[test]
fn the_image_on_screen_replaced_by_an_atomic_save_is_published_again() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let image = world.image(Style::Light, "a.png");

        let draft = world.image(Style::Light, "a.png.part");
        write_image(&draft, "a.png, saved");
        fs::rename(&draft, &image).expect("save the image by rename");
        let saved = inode(&image);
        // The slot links to the source where it can, so it gets its inode.
        eventually("the slot to hold the saved file", || {
            world.slot_inode(Style::Light) == saved
        })
        .await;

        assert_eq!(world.shown(Style::Light), "a.png, saved");
    });
}

#[test]
fn a_source_directory_created_after_the_start_fills_the_slot() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Dark, &["x.png"]);
        let later = world.root().join("images/later");
        let config = world.parse(&world.config_text_reading(ALPHABETICAL_KEEP_CURRENT, &later));
        let mut daemon = world.start_daemon(config, SEED);
        let without_the_directory = world.showing(&mut daemon, None, "x.png").await;

        fs::create_dir(&later).expect("create the source directory");
        write_image(&later.join("a.png"), "a.png");
        daemon
            .status_where("an image in the light slot", |status| {
                status.current.light.is_some()
            })
            .await;

        assert_eq!(without_the_directory.current.light, None);
        assert_eq!(world.shown(Style::Light), "a.png");
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
    });
}

#[test]
fn a_source_directory_that_is_removed_keeps_the_slot_and_is_followed_when_it_is_back() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL_KEEP_CURRENT), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let slot = world.slot_inode(Style::Light);

        fs::remove_dir_all(world.images_dir(Style::Light)).expect("remove the source directory");
        world.log_line(ONE_LIGHT_FILE_REMOVED).await;
        let slot_without_the_directory = world.slot_inode(Style::Light);
        let shown_without_the_directory = world.shown(Style::Light);
        fs::create_dir(world.images_dir(Style::Light)).expect("create the source directory");
        world.add_images(Style::Light, &["c.png"]);
        daemon
            .status_where("a deadline for the image that came back", has_a_deadline)
            .await;
        world.clock.advance(LIGHT_INTERVAL);
        daemon.next_status().await;

        assert_eq!(slot_without_the_directory, slot);
        assert_eq!(shown_without_the_directory, "a.png");
        assert_eq!(world.shown(Style::Light), "c.png");
    });
}

#[test]
fn a_deleted_slot_file_is_restored() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        // As on every start but the very first, the slots directory is
        // there. Then the watch is on it from the start, and it sees the
        // slot come before it sees the slot go.
        fs::create_dir_all(world.paths().slots_dir).expect("create the slots directory");
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let slot = world.paths().slot(Style::Light);

        fs::remove_file(&slot).expect("delete the slot file");
        eventually("the slot file to be back", || slot.exists()).await;

        assert_eq!(world.shown(Style::Light), "a.png");
        assert!(world.logged("slot file is gone"));
    });
}

#[test]
fn an_image_above_max_pixels_is_skipped() {
    if !crate::world::glycin_decodes() {
        return;
    }
    scenario(async {
        let world = World::decoding_with_glycin();
        world.add_huge_image(Style::Light, "a.png");
        world.add_images(Style::Light, &["b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(world.shown(Style::Light), "b.png");
        assert!(world.logged("30000x30000 pixels is more than max_pixels allows"));
    });
}

#[test]
fn an_image_that_is_still_copied_in_is_shown_once_it_is_whole() {
    if !crate::world::glycin_decodes() {
        return;
    }
    scenario(async {
        let world = World::decoding_with_glycin();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        world.add_unfinished_image(Style::Light, "b.png");
        daemon
            .status_where("a deadline for the new file", has_a_deadline)
            .await;
        world.clock.advance(LIGHT_INTERVAL);
        // The file is cut off, so it waits for its next check. Until then
        // there is nothing to rotate to.
        daemon
            .status_where("the rotation to find the file unfinished", |status| {
                !has_a_deadline(status)
            })
            .await;
        let next_check = world.clock.armed();
        let shown_while_unfinished = world.shown(Style::Light);
        // The copy ends.
        world.add_images(Style::Light, &["b.png"]);
        daemon
            .status_where("a deadline for the whole file", has_a_deadline)
            .await;
        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(
            next_check,
            Some(start_time() + LIGHT_INTERVAL + PROBE_RETRY_DELAY)
        );
        assert_eq!(shown_while_unfinished, "a.png");
        assert!(world.logged("image is incomplete"));
        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn styles_without_any_image_keep_the_desktop_and_warn() {
    scenario(async {
        let world = World::new();
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        let status = daemon.ready().await;
        world.log_line("no image found in any source").await;

        assert_eq!(status.next_change, None);
        assert_eq!(world.gnome.keys(), users_keys());
        assert!(world.gnome.key_writes().is_empty());
        assert!(!world.paths().slots_dir.exists());
    });
}

#[test]
fn temp_files_of_a_crashed_run_are_swept_at_the_start() {
    scenario(async {
        let world = World::new();
        let slots_dir = world.paths().slots_dir;
        let leftover = slots_dir.join(".light.tmp-4242");
        fs::create_dir_all(&slots_dir).expect("create the slots directory");
        fs::write(&leftover, "half a file").expect("write the leftover");
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);

        daemon.ready().await;

        assert!(!leftover.exists());
    });
}
