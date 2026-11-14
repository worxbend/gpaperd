//! Properties of remote images over generated event sequences: the fetches
//! in flight, the URLs that a reload removes, and rotation, which never
//! waits for the network.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::{BadReason, FileId, ItemId};
use gpaperd_core::config::{Config, EmptyStylePolicy, Order, RemoteRefresh, Source};
use gpaperd_core::event::{
    BackgroundKeys, Connectivity, Effect, Event, FetchOutcome, Fired, GnomeEvent, Method, SlotWrite,
};
use gpaperd_core::model::{Model, ModelInit};
use gpaperd_core::step::{MAX_FETCHES_IN_FLIGHT, step};
use gpaperd_core::style::{PerStyle, Style};
use proptest::prelude::*;
use proptest::sample::subsequence;
use proptest::test_runner::TestCaseError;
use url::Url;

use crate::support::{at, blob, files, keys_changed, slot_uris, started, user_keys, write_keys};

/// Every URL a generated config can list.
const URLS: [&str; 6] = [
    "https://img.test/a.jpg",
    "https://img.test/b.png",
    "https://img.test/c.jpg",
    "https://other.test/d.jpg",
    "https://other.test/e.jpg",
    "http://plain.test/f.jpg",
];

/// The light style's local images, so that it always has two to rotate
/// through, whatever the network does.
const LOCAL: [&str; 2] = ["/l/a.jpg", "/l/b.jpg"];

/// Steps that one event and the answers to its effects may take. Probes,
/// publishes and key writes of both styles come to a few dozen at most.
const MAX_STEPS_PER_EVENT: usize = 200;

/// Answers at the end that must drain every fetch: each URL once in flight,
/// and once more for a blob that was lost meanwhile.
const MAX_FETCHES_OWED: usize = 2 * URLS.len();

#[derive(Debug, Clone)]
enum Outcome {
    Stored,
    NotModified,
    Gone,
    Failed { secs: u64 },
    Unreachable { secs: u64 },
}

#[derive(Debug, Clone)]
enum Op {
    /// Time passes. The timer fires when its deadline is reached.
    Wait {
        secs: u64,
    },
    /// The fetcher answers one of the fetches in flight.
    Answer {
        pick: usize,
        outcome: Outcome,
    },
    Connectivity(Connectivity),
    /// The config file lists other URLs now.
    Reload {
        light: Vec<&'static str>,
        dark: Vec<&'static str>,
    },
    /// Someone deletes the cached blob of a URL.
    LoseBlob(&'static str),
}

#[derive(Debug, Clone)]
struct Scenario {
    /// Everything but the sources.
    config: Config,
    light: Vec<&'static str>,
    dark: Vec<&'static str>,
    /// The URLs whose blob is on disk at start.
    cached: Vec<&'static str>,
    seed: u64,
    ops: Vec<Op>,
}

fn urls() -> impl Strategy<Value = Vec<&'static str>> {
    subsequence(URLS.to_vec(), 0..=URLS.len())
}

fn order() -> impl Strategy<Value = Order> {
    prop_oneof![Just(Order::Random), Just(Order::Alphabetical)]
}

fn remote_refresh() -> impl Strategy<Value = RemoteRefresh> {
    prop_oneof![
        Just(RemoteRefresh::Startup),
        Just(RemoteRefresh::Never),
        (3600_u64..7200).prop_map(|secs| RemoteRefresh::Every(Duration::from_secs(secs))),
    ]
}

fn config() -> impl Strategy<Value = Config> {
    let general = (remote_refresh(), any::<bool>(), any::<bool>());
    let styles = (order(), order(), 60_u64..=3600, 60_u64..=3600);
    (general, styles).prop_map(|(general, styles)| {
        let (remote_refresh, rotate_hidden, keep_current) = general;
        let (light_order, dark_order, light_secs, dark_secs) = styles;
        let mut config = Config::default();
        config.general.remote_refresh = remote_refresh;
        config.general.rotate_hidden = rotate_hidden;
        if keep_current {
            config.general.empty_style = EmptyStylePolicy::KeepCurrent;
        }
        config.styles.light.order = light_order;
        config.styles.dark.order = dark_order;
        config.styles.light.interval = Duration::from_secs(light_secs);
        config.styles.dark.interval = Duration::from_secs(dark_secs);
        config
    })
}

fn outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        3 => Just(Outcome::Stored),
        2 => Just(Outcome::NotModified),
        1 => Just(Outcome::Gone),
        2 => (1_u64..4000).prop_map(|secs| Outcome::Failed { secs }),
        2 => (1_u64..4000).prop_map(|secs| Outcome::Unreachable { secs }),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let url = || prop::sample::select(URLS.to_vec());
    prop_oneof![
        6 => (1_u64..3000).prop_map(|secs| Op::Wait { secs }),
        6 => (any::<usize>(), outcome()).prop_map(|(pick, outcome)| Op::Answer { pick, outcome }),
        1 => Just(Op::Connectivity(Connectivity::Full)),
        1 => Just(Op::Connectivity(Connectivity::Limited)),
        1 => (urls(), urls()).prop_map(|(light, dark)| Op::Reload { light, dark }),
        1 => url().prop_map(Op::LoseBlob),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    let ops = prop::collection::vec(op(), 0..60);
    (config(), urls(), urls(), urls(), any::<u64>(), ops).prop_map(
        |(config, light, dark, cached, seed, ops)| Scenario {
            config,
            light,
            dark,
            cached,
            seed,
            ops,
        },
    )
}

fn url(text: &str) -> Url {
    Url::parse(text).expect("a test URL")
}

/// `base` with these URLs, and with the local images for light.
fn with_sources(base: &Config, light: &[&str], dark: &[&str]) -> Config {
    let remote = |urls: &[&str]| urls.iter().map(|text| Source::Url(url(text))).collect();
    let mut config = base.clone();
    config.styles.light.sources = remote(light);
    let local = LOCAL.iter().map(|path| Source::File(PathBuf::from(path)));
    config.styles.light.sources.extend(local);
    config.styles.dark.sources = remote(dark);
    config
}

/// The interpreter, the fetcher and GNOME, as far as the reducer can tell.
struct Daemon {
    base: Config,
    model: Model,
    now_secs: u64,
    armed: Option<SystemTime>,
    keys: BackgroundKeys,
    last_slot_inode: u64,
    /// The URLs of the running config.
    listed: BTreeSet<Url>,
    /// The URLs that a reload removed and none listed again since.
    removed: BTreeSet<ItemId>,
    /// The `Fetch` effects that have no answer yet, oldest first.
    in_flight: Vec<Url>,
    /// The version of each URL's blob in the cache.
    versions: BTreeMap<Url, u32>,
    /// The URLs whose latest blob someone deleted.
    lost: BTreeSet<Url>,
    /// When the next rotation is due, as the latest status said.
    next_change: Option<SystemTime>,
}

impl Daemon {
    fn boot(scenario: &Scenario) -> Result<Daemon, TestCaseError> {
        let config = with_sources(&scenario.config, &scenario.light, &scenario.dark);
        let model = Model::new(ModelInit {
            config: Some(config.clone()),
            local_files: PerStyle {
                light: Some(files(&LOCAL)),
                dark: Some(Vec::new()),
            },
            restored: None,
            seed: scenario.seed,
            slot_uris: slot_uris(),
        });
        let mut daemon = Daemon {
            base: scenario.config.clone(),
            model,
            now_secs: 0,
            armed: None,
            keys: user_keys(),
            last_slot_inode: 0,
            listed: BTreeSet::new(),
            removed: BTreeSet::new(),
            in_flight: Vec::new(),
            versions: BTreeMap::new(),
            lost: BTreeSet::new(),
            next_change: None,
        };
        daemon.listed = scenario
            .light
            .iter()
            .chain(&scenario.dark)
            .map(|text| url(text))
            .collect();
        daemon.send(started(Style::Light, user_keys(), PerStyle::default()))?;
        let cached = scenario.cached.iter().map(|text| blob(text, 1)).collect();
        daemon.versions = scenario.cached.iter().map(|text| (url(text), 1)).collect();
        daemon.send(Event::CacheLoaded(cached))?;
        Ok(daemon)
    }

    fn now(&self) -> SystemTime {
        at(self.now_secs)
    }

    fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::Wait { secs } => self.wait(*secs),
            Op::Answer { pick, outcome } => {
                if self.in_flight.is_empty() {
                    return Ok(());
                }
                let fetched = self.in_flight.remove(pick % self.in_flight.len());
                self.answer(fetched, outcome)
            }
            Op::Connectivity(level) => self
                .send(Event::Gnome(GnomeEvent::ConnectivityChanged(*level)))
                .map(drop),
            Op::Reload { light, dark } => self.reload(light, dark),
            Op::LoseBlob(text) => {
                let lost = url(text);
                if self.versions.contains_key(&lost) {
                    self.lost.insert(lost);
                }
                Ok(())
            }
        }
    }

    /// Time passes. When a rotation is due by then, the timer's step starts
    /// it, whatever the fetches in flight.
    fn wait(&mut self, secs: u64) -> Result<(), TestCaseError> {
        self.now_secs += secs;
        if self.armed.is_none_or(|armed| self.now() < armed) {
            return Ok(());
        }
        self.armed = None;
        let is_rotation_due = self.next_change.is_some_and(|due| due <= self.now());
        let first = self.send(Event::Fired(Fired::Deadline))?;
        let probes = |effect: &Effect| matches!(effect, Effect::Probe { .. });
        prop_assert!(
            !is_rotation_due || first.iter().any(probes),
            "a rotation that was due waited, with {:?} in flight",
            self.in_flight
        );
        Ok(())
    }

    fn answer(&mut self, fetched: Url, outcome: &Outcome) -> Result<(), TestCaseError> {
        let outcome = match outcome {
            Outcome::Stored => {
                let version = self.versions.entry(fetched.clone()).or_insert(0);
                *version += 1;
                self.lost.remove(&fetched);
                let blob = blob(fetched.as_str(), *version).blob;
                FetchOutcome::Stored { blob }
            }
            Outcome::NotModified => FetchOutcome::NotModified,
            Outcome::Gone => FetchOutcome::Gone,
            Outcome::Failed { secs } => FetchOutcome::Failed {
                retry_in: Duration::from_secs(*secs),
            },
            Outcome::Unreachable { secs } => FetchOutcome::Unreachable {
                retry_in: Duration::from_secs(*secs),
            },
        };
        self.send(Event::Fetched {
            url: fetched,
            outcome,
        })
        .map(drop)
    }

    fn reload(&mut self, light: &[&str], dark: &[&str]) -> Result<(), TestCaseError> {
        let listed: BTreeSet<Url> = light.iter().chain(dark).map(|text| url(text)).collect();
        let left = self.listed.difference(&listed).map(ItemId::from_url);
        self.removed.extend(left);
        self.removed
            .retain(|item| !listed.iter().any(|url| ItemId::from_url(url) == *item));
        self.listed = listed;
        let config = with_sources(&self.base, light, dark);
        self.send(Event::ConfigChanged(Ok(config))).map(drop)
    }

    /// Answers every fetch still in flight with "not modified". The queue
    /// drains.
    fn drain_fetches(&mut self) -> Result<(), TestCaseError> {
        for _ in 0..MAX_FETCHES_OWED {
            let Some(fetched) = self.in_flight.first().cloned() else {
                return Ok(());
            };
            self.in_flight.remove(0);
            self.answer(fetched, &Outcome::NotModified)?;
        }
        prop_assert!(
            self.in_flight.is_empty(),
            "the fetches never come to an end"
        );
        Ok(())
    }

    /// Feeds `event`, then answers every probe, publish and key write until
    /// the reducer is quiet. Returns the effects of the first step.
    fn send(&mut self, event: Event) -> Result<Vec<Effect>, TestCaseError> {
        let mut inbox = VecDeque::from([event]);
        let mut first = None;
        for _ in 0..MAX_STEPS_PER_EVENT {
            let Some(event) = inbox.pop_front() else {
                return Ok(first.unwrap_or_default());
            };
            let now = self.now();
            let effects = step(&mut self.model, now, event);
            for effect in &effects {
                inbox.extend(self.check(effect)?);
            }
            first.get_or_insert(effects);
        }
        panic!("the reducer is not quiet after {MAX_STEPS_PER_EVENT} steps");
    }

    /// Checks one effect and returns the answer to it, if any.
    fn check(&mut self, effect: &Effect) -> Result<Option<Event>, TestCaseError> {
        let answer = match effect {
            Effect::Fetch(fetched) => {
                prop_assert!(self.listed.contains(fetched), "{fetched} is not listed");
                prop_assert!(
                    !self.in_flight.contains(fetched),
                    "two fetches of {fetched} in flight"
                );
                self.in_flight.push(fetched.clone());
                prop_assert!(
                    self.in_flight.len() <= MAX_FETCHES_IN_FLIGHT,
                    "{} fetches in flight",
                    self.in_flight.len()
                );
                None
            }
            Effect::DropRemote(dropped) => {
                prop_assert!(!self.listed.contains(dropped), "{dropped} is still listed");
                self.in_flight.retain(|fetched| fetched != dropped);
                None
            }
            Effect::CollectGarbage { keep } => {
                let kept: BTreeSet<Url> = keep.iter().cloned().collect();
                prop_assert_eq!(
                    &kept,
                    &self.listed,
                    "garbage collection keeps the listed URLs"
                );
                None
            }
            Effect::Probe {
                style, item, path, ..
            } => Some(Event::Probed {
                style: *style,
                item: item.clone(),
                verdict: self.probe(path),
            }),
            Effect::Publish { style, item, .. } => {
                prop_assert!(
                    !self.removed.contains(item),
                    "{item} was published after its removal"
                );
                Some(Event::Published {
                    style: *style,
                    item: item.clone(),
                    result: Ok(self.write_slot()),
                })
            }
            Effect::Republish(style) => Some(Event::Republished {
                style: *style,
                result: Ok(self.write_slot()),
            }),
            Effect::SetKeys { .. } => {
                write_keys(&mut self.keys, effect);
                Some(keys_changed(self.keys.clone()))
            }
            Effect::Arm(deadline) => {
                if let Some(deadline) = deadline {
                    prop_assert!(self.now() < *deadline, "armed for a time that has passed");
                }
                self.armed = *deadline;
                None
            }
            Effect::Report(status) => {
                self.next_change = status.next_change;
                None
            }
            Effect::Watch { .. } | Effect::Persist | Effect::Ready | Effect::Changed { .. } => None,
            Effect::Reply { .. } | Effect::ReadConfig(_) | Effect::RestoreKeys { .. } => {
                prop_assert!(false, "{effect:?} without a control command");
                None
            }
        };
        Ok(answer)
    }

    /// A blob that someone deleted is missing. Every other file is fine.
    fn probe(&self, path: &Path) -> Result<(), BadReason> {
        let is_lost = self.lost.iter().any(|lost| {
            let version = self.versions.get(lost).copied().unwrap_or_default();
            blob(lost.as_str(), version).blob == *path
        });
        if is_lost {
            Err(BadReason::Missing)
        } else {
            Ok(())
        }
    }

    /// Puts a slot on a new inode.
    fn write_slot(&mut self) -> SlotWrite {
        self.last_slot_inode += 1;
        let slot = FileId {
            dev: 2,
            ino: self.last_slot_inode,
        };
        SlotWrite {
            method: Method::Hardlink,
            slot,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Whatever the network does: never more than four fetches in flight,
    /// never two of one URL, and only of listed URLs. A URL that a reload
    /// removed is not published until a reload lists it again. A rotation
    /// that is due starts on time, whatever is in flight. Every fetch ends.
    #[test]
    fn fetches_never_hold_up_rotation(scenario in scenario()) {
        let mut daemon = Daemon::boot(&scenario)?;
        for op in &scenario.ops {
            daemon.apply(op)?;
        }
        daemon.drain_fetches()?;
    }
}
