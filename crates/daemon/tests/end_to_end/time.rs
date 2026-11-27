//! A wall clock that does not run evenly: the Time rows of the plan's
//! corner-case table.

use gpaperd_core::style::Style;

use crate::world::{
    ALPHABETICAL, LIGHT_INTERVAL, MINUTE, SEED, World, eventually, scenario, start_time,
};

#[test]
fn a_clock_that_went_back_starts_a_fresh_interval() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        let an_hour_earlier = start_time() - 60 * MINUTE;
        world.clock.step_to(an_hour_earlier);
        let status = daemon.next_status().await;

        let fresh_deadline = an_hour_earlier + LIGHT_INTERVAL;
        assert_eq!(status.next_change, Some(fresh_deadline));
        assert_eq!(world.clock.armed(), Some(fresh_deadline));
        assert_eq!(world.shown(Style::Light), "a.png");
    });
}

#[test]
fn a_deadline_that_passed_while_the_machine_slept_fires_on_resume() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        // Three hours pass at once, as they do over a suspend.
        world.clock.advance(180 * MINUTE);
        let status = world.showing(&mut daemon, "b.png", "x.png").await;

        // One rotation, not one per interval that was slept through.
        let woke_up = start_time() + 180 * MINUTE;
        assert_eq!(status.next_change, Some(woke_up + LIGHT_INTERVAL));
        assert_eq!(world.shown(Style::Light), "b.png");
    });
}

#[test]
fn a_clock_stepped_past_the_deadline_rotates_at_once() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;

        // The alarm does not go off: the daemon is only told of the step.
        let stepped_to = start_time() + LIGHT_INTERVAL + MINUTE;
        world.clock.step_to(stepped_to);
        let status = world.showing(&mut daemon, "b.png", "x.png").await;

        assert_eq!(status.next_change, Some(stepped_to + LIGHT_INTERVAL));
        assert_eq!(world.clock.armed(), Some(stepped_to + LIGHT_INTERVAL));
    });
}

#[test]
fn a_clock_step_that_changes_no_deadline_arms_the_alarm_again() {
    scenario(async {
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut daemon = world.start_daemon(world.config(ALPHABETICAL), SEED);
        world.showing(&mut daemon, "a.png", "x.png").await;
        let armed_before = world.clock.times_armed();

        world.clock.step_to(start_time() + MINUTE);
        eventually("the alarm to be armed again", || {
            world.clock.times_armed() == armed_before + 1
        })
        .await;

        assert_eq!(world.clock.armed(), Some(start_time() + LIGHT_INTERVAL));
        assert_eq!(world.shown(Style::Light), "a.png");
    });
}

#[test]
fn a_weekly_interval_outlives_a_restart_and_fires_after_a_suspend() {
    scenario(async {
        let week = 7 * 24 * 60 * MINUTE;
        let weekly = "order = \"alphabetical\"\ninterval = \"7d\"";
        let world = World::new();
        world.add_images(Style::Light, &["a.png", "b.png"]);
        world.add_images(Style::Dark, &["x.png"]);
        let mut first_run = world.start_daemon(world.config(weekly), SEED);
        world.showing(&mut first_run, "a.png", "x.png").await;
        first_run.terminate().await;

        // Three days pass while the daemon is down.
        world.clock.advance(3 * 24 * 60 * MINUTE);
        let mut second_run = world.start_daemon(world.config(weekly), SEED);
        let first_deadline = start_time() + week;
        second_run
            .status_where("the saved weekly deadline", |status| {
                status.next_change == Some(first_deadline)
            })
            .await;
        assert_eq!(world.clock.armed(), Some(first_deadline));
        assert_eq!(world.shown(Style::Light), "a.png");

        // The machine sleeps through the deadline and wakes a day after it.
        world.clock.advance(5 * 24 * 60 * MINUTE);
        let status = world.showing(&mut second_run, "b.png", "x.png").await;

        let woke_up = start_time() + 8 * 24 * 60 * MINUTE;
        assert_eq!(status.next_change, Some(woke_up + week));
    });
}
