//! gpaperd's tile in the wallpaper picker of GNOME Settings: the file that
//! Settings reads, and the keys it writes when the user picks the tile.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use gpaperd_core::event::{BackgroundKeys, Pause};
use gpaperd_core::style::Style;

use crate::world::{ALPHABETICAL, SEED, World, eventually, scenario};

const URI_MODE: &str = "order = \"alphabetical\"\napply_mode = \"uri\"";

/// The text of the element `name` in the tile file: Settings reads it as
/// a file name. The test paths need no unescaping.
fn element(tile: &str, name: &str) -> PathBuf {
    let open = format!("<{name}>");
    let start = tile.find(&open).expect("the element in the tile") + open.len();
    let length = tile[start..].find('<').expect("the end of the element");
    PathBuf::from(&tile[start..start + length])
}

/// The URI that Settings makes of a file name of the tile: GLib's
/// `g_filename_to_uri` escapes every byte but the unreserved ones and
/// `!$&'()*+,;=:@/`. That is more than gpaperd's slot URIs escape.
fn uri_as_settings_makes_it(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[test]
fn a_start_in_slots_mode_offers_both_slots_in_settings() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let tile = world.paths().settings_tile;
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        eventually("the Settings tile", || tile.exists()).await;

        let text = fs::read_to_string(&tile).expect("read the tile");
        assert_eq!(element(&text, "filename"), world.paths().slot(Style::Light));
        assert_eq!(
            element(&text, "filename-dark"),
            world.paths().slot(Style::Dark)
        );
        assert_eq!(element(&text, "options"), PathBuf::from("zoom"));
    });
}

#[test]
fn a_reload_into_uri_mode_removes_the_tile() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let tile = world.paths().settings_tile;
        world.save_config_file(&world.config_text(ALPHABETICAL));
        let mut daemon = world.start_daemon_from_config_file(SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        eventually("the Settings tile", || tile.exists()).await;

        world.save_config_file(&world.config_text(URI_MODE));

        eventually("the Settings tile to go", || !tile.exists()).await;
        world.log_line("Settings tile removed").await;
    });
}

#[test]
fn picking_the_tile_in_settings_ends_the_external_change_pause() {
    scenario(async {
        // GLib escapes `[`, `]` and `|`; the slot URIs keep them.
        let world = World::in_a_folder_named("gpaperd[tile]|");
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png", "y.png"]);
        let tile = world.paths().settings_tile;
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        eventually("the Settings tile", || tile.exists()).await;
        world.gnome.set_keys_from_outside(BackgroundKeys {
            picture_uri: "file:///user/other.png".to_owned(),
            picture_uri_dark: Some("file:///user/other-dark.png".to_owned()),
            picture_options: "scaled".to_owned(),
        });
        let paused = daemon
            .status_where("both styles paused", |status| {
                status.paused.light.is_some() && status.paused.dark.is_some()
            })
            .await;

        // Settings turns each file name of the tile into a URI.
        let text = fs::read_to_string(&tile).expect("read the tile");
        let uri = |name| uri_as_settings_makes_it(&element(&text, name));
        world.gnome.set_keys_from_outside(BackgroundKeys {
            picture_uri: uri("filename"),
            picture_uri_dark: Some(uri("filename-dark")),
            picture_options: element(&text, "options").display().to_string(),
        });
        let resumed = daemon
            .status_where("no pause", |status| {
                status.paused.light.is_none() && status.paused.dark.is_none()
            })
            .await;

        assert_ne!(uri("filename"), world.slot_uri(Style::Light));
        assert_eq!(paused.paused.light, Some(Pause::ExternalChange));
        assert_eq!(paused.paused.dark, Some(Pause::ExternalChange));
        assert_eq!(resumed.paused.light, None);
        assert_eq!(resumed.paused.dark, None);
        assert!(!world.saved_state().styles.light.paused);
        assert!(!world.saved_state().styles.dark.paused);
    });
}
