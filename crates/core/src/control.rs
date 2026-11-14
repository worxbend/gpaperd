//! The control API's plain data: commands from `gpaperctl` (through the
//! daemon's D-Bus server), the core's replies, the `Status` JSON document,
//! and each style's history for `Previous`.
//!
//! The D-Bus server keeps the reply channel; the core only sees a
//! [`ReplyId`] in [`Event::Control`](crate::event::Event::Control) and answers
//! it with [`Effect::Reply`](crate::event::Effect::Reply). So the core stays
//! free of async. `gpaperctl` depends on this module for the bus names and
//! for [`StatusReport`], so client and daemon cannot drift apart.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::catalog::ItemId;
use crate::event::Pause;
use crate::style::{PerStyle, Style};

/// Well-known bus name of the daemon on the session bus.
pub const BUS_NAME: &str = "io.github.worxbend.Gpaperd1";
/// Path of the daemon's one object.
pub const OBJECT_PATH: &str = "/io/github/worxbend/Gpaperd1";
/// Name of the control interface.
pub const INTERFACE: &str = "io.github.worxbend.Gpaperd1";

/// Entries of a style's history, at most.
pub const HISTORY_LEN: usize = 20;

/// Version of the [`StatusReport`] JSON document. A change that removes or
/// retypes a field bumps it; a new field does not.
pub const STATUS_VERSION: u32 = 1;

/// Which styles a command acts on: the `style` argument of the D-Bus
/// methods `Next`, `Previous`, `Pause` and `Resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StyleTarget {
    Light,
    Dark,
    /// The style on screen when the core handles the command.
    Visible,
    Both,
}

impl StyleTarget {
    /// `"light"`, `"dark"`, `"visible"` or `"both"`: the D-Bus argument.
    pub const fn as_str(self) -> &'static str {
        match self {
            StyleTarget::Light => "light",
            StyleTarget::Dark => "dark",
            StyleTarget::Visible => "visible",
            StyleTarget::Both => "both",
        }
    }

    /// The styles meant, light first, while `visible` is on screen.
    pub fn styles(self, visible: Style) -> Vec<Style> {
        match self {
            StyleTarget::Light => vec![Style::Light],
            StyleTarget::Dark => vec![Style::Dark],
            StyleTarget::Visible => vec![visible],
            StyleTarget::Both => Style::ALL.to_vec(),
        }
    }
}

impl fmt::Display for StyleTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `style` argument that is none of the four names. The D-Bus server
/// answers it with `org.freedesktop.DBus.Error.InvalidArgs`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown style {0:?}: expected light, dark, visible or both")]
pub struct UnknownStyleTarget(pub String);

impl FromStr for StyleTarget {
    type Err = UnknownStyleTarget;

    /// Exact, lowercase names only.
    fn from_str(name: &str) -> Result<StyleTarget, UnknownStyleTarget> {
        match name {
            "light" => Ok(StyleTarget::Light),
            "dark" => Ok(StyleTarget::Dark),
            "visible" => Ok(StyleTarget::Visible),
            "both" => Ok(StyleTarget::Both),
            other => Err(UnknownStyleTarget(other.to_owned())),
        }
    }
}

/// What a control client asks the core to do. One per D-Bus method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Rotate now.
    Next(StyleTarget),
    /// Show the image before the current one, from the style's [`History`].
    Previous(StyleTarget),
    /// Stop the style's timer: a user pause, next to any other pause.
    Pause(StyleTarget),
    /// End the user pause. An overdue style rotates at once.
    Resume(StyleTarget),
    /// Read the config file again. The core asks for it with
    /// [`Effect::ReadConfig`](crate::event::Effect::ReadConfig) and replies
    /// [`Reply::Reloaded`] once
    /// [`Event::ConfigRead`](crate::event::Event::ConfigRead) brings it.
    Reload,
    /// Fetch every listed URL now.
    RefreshRemote,
    /// Write the user's original keys back and pause both styles.
    Restore,
    /// Reply with a [`StatusReport`].
    Status,
}

/// Identifies one control request, so that the core's
/// [`Effect::Reply`](crate::event::Effect::Reply) reaches the caller that is
/// waiting. The daemon picks it; the core only copies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReplyId(pub u64);

/// The core's answer to a [`Command`]. Every command gets exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The command was carried out. For `Next` and `Previous`: every
    /// style meant shows another image now, its publish finished.
    Done,
    /// Answer to [`Command::Reload`]: `Err` holds the config error's text,
    /// with line and column.
    Reloaded(Result<(), String>),
    /// Answer to [`Command::Status`].
    Status(StatusReport),
    /// The command cannot be done now; the text says why. The D-Bus server
    /// answers it with `org.freedesktop.DBus.Error.Failed`. A `Next` or
    /// `Previous` for both styles fails when either style could not show
    /// another image, although the other one may have.
    Failed(String),
}

/// Which way a [`Request`] moves through a style's images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// `Next`: the style's order draws the image.
    Forward,
    /// `Previous`: the newest image of the style's [`History`] that can
    /// still be shown.
    Back,
}

/// A `Next` or `Previous` that waits for its rotations to end.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) id: ReplyId,
    pub(crate) direction: Direction,
    /// One entry per style meant, light first.
    pub(crate) styles: Vec<StyleRequest>,
}

/// One style's part of a [`Request`].
#[derive(Debug, Clone)]
pub(crate) struct StyleRequest {
    pub(crate) style: Style,
    pub(crate) stage: RequestStage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestStage {
    /// Waits until nothing is in flight for the style and no earlier
    /// request runs for it.
    Queued,
    /// The request's rotation runs. `before` is the item the style showed
    /// when it started: any other item there ends the request.
    Running { before: Option<ItemId> },
    /// Over: `Err` says why the style shows no other image.
    Finished(Result<(), String>),
}

impl Request {
    pub(crate) fn new(id: ReplyId, direction: Direction, styles: Vec<Style>) -> Request {
        let styles = styles
            .into_iter()
            .map(|style| StyleRequest {
                style,
                stage: RequestStage::Queued,
            })
            .collect();
        Request {
            id,
            direction,
            styles,
        }
    }

    /// Whether the request's rotation of `style` runs.
    pub(crate) fn runs_for(&self, style: Style) -> bool {
        self.styles
            .iter()
            .any(|part| part.style == style && matches!(part.stage, RequestStage::Running { .. }))
    }

    /// The reply, once every style is finished. `None` before.
    pub(crate) fn reply(&self) -> Option<Reply> {
        let mut failures = Vec::new();
        for part in &self.styles {
            match &part.stage {
                RequestStage::Finished(Ok(())) => {}
                RequestStage::Finished(Err(why)) => failures.push(why.as_str()),
                RequestStage::Queued | RequestStage::Running { .. } => return None,
            }
        }
        Some(if failures.is_empty() {
            Reply::Done
        } else {
            Reply::Failed(failures.join("; "))
        })
    }
}

/// The JSON document that `Status()` returns and `gpaperctl status --json`
/// prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// Always [`STATUS_VERSION`] when the core builds it.
    pub version: u32,
    /// The style on screen.
    pub visible: Style,
    /// When the next rotation is due, in seconds since the Unix epoch.
    /// `None` when none is scheduled.
    pub next_change: Option<u64>,
    pub styles: PerStyle<StyleReport>,
}

/// One style in a [`StatusReport`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleReport {
    /// The item in the slot: a path or a URL.
    pub current: Option<ItemId>,
    /// Why the style does not rotate. Empty when it does. Several reasons
    /// can hold at once, for example a user pause and an external change.
    pub paused: Vec<Pause>,
    /// Items in the style's own catalog, usable or not.
    pub items: usize,
    /// Items that may be picked: present and not known to be bad.
    pub usable: usize,
    /// Items rejected by a probe, with the reason.
    pub bad: Vec<BadItem>,
}

/// An item that cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadItem {
    pub id: ItemId,
    /// The [`BadReason`](crate::catalog::BadReason)'s text.
    pub reason: String,
}

/// The images a style showed before its current one, newest last, for
/// `Previous`. Holds at most [`HISTORY_LEN`] entries; the oldest falls out.
///
/// Persisted in `state.json` as a plain list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<ItemId>", into = "Vec<ItemId>")]
pub struct History(VecDeque<ItemId>);

impl History {
    /// Records that `item` left the screen. Drops the oldest entry when full.
    pub fn push(&mut self, item: ItemId) {
        if self.0.len() == HISTORY_LEN {
            self.0.pop_front();
        }
        self.0.push_back(item);
    }

    /// Takes the newest entry: the image to step back to.
    pub fn pop(&mut self) -> Option<ItemId> {
        self.0.pop_back()
    }

    /// Oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &ItemId> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Builds a history from a state file's list, keeping the newest
/// [`HISTORY_LEN`] entries: the file is outside input.
impl From<Vec<ItemId>> for History {
    fn from(items: Vec<ItemId>) -> History {
        let skip = items.len().saturating_sub(HISTORY_LEN);
        History(items.into_iter().skip(skip).collect())
    }
}

impl From<History> for Vec<ItemId> {
    fn from(history: History) -> Vec<ItemId> {
        history.0.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_targets_parse_their_own_names_only() {
        for target in [
            StyleTarget::Light,
            StyleTarget::Dark,
            StyleTarget::Visible,
            StyleTarget::Both,
        ] {
            assert_eq!(target.as_str().parse(), Ok(target));
        }
        for bad in ["", "Light", "all", "light "] {
            assert_eq!(
                bad.parse::<StyleTarget>(),
                Err(UnknownStyleTarget(bad.to_owned()))
            );
        }
    }

    #[test]
    fn history_keeps_the_newest_entries() {
        let mut history = History::default();
        for n in 0..HISTORY_LEN + 3 {
            history.push(ItemId::new(format!("/p/{n}")));
        }
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.iter().next(), Some(&ItemId::new("/p/3")));
        let newest = ItemId::new(format!("/p/{}", HISTORY_LEN + 2));
        assert_eq!(history.pop(), Some(newest));
    }

    #[test]
    fn history_from_an_overlong_list_keeps_its_end() {
        let items: Vec<ItemId> = (0..HISTORY_LEN + 5)
            .map(|n| ItemId::new(format!("/p/{n}")))
            .collect();
        let history = History::from(items);
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.iter().next(), Some(&ItemId::new("/p/5")));
    }
}
