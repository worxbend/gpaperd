//! Generated scenarios: the config, the folders, and what happens to the
//! daemon.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use gpaperd_core::catalog::ItemId;
use gpaperd_core::config::{
    Config, EmptyStylePolicy, ExternalChangePolicy, Order, PictureOptions, Source,
};
use gpaperd_core::control::{Command, StyleTarget};
use gpaperd_core::event::ScreenLock;
use gpaperd_core::style::{PerStyle, Style};
use proptest::prelude::*;
use proptest::sample::subsequence;

use crate::support::id;

/// Every image a generated folder can hold. Both styles draw from this list,
/// so some scenarios share files between the styles.
pub const IMAGES: [&str; 6] = [
    "/pool/a.jpg",
    "/pool/B.jpg",
    "/pool/c2.jpg",
    "/pool/c10.jpg",
    "/other/a.jpg",
    "/other/d.jpg",
];

/// The images that each style's sources cover.
#[derive(Debug, Clone)]
pub struct Folders {
    pub light: Vec<&'static str>,
    pub dark: Vec<&'static str>,
}

impl Folders {
    pub fn of(&self, style: Style) -> &[&'static str] {
        match style {
            Style::Light => &self.light,
            Style::Dark => &self.dark,
        }
    }

    pub fn of_mut(&mut self, style: Style) -> &mut Vec<&'static str> {
        match style {
            Style::Light => &mut self.light,
            Style::Dark => &mut self.dark,
        }
    }

    /// The config sources that cover exactly these images: one file source
    /// per image, so that other folders mean other sources.
    pub fn sources(&self, style: Style) -> Vec<Source> {
        let covered = self.of(style).iter();
        covered
            .map(|image| Source::File(PathBuf::from(image)))
            .collect()
    }
}

/// One run of the daemon process.
#[derive(Debug, Clone)]
pub struct Boot {
    /// Every image in them is on disk when the run starts.
    pub folders: Folders,
    pub visible: Style,
    pub seed: u64,
    /// Whether the model is built over a finished scan. Otherwise it learns
    /// of its files when the scans come back.
    pub scanned_up_front: bool,
    /// The keys that an administrator locked for this run, on whatever they
    /// hold when it starts.
    pub locked: PerStyle<bool>,
    /// Whether this run's GNOME has a dark key. Without one it shows light
    /// only.
    pub has_dark_key: bool,
}

/// The config file after it was saved again.
#[derive(Debug, Clone)]
pub struct Reload {
    /// Everything but the sources.
    pub config: Config,
    /// What the new sources cover, unless `keeps_sources`.
    pub folders: Folders,
    pub keeps_sources: bool,
}

#[derive(Debug, Clone)]
pub enum Op {
    /// Time passes. The timer fires when its deadline is reached.
    Wait {
        secs: u64,
    },
    /// The timer fires although its deadline has not passed.
    FireEarly,
    /// The wall clock jumps forward.
    ClockForward {
        secs: u64,
    },
    /// The wall clock jumps back.
    ClockBack {
        secs: u64,
    },
    Switch(Style),
    /// The interpreter answers its oldest unanswered probe or publish. A
    /// probe of a sound image says "truncated" while it is still copied.
    Answer {
        publish_fails: bool,
        still_copied: bool,
    },
    /// A scan that the daemon asked for comes back.
    ScanDone(Style),
    /// A file appears, as it was when it left, in every folder that covers
    /// it.
    Create(&'static str),
    Delete(&'static str),
    /// A file is saved again.
    Edit(&'static str),
    /// Someone removes a slot file.
    LoseSlot(Style),
    /// Something else writes a wallpaper of its own to a style's key.
    TakeKey(Style),
    /// Something else writes the slot URI back to a style's key.
    ReturnKey(Style),
    /// The config file is saved again, and it is valid.
    Reload(Box<Reload>),
    /// The config file is saved with a mistake in it.
    ReloadFails,
    /// The daemon is killed and started again.
    Restart {
        boot: Boot,
        lost_slots: PerStyle<bool>,
    },
    /// `gpaperctl` sends a command: `next`, `previous`, `pause`, `resume`,
    /// `restore` or `status`.
    Ask(Command),
    /// The screen is locked or unlocked.
    Lock(ScreenLock),
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub config: Config,
    /// Images whose probe fails, whatever is saved to them.
    pub broken: BTreeSet<ItemId>,
    pub first_boot: Boot,
    pub ops: Vec<Op>,
}

pub fn style() -> impl Strategy<Value = Style> {
    prop_oneof![Just(Style::Light), Just(Style::Dark)]
}

fn order() -> impl Strategy<Value = Order> {
    prop_oneof![Just(Order::Random), Just(Order::Alphabetical)]
}

fn folder() -> impl Strategy<Value = Vec<&'static str>> {
    subsequence(IMAGES.to_vec(), 0..=IMAGES.len())
}

pub fn image() -> impl Strategy<Value = &'static str> {
    prop::sample::select(IMAGES.to_vec())
}

fn folders() -> impl Strategy<Value = Folders> {
    (folder(), folder()).prop_map(|(light, dark)| Folders { light, dark })
}

/// What GNOME lets the daemon do with the keys. Mostly everything.
fn key_access() -> impl Strategy<Value = (PerStyle<bool>, bool)> {
    let rarely = || prop::bool::weighted(0.1);
    (rarely(), rarely(), prop::bool::weighted(0.9)).prop_map(|(light, dark, has_dark_key)| {
        // GNOME cannot lock a key that it does not have.
        let dark = dark && has_dark_key;
        (PerStyle { light, dark }, has_dark_key)
    })
}

fn boot() -> impl Strategy<Value = Boot> {
    let run = (style(), any::<u64>(), any::<bool>());
    (folders(), run, key_access()).prop_map(|(folders, run, key_access)| {
        let (visible, seed, scanned_up_front) = run;
        let (locked, has_dark_key) = key_access;
        Boot {
            folders,
            // Without a dark key GNOME has no dark style to show.
            visible: if has_dark_key { visible } else { Style::Light },
            seed,
            scanned_up_front,
            locked,
            has_dark_key,
        }
    })
}

fn policy() -> impl Strategy<Value = ExternalChangePolicy> {
    prop_oneof![
        Just(ExternalChangePolicy::Pause),
        Just(ExternalChangePolicy::ReassertNextTick),
        Just(ExternalChangePolicy::ReassertNow),
    ]
}

fn picture_options() -> impl Strategy<Value = Option<PictureOptions>> {
    prop::option::of(prop_oneof![
        Just(PictureOptions::Zoom),
        Just(PictureOptions::Spanned),
    ])
}

/// A config in slots mode with no sources.
pub fn config() -> impl Strategy<Value = Config> {
    let general = (any::<bool>(), any::<bool>(), policy(), picture_options());
    let styles = (order(), order(), 60_u64..=3600, 60_u64..=3600);
    let switches = (any::<bool>(), any::<bool>());
    (general, styles, switches).prop_map(|(general, styles, switches)| {
        let (rotate_hidden, keep_current, on_external_change, picture_options) = general;
        let (pause_when_locked, has_favorites_dir) = switches;
        let (light_order, dark_order, light_secs, dark_secs) = styles;
        let mut config = Config::default();
        config.general.rotate_hidden = rotate_hidden;
        config.general.on_external_change = on_external_change;
        config.general.picture_options = picture_options;
        config.general.pause_when_locked = pause_when_locked;
        if has_favorites_dir {
            config.general.favorites_dir = Some(PathBuf::from("/home/u/favorites"));
        }
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

fn answer() -> impl Strategy<Value = Op> {
    let rarely = || prop::bool::weighted(0.2);
    (rarely(), rarely()).prop_map(|(publish_fails, still_copied)| Op::Answer {
        publish_fails,
        still_copied,
    })
}

fn reload() -> impl Strategy<Value = Op> {
    (config(), folders(), any::<bool>()).prop_map(|(config, folders, keeps_sources)| {
        Op::Reload(Box::new(Reload {
            config,
            folders,
            keeps_sources,
        }))
    })
}

fn target() -> impl Strategy<Value = StyleTarget> {
    prop_oneof![
        Just(StyleTarget::Light),
        Just(StyleTarget::Dark),
        Just(StyleTarget::Visible),
        Just(StyleTarget::Both),
    ]
}

/// A command whose effects the [`Daemon`](super::daemon::Daemon) can
/// answer: no reload, which reads the config file.
fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        3 => target().prop_map(Command::Next),
        3 => target().prop_map(Command::Previous),
        2 => (target(), image()).prop_map(|(target, image)| Command::Set(target, id(image))),
        2 => target().prop_map(Command::Pause),
        2 => target().prop_map(Command::Resume),
        1 => Just(Command::Restore),
        1 => Just(Command::Status),
    ]
}

fn screen_lock() -> impl Strategy<Value = ScreenLock> {
    prop_oneof![Just(ScreenLock::Locked), Just(ScreenLock::Unlocked)]
}

fn op() -> impl Strategy<Value = Op> {
    let lost_slots =
        (any::<bool>(), any::<bool>()).prop_map(|(light, dark)| PerStyle { light, dark });
    prop_oneof![
        8 => answer(),
        4 => (1_u64..2500).prop_map(|secs| Op::Wait { secs }),
        // Short enough to land inside the waits of a second or less.
        2 => Just(Op::Wait { secs: 1 }),
        3 => style().prop_map(Op::TakeKey),
        2 => style().prop_map(Op::ReturnKey),
        1 => reload(),
        1 => Just(Op::ReloadFails),
        1 => Just(Op::FireEarly),
        1 => (1_u64..5000).prop_map(|secs| Op::ClockForward { secs }),
        1 => (1_u64..5000).prop_map(|secs| Op::ClockBack { secs }),
        2 => style().prop_map(Op::Switch),
        2 => style().prop_map(Op::ScanDone),
        1 => image().prop_map(Op::Create),
        1 => image().prop_map(Op::Delete),
        1 => image().prop_map(Op::Edit),
        1 => style().prop_map(Op::LoseSlot),
        1 => (boot(), lost_slots).prop_map(|(boot, lost_slots)| Op::Restart { boot, lost_slots }),
        2 => command().prop_map(Op::Ask),
        1 => screen_lock().prop_map(Op::Lock),
    ]
}

pub fn scenario() -> impl Strategy<Value = Scenario> {
    let broken = subsequence(IMAGES.to_vec(), 0..=IMAGES.len());
    let ops = prop::collection::vec(op(), 0..80);
    (config(), broken, boot(), ops).prop_map(|(config, broken, first_boot, ops)| Scenario {
        config,
        broken: broken.into_iter().map(id).collect(),
        first_boot,
        ops,
    })
}
