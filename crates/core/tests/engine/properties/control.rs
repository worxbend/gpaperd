//! Control requests under any interleaving: each gets exactly one reply.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpaperd_core::catalog::{BadReason, FileId, FsChange};
use gpaperd_core::config::{Config, ConfigError};
use gpaperd_core::control::{Command, ReplyId, StyleTarget};
use gpaperd_core::event::{Effect, Event, Fired, GnomeEvent, Method, SlotWrite};
use gpaperd_core::model::Model;
use gpaperd_core::step::step;
use gpaperd_core::style::PerStyle;
use proptest::prelude::*;
use proptest::sample::subsequence;
use proptest::test_runner::TestCaseError;

use super::daemon::new_model;
use super::scenario::{IMAGES, config, image, style};
use crate::support::{at, id, keys_changed, slot_uris, started, user_keys, version};

/// Steps that the final drain takes before it gives up. Every answer
/// leads to a few more effects at most, so a reducer that needs more
/// answers its own answers for ever.
const MAX_DRAIN_STEPS: usize = 2_000;

#[derive(Debug, Clone)]
enum Op {
    Ask(Command),
    /// The interpreter answers one of the effects it holds, picked by the
    /// index, with success or failure.
    Answer {
        pick: usize,
        fails: bool,
    },
    Wait {
        secs: u64,
    },
    /// Anything else that may happen meanwhile.
    Happen(Event),
}

fn target() -> impl Strategy<Value = StyleTarget> {
    prop_oneof![
        Just(StyleTarget::Light),
        Just(StyleTarget::Dark),
        Just(StyleTarget::Visible),
        Just(StyleTarget::Both),
    ]
}

pub fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        3 => target().prop_map(Command::Next),
        3 => target().prop_map(Command::Previous),
        2 => (target(), image()).prop_map(|(target, image)| Command::Set(target, id(image))),
        1 => target().prop_map(Command::Ban),
        1 => image().prop_map(|image| Command::Unban(id(image))),
        1 => target().prop_map(Command::Favorite),
        2 => target().prop_map(Command::Pause),
        2 => target().prop_map(Command::Resume),
        1 => Just(Command::Reload),
        1 => Just(Command::RefreshRemote),
        1 => Just(Command::Restore),
        1 => Just(Command::Status),
    ]
}

fn happening() -> impl Strategy<Value = Event> {
    let foreign_key = Just("file:///home/u/picked.jpg".to_owned());
    let keys = prop_oneof![Just(slot_uris().light), foreign_key].prop_map(|light| {
        let mut keys = user_keys();
        keys.picture_uri = light;
        keys
    });
    prop_oneof![
        2 => Just(Event::Fired(Fired::Deadline)),
        2 => style().prop_map(|style| Event::Gnome(GnomeEvent::StyleChanged(style))),
        1 => style().prop_map(Event::SlotLost),
        2 => keys.prop_map(keys_changed),
        2 => (style(), image()).prop_map(|(style, image)| Event::Fs {
            style,
            changes: vec![FsChange::Removed(PathBuf::from(image))],
        }),
        2 => (style(), image(), 0_u64..3).prop_map(|(style, image, saves)| Event::Fs {
            style,
            changes: vec![FsChange::Found(version(image, saves))],
        }),
        1 => config().prop_map(|config| Event::ConfigChanged(Ok(config))),
        1 => Just(Event::Shutdown),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => command().prop_map(Op::Ask),
        8 => (any::<usize>(), prop::bool::weighted(0.2))
            .prop_map(|(pick, fails)| Op::Answer { pick, fails }),
        2 => (1_u64..2000).prop_map(|secs| Op::Wait { secs }),
        3 => happening().prop_map(Op::Happen),
    ]
}

/// Plays the interpreter: holds every effect that wants an answer until an
/// op answers it, and counts the replies per request.
struct Interpreter {
    model: Model,
    now: SystemTime,
    unanswered: Vec<Effect>,
    reloaded: Result<Config, ConfigError>,
    asked: u64,
    replies: BTreeMap<u64, usize>,
    last_inode: u64,
}

impl Interpreter {
    fn feed(&mut self, event: Event) -> Result<(), TestCaseError> {
        for effect in step(&mut self.model, self.now, event) {
            match effect {
                Effect::Reply { id, .. } => {
                    prop_assert!(id.0 < self.asked, "a reply to {id:?}, which nobody sent");
                    *self.replies.entry(id.0).or_default() += 1;
                }
                Effect::Probe { .. }
                | Effect::Publish { .. }
                | Effect::Republish(_)
                | Effect::ReadConfig(_)
                | Effect::RestoreKeys { .. }
                | Effect::SaveFavorites { .. } => self.unanswered.push(effect),
                _ => {}
            }
        }
        Ok(())
    }

    fn ask(&mut self, command: Command) -> Result<(), TestCaseError> {
        let id = ReplyId(self.asked);
        self.asked += 1;
        self.feed(Event::Control { id, command })
    }

    fn answer(&mut self, pick: usize, fails: bool) -> Result<(), TestCaseError> {
        if self.unanswered.is_empty() {
            return Ok(());
        }
        let effect = self.unanswered.remove(pick % self.unanswered.len());
        let answer = self.answer_to(effect, fails);
        self.feed(answer)
    }

    fn answer_to(&mut self, effect: Effect, fails: bool) -> Event {
        self.last_inode += 1;
        let written = SlotWrite {
            method: Method::Copy,
            slot: FileId {
                dev: 2,
                ino: self.last_inode,
            },
        };
        let written = if fails {
            Err("Permission denied".to_owned())
        } else {
            Ok(written)
        };
        match effect {
            Effect::Probe { style, item, .. } => Event::Probed {
                style,
                item,
                verdict: if fails {
                    Err(BadReason::NotAnImage)
                } else {
                    Ok(())
                },
            },
            Effect::Publish { style, item, .. } => Event::Published {
                style,
                item,
                result: written,
            },
            Effect::Republish(style) => Event::Republished {
                style,
                result: written,
            },
            Effect::ReadConfig(id) => Event::ConfigRead {
                id,
                result: self.reloaded.clone(),
            },
            Effect::RestoreKeys { id, .. } => Event::KeysRestored {
                id,
                result: if fails {
                    Err("picture-uri is locked".to_owned())
                } else {
                    Ok(())
                },
            },
            Effect::SaveFavorites { id, .. } => Event::FavoritesSaved {
                id,
                result: if fails {
                    Err("No space left on device".to_owned())
                } else {
                    Ok(())
                },
            },
            other => unreachable!("{other:?} is never held"),
        }
    }

    /// Answers everything still held, oldest first, until nothing is left.
    fn drain(&mut self) -> Result<(), TestCaseError> {
        for _ in 0..MAX_DRAIN_STEPS {
            if self.unanswered.is_empty() {
                return Ok(());
            }
            self.answer(0, false)?;
        }
        panic!("the reducer is not quiet after {MAX_DRAIN_STEPS} answers");
    }

    fn check_every_request_answered_once(&self) -> Result<(), TestCaseError> {
        for n in 0..self.asked {
            let count = self.replies.get(&n).copied().unwrap_or(0);
            prop_assert_eq!(count, 1, "request {} got {} replies", n, count);
        }
        Ok(())
    }
}

fn folder() -> impl Strategy<Value = Vec<&'static str>> {
    subsequence(IMAGES.to_vec(), 0..=IMAGES.len())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every `Control` event yields exactly one `Reply` for its id, under
    /// any interleaving with answers and other events: before `Started`,
    /// while rotations are in flight, and after `Shutdown`.
    #[test]
    fn every_control_request_gets_exactly_one_reply(
        config in config(),
        folders in (folder(), folder()),
        visible in style(),
        seed in any::<u64>(),
        before_start in prop::collection::vec(command(), 0..3),
        ops in prop::collection::vec(op(), 0..80),
        reload_fails in any::<bool>(),
    ) {
        let found = |images: &[&str]| Some(images.iter().map(|image| version(image, 0)).collect());
        let files = PerStyle { light: found(&folders.0), dark: found(&folders.1) };
        let reloaded = if reload_fails {
            Err(ConfigError { line: 2, column: 5, message: "unknown key".to_owned() })
        } else {
            Ok(config.clone())
        };
        let mut daemon = Interpreter {
            model: new_model(&config, files, seed, None),
            now: at(0),
            unanswered: Vec::new(),
            reloaded,
            asked: 0,
            replies: BTreeMap::new(),
            last_inode: 0,
        };
        for command in before_start {
            daemon.ask(command)?;
        }
        daemon.feed(started(visible, user_keys(), PerStyle::default()))?;
        for op in ops {
            match op {
                Op::Ask(command) => daemon.ask(command)?,
                Op::Answer { pick, fails } => daemon.answer(pick, fails)?,
                Op::Wait { secs } => daemon.now += Duration::from_secs(secs),
                Op::Happen(event) => daemon.feed(event)?,
            }
        }
        daemon.drain()?;
        daemon.check_every_request_answered_once()?;
        daemon.feed(Event::Shutdown)?;
        daemon.ask(Command::Status)?;
        daemon.check_every_request_answered_once()?;
    }
}
