//! `on_change`: the user's command runs when the image on screen changes.

use std::fs;
use std::path::Path;

use gpaperd_core::style::Style;

use crate::world::{ALPHABETICAL, LIGHT_INTERVAL, SEED, World, eventually, scenario};

/// Appends one line per run to the file in `$0`: the event, the style,
/// whether it is visible, the item and the path.
const APPEND_RUN: &str = r#"printf '%s %s %s %s %s\n' "$GPAPERD_EVENT" "$GPAPERD_STYLE" "$GPAPERD_VISIBLE" "$GPAPERD_ID" "$GPAPERD_PATH" >> "$0""#;

/// A `[general]` table in alphabetical order whose `on_change` appends each
/// run to `runs`.
fn hooked_general(runs: &Path) -> String {
    format!("{ALPHABETICAL}\non_change = [\"/bin/sh\", \"-c\", {APPEND_RUN:?}, {runs:?}]")
}

/// The lines of the runs file, in the order the runs wrote them.
fn runs_logged(runs: &Path) -> Vec<String> {
    fs::read_to_string(runs)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_hook_runs_for_a_rotation_and_a_switch_and_not_for_a_restart() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["night.png"]);
        let runs = world.root().join("hook-runs");
        let config = world.config(&hooked_general(&runs));
        let line = |event: &str, style: Style, visible: u8, name: &str| {
            let image = world.image(style, name);
            let image = image.display();
            format!("{event} {style} {visible} {image} {image}")
        };

        let mut first_run = world.start_daemon(config.clone(), SEED);
        world.showing(&mut first_run, "a.png", "night.png").await;
        eventually("a run per style", || runs_logged(&runs).len() == 2).await;
        let mut at_start = runs_logged(&runs);
        at_start.sort();
        assert_eq!(
            at_start,
            [
                line("changed", Style::Dark, 0, "night.png"),
                line("changed", Style::Light, 1, "a.png"),
            ]
        );

        world.clock.advance(LIGHT_INTERVAL);
        world.showing(&mut first_run, "b.png", "night.png").await;
        eventually("the rotation's run", || runs_logged(&runs).len() == 3).await;
        assert_eq!(
            runs_logged(&runs)[2],
            line("changed", Style::Light, 1, "b.png")
        );

        world.gnome.switch_style(Style::Dark);
        eventually("the switch's run", || runs_logged(&runs).len() == 4).await;
        assert_eq!(
            runs_logged(&runs)[3],
            line("switched", Style::Dark, 1, "night.png")
        );
        first_run.terminate().await;

        // The slots are intact: a run started now would be the restart's.
        let mut second_run = world.start_daemon(config, SEED);
        second_run.ready().await;
        world.gnome.switch_style(Style::Light);
        eventually("the second switch's run", || runs_logged(&runs).len() >= 5).await;
        assert_eq!(
            runs_logged(&runs)[4..],
            [line("switched", Style::Light, 1, "b.png")]
        );
        second_run.terminate().await;
    });
}
