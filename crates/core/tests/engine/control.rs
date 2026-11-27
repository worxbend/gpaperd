//! Control commands from `gpaperctl`: each gets exactly one reply.

use std::time::Duration;

use gpaperd_core::catalog::{BadReason, ItemId};
use gpaperd_core::config::{
    ApplyMode, Config, ConfigError, EmptyStylePolicy, ExternalChangePolicy,
};
use gpaperd_core::control::{
    BadItem, Command, HISTORY_LEN, Reply, ReplyId, STATUS_VERSION, StyleTarget,
};
use gpaperd_core::event::{Effect, Event, Pause};
use gpaperd_core::model::Model;
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};

use crate::support::{
    DEADLINE, World, alphabetical, armed, at, cache_loaded, changed, control, fetched, fetches, id,
    init, is_publish, is_set_keys, keys_written, position, probed, published, remote_config,
    removed, replies, reported, running_world, slot_uris, started, the_reply, two_folders,
    two_folders_with, unscanned, user_keys,
};

const NEXT: Command = Command::Next(StyleTarget::Visible);
const PREVIOUS: Command = Command::Previous(StyleTarget::Visible);

fn history(world: &World, style: Style) -> Vec<ItemId> {
    world.model.persisted().styles[style]
        .history
        .iter()
        .cloned()
        .collect()
}

fn is_user_paused(world: &World, style: Style) -> bool {
    world.model.persisted().styles[style].user_paused
}

fn is_reply(effect: &Effect) -> bool {
    matches!(effect, Effect::Reply { .. })
}

fn is_failure(reply: &Reply) -> bool {
    matches!(reply, Reply::Failed(_))
}

#[test]
fn next_rotates_the_visible_style_and_replies_after_the_publish() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, NEXT);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(replies(&trace), [(1, &Reply::Done)]);
    assert!(position(&trace, is_publish) < position(&trace, is_reply));
}

#[test]
fn a_successful_publish_is_announced() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, NEXT);

    let announced = Effect::Changed {
        style: Style::Light,
        item: id("/l/b.jpg"),
    };
    assert!(trace.contains(&announced));
}

#[test]
fn next_restarts_the_interval() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, NEXT);

    assert_eq!(armed(&trace).last(), Some(&Some(at(910))));
}

#[test]
fn next_for_both_replies_once_after_both_publishes() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Both));

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Dark, "/d/b.jpg")]
    );
    assert_eq!(replies(&trace), [(1, &Reply::Done)]);
    let last_publish = trace.iter().rposition(is_publish).unwrap();
    assert!(last_publish < position(&trace, is_reply));
}

#[test]
fn next_for_a_hidden_style_writes_its_slot() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Dark));

    assert_eq!(published(&trace), [(Style::Dark, "/d/b.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert!(world.model.persisted().styles.dark.dirty_while_hidden);
}

#[test]
fn visible_means_the_style_on_screen() {
    let mut world = World::new(two_folders());
    world.start(at(0), Style::Dark);

    let trace = world.ask(at(10), 1, NEXT);

    assert_eq!(published(&trace), [(Style::Dark, "/d/b.jpg")]);
}

#[test]
fn next_with_no_other_image_fails_at_once() {
    let mut world = World::new(init(alphabetical(), &["/l/a.jpg"], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, NEXT);

    assert!(probed(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn next_with_no_other_image_leaves_the_shuffle_round_alone() {
    let mut world = World::new(init(Config::default(), &["/l/a.jpg"], &["/d/a.jpg"]));
    world.start(at(0), Style::Light);
    let before = world.model.persisted().styles.light;

    let trace = world.ask(at(10), 1, NEXT);

    assert!(is_failure(the_reply(&trace)));
    assert_eq!(world.model.persisted().styles.light, before);
}

#[test]
fn next_for_a_style_without_images_fails() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Dark));

    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn next_for_both_fails_when_one_style_cannot_rotate() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg"], &[]));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Both));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    let Reply::Failed(why) = the_reply(&trace) else {
        panic!("expected a failure");
    };
    assert!(why.contains("dark"), "{why}");
}

#[test]
fn a_retry_of_both_does_not_advance_the_style_that_moved() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"], &[]));
    world.start(at(0), Style::Light);
    world.ask(at(10), 1, Command::Next(StyleTarget::Both));

    let retried = world.ask(at(20), 2, Command::Next(StyleTarget::Both));

    assert!(published(&retried).is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
    let Reply::Failed(why) = the_reply(&retried) else {
        panic!("expected a failure");
    };
    assert!(why.contains("dark"), "{why}");
}

#[test]
fn a_single_style_next_after_a_partial_both_still_advances() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"], &[]));
    world.start(at(0), Style::Light);
    world.ask(at(10), 1, Command::Next(StyleTarget::Both));

    let alone = world.ask(at(20), 2, Command::Next(StyleTarget::Light));

    assert_eq!(published(&alone), [(Style::Light, "/l/c.jpg")]);
    assert_eq!(the_reply(&alone), &Reply::Done);
}

#[test]
fn a_second_retry_of_both_advances_the_style_that_moved() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(config, &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg"], &[]));
    world.start(at(0), Style::Light);
    world.ask(at(10), 1, Command::Next(StyleTarget::Both));
    world.ask(at(20), 2, Command::Next(StyleTarget::Both));

    let again = world.ask(at(30), 3, Command::Next(StyleTarget::Both));

    assert_eq!(published(&again), [(Style::Light, "/l/c.jpg")]);
}

#[test]
fn a_retry_of_both_advances_a_style_that_rotated_since() {
    let mut config = alphabetical();
    config.general.empty_style = EmptyStylePolicy::KeepCurrent;
    let mut world = World::new(init(
        config,
        &["/l/a.jpg", "/l/b.jpg", "/l/c.jpg", "/l/d.jpg"],
        &[],
    ));
    world.start(at(0), Style::Light);
    world.ask(at(10), 1, Command::Next(StyleTarget::Both));
    world.run(at(1000), DEADLINE);
    assert_eq!(world.current(Style::Light), Some(id("/l/c.jpg")));

    let retried = world.ask(at(1010), 2, Command::Next(StyleTarget::Both));

    assert_eq!(published(&retried), [(Style::Light, "/l/d.jpg")]);
}

#[test]
fn next_whose_candidates_all_fail_replies_with_a_failure() {
    let mut world = running_world();
    world.broken.insert(id("/l/b.jpg"));
    world.broken.insert(id("/l/c.jpg"));

    let trace = world.ask(at(10), 1, NEXT);

    assert!(published(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn next_waits_for_a_rotation_in_flight() {
    let mut world = running_world();

    let trace = world.run_all(at(900), [DEADLINE, control(1, NEXT)]);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Light, "/l/c.jpg")]
    );
    let last_publish = trace.iter().rposition(is_publish).unwrap();
    assert!(last_publish < position(&trace, is_reply));
}

#[test]
fn two_requests_for_one_style_rotate_twice() {
    let mut world = running_world();

    let trace = world.run_all(at(10), [control(1, NEXT), control(2, NEXT)]);

    assert_eq!(
        published(&trace),
        [(Style::Light, "/l/b.jpg"), (Style::Light, "/l/c.jpg")]
    );
    assert_eq!(replies(&trace), [(1, &Reply::Done), (2, &Reply::Done)]);
}

#[test]
fn every_publish_of_a_new_image_records_the_one_it_replaced() {
    let mut world = running_world();

    world.ask(at(10), 1, NEXT);
    world.ask(at(20), 2, NEXT);

    assert_eq!(
        history(&world, Style::Light),
        [id("/l/a.jpg"), id("/l/b.jpg")]
    );
    assert!(history(&world, Style::Dark).is_empty());
}

#[test]
fn history_keeps_the_newest_entries() {
    let mut world = running_world();

    for n in 0..HISTORY_LEN as u64 + 5 {
        world.ask(at(10 + n), n, NEXT);
    }

    assert_eq!(history(&world, Style::Light).len(), HISTORY_LEN);
}

#[test]
fn previous_steps_back_through_the_history() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);
    world.ask(at(20), 2, NEXT);

    let first = world.ask(at(30), 3, PREVIOUS);
    let second = world.ask(at(40), 4, PREVIOUS);

    assert_eq!(published(&first), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(published(&second), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(the_reply(&second), &Reply::Done);
    assert!(history(&world, Style::Light).is_empty());
}

#[test]
fn next_after_previous_draws_by_the_order() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);
    world.ask(at(20), 2, PREVIOUS);

    let trace = world.ask(at(30), 3, NEXT);

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(history(&world, Style::Light), [id("/l/a.jpg")]);
}

#[test]
fn previous_skips_images_that_left() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);
    world.ask(at(20), 2, NEXT);
    world.run(at(25), changed(Style::Light, vec![removed("/l/b.jpg")]));

    let trace = world.ask(at(30), 3, PREVIOUS);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
}

#[test]
fn previous_skips_an_image_that_fails_its_probe() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);
    world.ask(at(20), 2, NEXT);
    world.broken.insert(id("/l/b.jpg"));

    let trace = world.ask(at(30), 3, PREVIOUS);

    assert_eq!(published(&trace), [(Style::Light, "/l/a.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn previous_with_no_history_fails_and_rotates_nothing() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, PREVIOUS);

    assert!(probed(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn previous_whose_images_all_fail_keeps_the_slot() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);
    world.broken.insert(id("/l/a.jpg"));

    let trace = world.ask(at(20), 2, PREVIOUS);

    assert!(published(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
    assert_eq!(world.current(Style::Light), Some(id("/l/b.jpg")));
}

#[test]
fn history_survives_a_restart() {
    let mut world = running_world();
    world.ask(at(10), 1, NEXT);

    let restarted = world.restart(two_folders());

    assert_eq!(history(&restarted, Style::Light), [id("/l/a.jpg")]);
}

#[test]
fn pause_stops_the_timer_and_is_saved() {
    let mut world = running_world();

    let at_pause = world.ask(at(10), 1, Command::Pause(StyleTarget::Visible));
    let at_deadline = world.run(at(900), DEADLINE);

    assert_eq!(the_reply(&at_pause), &Reply::Done);
    assert!(at_pause.contains(&Effect::Persist));
    assert!(is_user_paused(&world, Style::Light));
    assert_eq!(reported(&at_pause).paused.light, Some(Pause::User));
    assert!(probed(&at_deadline).is_empty());
}

#[test]
fn a_user_pause_survives_a_restart() {
    let mut world = running_world();
    world.ask(at(10), 1, Command::Pause(StyleTarget::Both));

    let mut restarted = world.restart(two_folders());
    let trace = restarted.start(at(5000), Style::Light);

    assert!(probed(&trace).is_empty());
    assert_eq!(reported(&trace).paused.dark, Some(Pause::User));
}

#[test]
fn resume_rotates_at_once_when_overdue() {
    let mut world = running_world();
    world.ask(at(10), 1, Command::Pause(StyleTarget::Light));

    let trace = world.ask(at(1000), 2, Command::Resume(StyleTarget::Light));

    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert!(!is_user_paused(&world, Style::Light));
}

#[test]
fn resume_keeps_a_deadline_that_is_still_ahead() {
    let mut world = running_world();
    world.ask(at(10), 1, Command::Pause(StyleTarget::Light));

    let trace = world.ask(at(500), 2, Command::Resume(StyleTarget::Light));

    assert!(probed(&trace).is_empty());
    assert_eq!(armed(&trace), [Some(at(900))]);
}

#[test]
fn a_reload_does_not_end_a_user_pause() {
    let mut world = running_world();
    world.ask(at(10), 1, Command::Pause(StyleTarget::Light));

    world.run(at(20), Event::ConfigChanged(Ok(alphabetical())));

    assert!(is_user_paused(&world, Style::Light));
}

#[test]
fn resume_ends_an_external_change_pause_and_writes_the_key_back() {
    let mut world = running_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    world.set_key(at(6), Style::Light, "file:///home/u/picked.jpg");

    let trace = world.ask(at(10), 2, Command::Resume(StyleTarget::Light));

    let persisted = world.model.persisted();
    assert!(!persisted.styles.light.paused && !persisted.styles.light.user_paused);
    assert_eq!(reported(&trace).paused.light, None);
    assert_eq!(world.keys.picture_uri, slot_uris().light);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn a_change_after_a_restart_under_a_user_pause_is_an_external_change() {
    let mut world = running_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    let mut restarted = world.restart(two_folders());
    restarted.start(at(10), Style::Light);

    restarted.set_key(at(20), Style::Light, "file:///home/u/picked.jpg");

    let trace = restarted.ask(at(30), 2, Command::Status);
    let Reply::Status(report) = the_reply(&trace) else {
        panic!("expected a status");
    };
    assert_eq!(
        report.styles.light.paused,
        [Pause::User, Pause::ExternalChange]
    );
}

#[test]
fn next_under_a_user_pause_in_uri_mode_writes_the_key() {
    let mut config = alphabetical();
    config.general.apply_mode = ApplyMode::Uri;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));

    let trace = world.ask(at(10), 2, NEXT);

    assert_eq!(world.keys.picture_uri, "file:///l/b.jpg");
    let announced = Effect::Changed {
        style: Style::Light,
        item: id("/l/b.jpg"),
    };
    assert!(trace.contains(&announced));
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn next_after_restore_fails_and_leaves_the_users_keys() {
    let mut world = running_world();
    world.ask(at(5), 1, Command::Restore);

    let trace = world.ask(at(10), 2, NEXT);

    assert!(probed(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
    assert_eq!(world.keys, user_keys());
}

#[test]
fn next_under_an_external_change_pause_fails() {
    let mut world = running_world();
    world.set_key(at(5), Style::Light, "file:///home/u/picked.jpg");

    let trace = world.ask(at(10), 1, NEXT);

    assert!(probed(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn a_rotation_in_flight_when_pause_arrives_is_not_published() {
    let mut world = running_world();
    let at_deadline = step(&mut world.model, at(900), DEADLINE);
    assert!(!probed(&at_deadline).is_empty());

    world.ask(at(901), 1, Command::Pause(StyleTarget::Light));
    let trace = world.run(
        at(902),
        Event::Probed {
            style: Style::Light,
            item: id("/l/b.jpg"),
            verdict: Ok(()),
        },
    );

    assert!(published(&trace).is_empty());
    assert_eq!(world.current(Style::Light), Some(id("/l/a.jpg")));
}

#[test]
fn reload_applies_the_file_and_then_replies() {
    let mut world = running_world();
    let mut config = alphabetical();
    config.styles.light.interval = Duration::from_secs(60);
    world.config_file = Ok(config);

    let trace = world.ask(at(100), 1, Command::Reload);

    assert_eq!(trace[0], Effect::ReadConfig(ReplyId(1)));
    assert_eq!(the_reply(&trace), &Reply::Reloaded(Ok(())));
    // The new interval made light overdue.
    assert_eq!(published(&trace), [(Style::Light, "/l/b.jpg")]);
}

#[test]
fn reload_of_a_broken_file_replies_with_line_and_column() {
    let mut world = running_world();
    world.config_file = Err(ConfigError {
        line: 3,
        column: 7,
        message: "unknown field `intervl`".to_owned(),
    });

    let trace = world.ask(at(100), 1, Command::Reload);

    let Reply::Reloaded(Err(why)) = the_reply(&trace) else {
        panic!("expected a failed reload");
    };
    assert!(why.contains("line 3, column 7"), "{why}");
    assert!(published(&trace).is_empty());
}

#[test]
fn a_config_read_nobody_asked_for_is_ignored() {
    let mut world = running_world();

    let trace = world.run(
        at(100),
        Event::ConfigRead {
            id: ReplyId(9),
            result: Ok(alphabetical()),
        },
    );

    assert!(replies(&trace).is_empty());
}

#[test]
fn refresh_remote_fetches_every_url_again() {
    let a = "https://img.test/a.jpg";
    let b = "https://img.test/b.jpg";
    let mut world = World::new(init(remote_config(&[a], &[b]), &[], &[]));
    world.start(at(0), Style::Light);
    world.run(at(1), cache_loaded(&[]));
    world.run(at(2), fetched(a, gpaperd_core::event::FetchOutcome::Gone));
    world.run(at(2), fetched(b, gpaperd_core::event::FetchOutcome::Gone));

    let trace = world.ask(at(10), 1, Command::RefreshRemote);

    assert_eq!(fetches(&trace), [a, b]);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn refresh_remote_before_the_cache_is_loaded_is_not_done() {
    let a = "https://img.test/a.jpg";
    let b = "https://img.test/b.jpg";
    let mut world = World::new(init(remote_config(&[a], &[b]), &[], &[]));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(1), 1, Command::RefreshRemote);

    assert!(matches!(the_reply(&trace), Reply::Failed(_)));
    assert!(fetches(&trace).is_empty());
}

#[test]
fn an_older_config_read_does_not_replace_a_newer_one() {
    let mut world = running_world();
    step(&mut world.model, at(100), control(1, Command::Reload));
    step(&mut world.model, at(101), control(2, Command::Reload));
    let mut short = alphabetical();
    short.styles.light.interval = Duration::from_secs(60);

    let early = step(
        &mut world.model,
        at(102),
        Event::ConfigRead {
            id: ReplyId(1),
            result: Ok(short.clone()),
        },
    );
    let applied = step(
        &mut world.model,
        at(103),
        Event::ConfigRead {
            id: ReplyId(2),
            result: Ok(short),
        },
    );

    assert!(matches!(the_reply(&early), Reply::Reloaded(Err(_))));
    assert!(probed(&early).is_empty());
    assert_eq!(probed(&applied), [(Style::Light, "/l/b.jpg")]);
    assert!(
        replies(&applied)
            .iter()
            .any(|(id, reply)| { *id == 2 && *reply == &Reply::Reloaded(Ok(())) })
    );
}

#[test]
fn a_failed_newest_config_read_replaces_the_older_ones() {
    let mut world = running_world();
    step(&mut world.model, at(100), control(1, Command::Reload));
    step(&mut world.model, at(101), control(2, Command::Reload));
    let mut short = alphabetical();
    short.styles.light.interval = Duration::from_secs(60);

    let broken = step(
        &mut world.model,
        at(102),
        Event::ConfigRead {
            id: ReplyId(2),
            result: Err(ConfigError {
                line: 1,
                column: 1,
                message: "broken".to_owned(),
            }),
        },
    );
    let late = step(
        &mut world.model,
        at(103),
        Event::ConfigRead {
            id: ReplyId(1),
            result: Ok(short),
        },
    );

    assert!(
        replies(&broken)
            .iter()
            .any(|(id, reply)| { *id == 1 && matches!(reply, Reply::Reloaded(Err(_))) })
    );
    assert!(replies(&late).is_empty());
    assert!(probed(&late).is_empty());
}

#[test]
fn next_is_not_done_before_the_repair_probe() {
    let mut world = running_world();
    let asked = step(&mut world.model, at(10), control(1, NEXT));
    assert_eq!(probed(&asked), [(Style::Light, "/l/b.jpg")]);
    assert!(replies(&asked).is_empty());

    let failed = step(
        &mut world.model,
        at(11),
        Event::Probed {
            style: Style::Light,
            item: id("/l/b.jpg"),
            verdict: Err(BadReason::NotAnImage),
        },
    );

    assert_eq!(probed(&failed), [(Style::Light, "/l/c.jpg")]);
    assert!(replies(&failed).is_empty());
}

#[test]
fn restore_writes_the_original_keys_and_pauses_both_styles() {
    let mut world = running_world();

    let trace = world.ask(at(10), 1, Command::Restore);

    let restore = Effect::RestoreKeys {
        id: ReplyId(1),
        keys: user_keys(),
    };
    assert!(position(&trace, |effect| *effect == restore) < position(&trace, is_reply));
    assert_eq!(the_reply(&trace), &Reply::Done);
    assert!(!trace.iter().any(is_set_keys));
    assert!(is_user_paused(&world, Style::Light) && is_user_paused(&world, Style::Dark));
    assert_eq!(world.keys, user_keys());
}

#[test]
fn restore_whose_write_fails_replies_with_the_error() {
    let mut world = running_world();
    world.restore_error = Some("picture-uri is locked".to_owned());

    let trace = world.ask(at(10), 1, Command::Restore);

    let Reply::Failed(why) = the_reply(&trace) else {
        panic!("expected a failure");
    };
    assert!(why.contains("picture-uri is locked"), "{why}");
}

#[test]
fn shutdown_answers_a_restore_that_waits() {
    let mut world = running_world();
    step(&mut world.model, at(10), control(1, Command::Restore));

    let effects = step(&mut world.model, at(11), Event::Shutdown);

    assert!(matches!(
        replies(&effects).as_slice(),
        [(1, Reply::Failed(_))]
    ));
}

#[test]
fn restored_keys_stay_until_resume() {
    let mut world = running_world();
    world.ask(at(10), 1, Command::Restore);

    let later = world.run(at(5000), DEADLINE);
    let at_resume = world.ask(at(5001), 2, Command::Resume(StyleTarget::Both));

    let slots = slot_uris();
    assert!(!later.iter().any(is_set_keys));
    assert_eq!(
        keys_written(&at_resume)[0].0,
        PerStyle::from_fn(|style| Some(slots[style].as_str()))
    );
}

#[test]
fn status_replies_with_the_report() {
    let mut world = World::new(two_folders());
    world.broken.insert(id("/d/a.jpg"));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Status);

    let Reply::Status(report) = the_reply(&trace) else {
        panic!("expected a status");
    };
    assert_eq!(report.version, STATUS_VERSION);
    assert_eq!(report.visible, Style::Light);
    let next_change = at(900).duration_since(std::time::UNIX_EPOCH).unwrap();
    assert_eq!(report.next_change, Some(next_change.as_secs()));
    assert_eq!(report.styles.light.current, Some(id("/l/a.jpg")));
    assert_eq!(report.styles.dark.items, 3);
    assert_eq!(report.styles.dark.usable, 2);
    let bad = BadItem {
        id: id("/d/a.jpg"),
        reason: BadReason::NotAnImage.to_string(),
    };
    assert_eq!(report.styles.dark.bad, [bad]);
    assert!(report.styles.light.paused.is_empty());
}

#[test]
fn status_lists_every_pause_of_a_style() {
    let mut world = running_world();
    world.ask(at(5), 1, Command::Pause(StyleTarget::Light));
    world.set_key(at(6), Style::Light, "file:///home/u/picked.jpg");

    let trace = world.ask(at(10), 2, Command::Status);

    let Reply::Status(report) = the_reply(&trace) else {
        panic!("expected a status");
    };
    assert_eq!(
        report.styles.light.paused,
        [Pause::User, Pause::ExternalChange]
    );
}

#[test]
fn a_command_before_start_fails_at_once() {
    let mut model = Model::new(two_folders());

    let effects = step(&mut model, at(0), control(1, NEXT));

    assert!(matches!(
        effects.as_slice(),
        [Effect::Reply {
            id: ReplyId(1),
            reply: Reply::Failed(_)
        }]
    ));
}

#[test]
fn a_command_after_shutdown_fails_at_once() {
    let mut world = running_world();
    world.run(at(10), Event::Shutdown);

    let effects = step(&mut world.model, at(11), control(1, Command::Status));

    assert!(matches!(
        effects.as_slice(),
        [Effect::Reply {
            id: ReplyId(1),
            reply: Reply::Failed(_)
        }]
    ));
}

#[test]
fn shutdown_answers_every_request_that_waits() {
    let mut model = Model::new(two_folders());
    step(
        &mut model,
        at(0),
        started(Style::Light, user_keys(), PerStyle::default()),
    );
    // The first apply is in flight, so the request waits.
    step(&mut model, at(1), control(1, NEXT));
    step(&mut model, at(1), control(2, Command::Reload));

    let effects = step(&mut model, at(2), Event::Shutdown);

    let answered: Vec<u64> = replies(&effects).iter().map(|(n, _)| *n).collect();
    assert_eq!(answered, [1, 2]);
    assert!(replies(&effects).iter().all(|(_, reply)| is_failure(reply)));
}

#[test]
fn a_style_without_a_key_cannot_rotate_on_request() {
    let mut keys = user_keys();
    keys.picture_uri_dark = None;
    let mut world = World::new(two_folders());
    world.keys = keys;
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, Command::Next(StyleTarget::Dark));

    assert!(probed(&trace).is_empty());
    assert!(is_failure(the_reply(&trace)));
}

#[test]
fn next_before_the_first_scan_waits_for_it() {
    let mut world = World::new(unscanned(alphabetical()));
    world.start(at(0), Style::Light);

    let asked = world.ask(at(5), 1, NEXT);
    let scanned = world.run(
        at(6),
        crate::support::scanned(Style::Light, &["/l/a.jpg", "/l/b.jpg"]),
    );

    // Nothing to pick from before the scan: the reply says so.
    assert!(matches!(the_reply(&asked), Reply::Failed(_)));
    assert!(replies(&scanned).is_empty());
}

#[test]
fn uri_mode_announces_the_image_with_its_key_write() {
    let mut config = alphabetical();
    config.general.apply_mode = ApplyMode::Uri;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);

    let trace = world.ask(at(10), 1, NEXT);

    let announced = Effect::Changed {
        style: Style::Light,
        item: id("/l/b.jpg"),
    };
    let announced_at = position(&trace, |effect| *effect == announced);
    assert!(position(&trace, is_set_keys) < announced_at);
    assert_eq!(the_reply(&trace), &Reply::Done);
}

#[test]
fn next_in_uri_mode_does_not_wait_for_a_key_written_later() {
    let mut config = alphabetical();
    config.general.apply_mode = ApplyMode::Uri;
    config.general.on_external_change = ExternalChangePolicy::ReassertNow;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    world.set_key(at(5), Style::Light, "file:///home/u/Pictures/picked.jpg");

    let trace = world.ask(at(5), 1, NEXT);

    let Reply::Failed(why) = the_reply(&trace) else {
        panic!("expected a failure");
    };
    assert!(why.contains("written later"), "{why}");
}

#[test]
fn next_writes_back_a_key_that_waits_for_the_next_tick() {
    let mut config = alphabetical();
    config.general.on_external_change = ExternalChangePolicy::ReassertNextTick;
    let mut world = World::new(two_folders_with(config));
    world.start(at(0), Style::Light);
    world.set_key(at(100), Style::Light, "file:///home/u/Pictures/picked.jpg");

    let trace = world.ask(at(200), 1, NEXT);

    assert_eq!(the_reply(&trace), &Reply::Done);
    assert_eq!(world.keys.picture_uri, slot_uris().light);
}
