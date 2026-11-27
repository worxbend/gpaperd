//! Properties of the reducer over generated event sequences.
//!
//! A [`Daemon`] plays the interpreter and the world around it: it turns each
//! generated [`Op`](scenario::Op) into events, feeds them to `step`, and
//! checks every effect that comes back.

mod control;
mod daemon;
mod remote;
mod scenario;

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::{BadReason, FileId, FsChange, ItemId, LocalFile};
use gpaperd_core::config::{
    ApplyMode, Config, ConfigError, PictureOptions, RemoteRefresh, Source,
};
use gpaperd_core::control::ReplyId;
use gpaperd_core::event::{
    BackgroundKeys, Connectivity, Effect, Event, FetchOutcome, Fired, GnomeEvent, Method,
    ScreenLock, SlotWrite,
};
use gpaperd_core::model::StylePersisted;
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use self::control::command;
use self::daemon::{Daemon, local_files, new_model};
use self::scenario::{IMAGES, Scenario, config, image, scenario, style};
use crate::support::{
    at, blob, id, is_set_keys, keys_changed, slot_uris, started_with_locks, url, user_keys,
    version, write_keys,
};

/// The URLs that a generated reload may list.
const REMOTE_URLS: [&str; 3] = [
    "https://img.test/a.jpg",
    "https://img.test/b.jpg",
    "http://plain.test/c.jpg",
];

/// Plays a scenario and returns the effects of every step. Fails when any
/// effect breaks an invariant.
fn play(scenario: &Scenario) -> Result<Vec<Vec<Effect>>, TestCaseError> {
    let mut daemon = Daemon::boot(scenario)?;
    for op in &scenario.ops {
        daemon.apply(op)?;
    }
    daemon.settle()?;
    prop_assert!(daemon.is_ready, "the first apply never settled");
    daemon.shut_down()?;
    Ok(daemon.trace)
}

/// Whether `part` can be made from `whole` by leaving items out.
fn is_subsequence(part: &[ItemId], whole: &[ItemId]) -> bool {
    let mut rest = whole.iter();
    part.iter().all(|wanted| rest.any(|item| item == wanted))
}

/// An image id an answer may name: one of [`IMAGES`], or a file no scan found.
fn any_item() -> impl Strategy<Value = ItemId> {
    let mut paths = IMAGES.to_vec();
    paths.push("/gone/z.jpg");
    prop::sample::select(paths).prop_map(id)
}

fn any_slot() -> impl Strategy<Value = Option<FileId>> {
    prop::option::of((0_u64..3).prop_map(|ino| FileId { dev: 2, ino }))
}

/// What a key may hold: the user's wallpaper, the daemon's slot, or an image
/// that the daemon shows in uri mode.
fn any_key(style: Style) -> impl Strategy<Value = String> {
    let slot_uri = slot_uris()[style].clone();
    let user_uri = user_keys().picture_uri;
    prop_oneof![
        Just(slot_uri),
        Just(user_uri),
        image().prop_map(|image| format!("file://{image}")),
    ]
}

/// Any background keys, also those of a GNOME without a dark key.
fn any_keys() -> impl Strategy<Value = BackgroundKeys> {
    let picture_options = prop::sample::select(vec!["zoom", "none"]);
    let dark_key = prop::option::weighted(0.9, any_key(Style::Dark));
    (any_key(Style::Light), dark_key, picture_options).prop_map(
        |(picture_uri, picture_uri_dark, picture_options)| BackgroundKeys {
            picture_uri,
            picture_uri_dark,
            picture_options: picture_options.to_owned(),
        },
    )
}

fn any_url() -> impl Strategy<Value = &'static str> {
    prop::sample::select(REMOTE_URLS.to_vec())
}

fn any_remote_refresh() -> impl Strategy<Value = RemoteRefresh> {
    prop_oneof![
        Just(RemoteRefresh::Startup),
        Just(RemoteRefresh::Never),
        (3600_u64..9000).prop_map(|secs| RemoteRefresh::Every(Duration::from_secs(secs))),
    ]
}

/// Any config, in either apply mode, with some URLs among its sources.
fn any_config() -> impl Strategy<Value = Config> {
    let urls = || prop::sample::subsequence(REMOTE_URLS.to_vec(), 0..=REMOTE_URLS.len());
    let remote = (urls(), urls(), any_remote_refresh());
    (config(), any::<bool>(), remote).prop_map(|(mut config, shows_by_uri, remote)| {
        if shows_by_uri {
            config.general.apply_mode = ApplyMode::Uri;
        }
        let (light, dark, remote_refresh) = remote;
        let sources = |urls: Vec<&'static str>| urls.into_iter().map(|text| Source::Url(url(text)));
        config.styles.light.sources.extend(sources(light));
        config.styles.dark.sources.extend(sources(dark));
        config.general.remote_refresh = remote_refresh;
        config
    })
}

/// Any answer of the fetcher, for a URL that may or may not be fetched.
fn any_fetched() -> impl Strategy<Value = Event> {
    let outcome = |text: &'static str| {
        prop_oneof![
            (1_u32..4).prop_map(move |n| FetchOutcome::Stored {
                blob: blob(text, n).blob
            }),
            Just(FetchOutcome::NotModified),
            Just(FetchOutcome::Gone),
            (0_u64..5000).prop_map(|secs| FetchOutcome::Failed {
                retry_in: Duration::from_secs(secs)
            }),
            (0_u64..5000).prop_map(|secs| FetchOutcome::Unreachable {
                retry_in: Duration::from_secs(secs)
            }),
        ]
    };
    any_url().prop_flat_map(move |text| {
        outcome(text).prop_map(move |outcome| Event::Fetched {
            url: url(text),
            outcome,
        })
    })
}

fn any_cache() -> impl Strategy<Value = Event> {
    let cached = prop::collection::vec((any_url(), 1_u32..4), 0..4);
    cached.prop_map(|cached| {
        let blobs = cached.into_iter().map(|(text, n)| blob(text, n));
        Event::CacheLoaded(blobs.collect())
    })
}

fn any_connectivity() -> impl Strategy<Value = Event> {
    prop_oneof![Just(Connectivity::Full), Just(Connectivity::Limited)]
        .prop_map(|level| Event::Gnome(GnomeEvent::ConnectivityChanged(level)))
}

fn any_reload() -> impl Strategy<Value = Result<Config, ConfigError>> {
    let mistake = ConfigError {
        line: 1,
        column: 1,
        message: "unknown key".to_owned(),
    };
    prop_oneof![
        4 => any_config().prop_map(Ok),
        1 => Just(Err(mistake)),
    ]
}

fn any_slots() -> impl Strategy<Value = PerStyle<Option<FileId>>> {
    (any_slot(), any_slot()).prop_map(|(light, dark)| PerStyle { light, dark })
}

fn any_locks() -> impl Strategy<Value = PerStyle<bool>> {
    (any::<bool>(), any::<bool>()).prop_map(|(light, dark)| PerStyle { light, dark })
}

/// A file of any image, in one of three versions.
fn any_file() -> impl Strategy<Value = LocalFile> {
    (image(), 0_u64..3).prop_map(|(image, saves)| version(image, saves))
}

fn any_change() -> impl Strategy<Value = FsChange> {
    prop_oneof![
        any_file().prop_map(FsChange::Found),
        image().prop_map(|image| FsChange::Removed(PathBuf::from(image))),
    ]
}

fn any_picture_options() -> impl Strategy<Value = Option<PictureOptions>> {
    prop_oneof![
        Just(None),
        Just(Some(PictureOptions::Wallpaper)),
        Just(Some(PictureOptions::Centered)),
        Just(Some(PictureOptions::Scaled)),
        Just(Some(PictureOptions::Stretched)),
        Just(Some(PictureOptions::Zoom)),
        Just(Some(PictureOptions::Spanned)),
    ]
}

fn any_failed_uris() -> impl Strategy<Value = PerStyle<Option<String>>> {
    (
        prop::option::of(any_key(Style::Light)),
        prop::option::of(any_key(Style::Dark)),
    )
        .prop_map(|(light, dark)| PerStyle { light, dark })
}

/// Any event at all: answers nobody asked for, a second `Started`, file
/// changes before a scan, reloads into the other apply mode, fetch answers
/// and cache reports in any order, control requests and config reads with
/// any id, key locks and screen locks that come and go, a refused key write,
/// events after `Shutdown`.
fn any_event() -> impl Strategy<Value = Event> {
    let verdict = prop_oneof![
        Just(Ok(())),
        Just(Err(BadReason::Missing)),
        Just(Err(BadReason::NotRegularFile)),
        Just(Err(BadReason::Unreadable("permission denied".to_owned()))),
        Just(Err(BadReason::NotAnImage)),
        Just(Err(BadReason::Truncated)),
        Just(Err(BadReason::Undecodable("no loader".to_owned()))),
        Just(Err(BadReason::TooLarge {
            width: 20_000,
            height: 10_000
        })),
    ];
    let written = || {
        any_slot().prop_map(|slot| match slot {
            Some(slot) => Ok(SlotWrite {
                method: Method::Copy,
                slot,
            }),
            None => Err("Permission denied".to_owned()),
        })
    };
    let scan = prop::collection::vec(any_file(), 0..8);
    let changes = prop::collection::vec(any_change(), 0..4);
    prop_oneof![
        2 => (style(), any_keys(), any_slots(), any_locks())
            .prop_map(|(visible, keys, slots, locked)| started_with_locks(visible, keys, slots, locked)),
        3 => Just(Event::Fired(Fired::Deadline)),
        1 => Just(Event::Fired(Fired::ClockChanged)),
        6 => (style(), any_item(), verdict)
            .prop_map(|(style, item, verdict)| Event::Probed { style, item, verdict }),
        6 => (style(), any_item(), written())
            .prop_map(|(style, item, result)| Event::Published { style, item, result }),
        2 => (style(), written()).prop_map(|(style, result)| Event::Republished { style, result }),
        2 => any_reload().prop_map(Event::ConfigChanged),
        2 => (style(), scan).prop_map(|(style, files)| Event::Scanned { style, files }),
        3 => (style(), changes).prop_map(|(style, changes)| Event::Fs { style, changes }),
        1 => style().prop_map(Event::SlotLost),
        2 => style().prop_map(|style| Event::Gnome(GnomeEvent::StyleChanged(style))),
        4 => any_keys().prop_map(keys_changed),
        2 => any_cache(),
        4 => any_fetched(),
        1 => any_connectivity(),
        1 => any_locks().prop_map(|locks| Event::Gnome(GnomeEvent::LocksChanged(locks))),
        1 => prop_oneof![Just(ScreenLock::Locked), Just(ScreenLock::Unlocked)].prop_map(Event::ScreenLock),
        2 => (0_u64..4, command()).prop_map(|(n, command)| Event::Control { id: ReplyId(n), command }),
        1 => (0_u64..4, any_reload()).prop_map(|(n, result)| Event::ConfigRead { id: ReplyId(n), result }),
        1 => (0_u64..4, any::<bool>()).prop_map(|(n, wrote)| Event::KeysRestored {
            id: ReplyId(n),
            result: if wrote {
                Ok(())
            } else {
                Err("the key is locked".to_owned())
            },
        }),
        1 => (any_failed_uris(), any_picture_options()).prop_map(|(uris, picture_options)| {
            Event::KeyWriteFailed {
                uris,
                picture_options,
            }
        }),
        1 => Just(Event::Shutdown),
    ]
}

/// Any wall-clock time: around the epoch, around the test timeline, and so
/// late that adding an interval overflows.
fn any_time() -> impl Strategy<Value = SystemTime> {
    let after_epoch = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    let last_secs = i64::MAX.unsigned_abs();
    prop_oneof![
        2 => (0_u64..5000).prop_map(after_epoch),
        6 => (0_u64..20_000).prop_map(at),
        1 => (0_u64..5000).prop_map(move |secs| after_epoch(last_secs - secs)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `step` is total: no event, order or time makes it panic, and nothing
    /// follows `Shutdown`. In uri mode no step touches a slot file. In
    /// either mode the timer is never armed for the past, and the report of
    /// the daemon's own key write is not answered with another write:
    /// against a real timer or a real GNOME either would never stop.
    #[test]
    fn any_event_at_any_time_is_handled(
        scenario in scenario(),
        events in prop::collection::vec((any_time(), any_event()), 0..60),
    ) {
        let boot = &scenario.first_boot;
        let scan = |style: Style| {
            let found = boot.folders.of(style).iter();
            found.map(|image| version(image, 0)).collect()
        };
        let files = PerStyle::from_fn(|style| boot.scanned_up_front.then(|| scan(style)));
        let mut model = new_model(&scenario.config, files, boot.seed, None);
        let mut is_stopped = false;
        // GNOME's keys, as far as the daemon was told of them.
        let mut gnome_keys = user_keys();
        let mut is_started = false;

        for (now, event) in events {
            let stops = event == Event::Shutdown;
            match &event {
                // A second `Started` is ignored, and its keys with it.
                Event::Started { gnome, .. } if !is_started => {
                    gnome_keys = gnome.keys.clone();
                    is_started = true;
                }
                Event::Gnome(GnomeEvent::KeysChanged(keys)) => gnome_keys = keys.clone(),
                _ => {}
            }
            let effects = step(&mut model, now, event);
            // A timer that is armed for the past fires at once, and so would
            // the next one: the daemon would spin.
            let is_armed_for_the_past = |effect: &Effect| {
                matches!(effect, Effect::Arm(Some(deadline)) if *deadline <= now)
            };
            prop_assert!(
                !effects.iter().any(is_armed_for_the_past),
                "armed for a time that has passed"
            );
            if effects.iter().any(is_set_keys) {
                for effect in &effects {
                    write_keys(&mut gnome_keys, effect);
                }
                let answer = step(&mut model, now, keys_changed(gnome_keys.clone()));
                prop_assert!(
                    !answer.iter().any(is_set_keys),
                    "the report of the daemon's own key write was answered with another write"
                );
            }

            let is_reply = |effect: &Effect| matches!(effect, Effect::Reply { .. });
            prop_assert!(
                !is_stopped || effects.iter().all(is_reply),
                "effects other than replies after shutdown"
            );
            is_stopped |= stops;
            let shows_by_uri = model.config().general.apply_mode == ApplyMode::Uri;
            let touches_a_slot =
                |effect: &Effect| matches!(effect, Effect::Publish { .. } | Effect::Republish(_));
            prop_assert!(
                !shows_by_uri || !effects.iter().any(touches_a_slot),
                "a slot was written in uri mode"
            );
        }
    }

    /// Whatever happens: only an image that is there is published, in the
    /// version that passed its probe, and never after the daemon learned
    /// that it left. A shuffled style shows no image twice in one round,
    /// whatever comes and goes. A hidden slot is written only for a repair,
    /// and a dirty slot is republished exactly once after its style came on
    /// screen. The timer is armed after now and no further ahead than the
    /// longest wait or interval. A key is written only when it must be,
    /// never while its style is paused, and never points at a missing slot.
    /// A style whose key is locked elsewhere or missing is left alone. Every
    /// rotation ends.
    #[test]
    fn every_effect_keeps_the_invariants(scenario in scenario()) {
        play(&scenario)?;
    }

    /// The same seed and the same events give the same effects.
    #[test]
    fn a_replay_gives_the_same_effects(scenario in scenario()) {
        prop_assert_eq!(play(&scenario)?, play(&scenario)?);
    }

    /// What the daemon saves is enough to restore it over the same files:
    /// each slot's image and clock, and the shuffle round in its order.
    /// Only an image that the round neither holds nor showed joins it.
    #[test]
    fn saved_state_survives_a_round_trip(scenario in scenario()) {
        let mut daemon = Daemon::boot(&scenario)?;
        for op in &scenario.ops {
            daemon.apply(op)?;
        }
        let saved = daemon.model.persisted();
        let files = PerStyle::from_fn(|style| Some(local_files(&daemon.scan(style))));

        let restored = new_model(&daemon.config, files, 0, Some(saved.clone())).persisted();

        prop_assert_eq!(&restored.original_keys, &saved.original_keys);
        for style in Style::ALL {
            let (was, is) = (&saved.styles[style], &restored.styles[style]);
            let without_bag = |state: &StylePersisted| StylePersisted {
                bag: Vec::new(),
                ..state.clone()
            };
            prop_assert_eq!(without_bag(is), without_bag(was));
            prop_assert!(is_subsequence(&was.bag, &is.bag), "the {style} round changed");
            let joined = is.bag.iter().filter(|item| !was.bag.contains(item));
            for item in joined {
                let was_shown = was.shown.as_ref().is_some_and(|shown| shown.contains(item));
                prop_assert!(!was_shown, "{item} joined its round again");
            }
        }
    }
}
