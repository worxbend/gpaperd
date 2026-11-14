//! Rotation order: the shuffle bag for `random`, the sorted cursor for
//! `alphabetical`. See the plan's "Sources, catalog and ordering" section.
//!
//! Both pick only items that are in the catalog and not [`Check::Bad`].
//!
//! [`Check::Bad`]: crate::catalog::Check::Bad

use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::mem;

use rand::seq::SliceRandom;
use rand::{Rng, RngExt};

use crate::catalog::{Catalog, ItemId};

/// Random order: every item is shown once per round.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShuffleBag {
    /// The rest of the current round, next item first.
    queue: VecDeque<ItemId>,
    /// The items the current round showed, oldest first. None of them gets
    /// a second place in the round.
    shown: Vec<ItemId>,
    /// The round was restored without what it showed. Whatever first joins
    /// it is the catalog as the round left it, so it was shown.
    is_shown_unknown: bool,
}

impl ShuffleBag {
    /// A round that holds `queue`, next item first, and showed nothing yet.
    pub fn from_queue(queue: Vec<ItemId>) -> ShuffleBag {
        ShuffleBag::restore(queue, Vec::new())
    }

    /// Restores a round from its persisted halves: the queue, next item
    /// first, and the items it showed, oldest first.
    pub fn restore(queue: Vec<ItemId>, shown: Vec<ItemId>) -> ShuffleBag {
        ShuffleBag {
            queue: queue.into(),
            shown,
            is_shown_unknown: false,
        }
    }

    /// Restores the rest of a round from a persisted queue, next item first,
    /// when what the round showed was not saved: a state file from before
    /// M2. The first [`ShuffleBag::insert`] tells it.
    pub fn restore_queue_only(queue: Vec<ItemId>) -> ShuffleBag {
        ShuffleBag {
            is_shown_unknown: true,
            ..ShuffleBag::from_queue(queue)
        }
    }

    /// The rest of the round, next item first. Persisted so that a restart
    /// continues the round.
    pub fn queue(&self) -> impl Iterator<Item = &ItemId> {
        self.queue.iter()
    }

    /// The items the round showed, oldest first. Persisted so that after a
    /// restart an image can be told from one that is new.
    pub fn shown(&self) -> impl Iterator<Item = &ItemId> {
        self.shown.iter()
    }

    /// Whether [`ShuffleBag::shown`] is all that the round showed. Not so
    /// for a round from [`ShuffleBag::restore_queue_only`] that nothing
    /// joined yet.
    pub fn knows_shown(&self) -> bool {
        !self.is_shown_unknown
    }

    /// Records that `id` was put on screen in this round.
    pub fn mark_shown(&mut self, id: &ItemId) {
        if !self.shown.contains(id) {
            self.shown.push(id.clone());
        }
    }

    /// Takes the next item to show, or `None` when there is none: every
    /// usable item is on screen or in `failed`.
    ///
    /// - Queued ids that are missing from `catalog` or bad are dropped when
    ///   reached. So is an id that the round showed: it got into the queue a
    ///   second time while it was on its way to the screen.
    /// - `on_screen`, which is being shown already, and the ids in `failed`,
    ///   which the rotation in flight gave up on, are passed over and keep
    ///   their places in the round.
    /// - An empty queue is refilled with a fresh random permutation of the
    ///   usable items: a new round, which has shown nothing yet.
    /// - The first item of a new round is never `on_screen`.
    pub fn draw(
        &mut self,
        catalog: &Catalog,
        on_screen: Option<&ItemId>,
        failed: &[ItemId],
        rng: &mut impl Rng,
    ) -> Option<ItemId> {
        if let Some(next) = self.pop_next(catalog, on_screen, failed) {
            return Some(next);
        }
        self.start_round(catalog, on_screen, rng);
        self.pop_next(catalog, on_screen, failed)
    }

    /// Adds catalog items to the round in progress, each at a random
    /// position.
    ///
    /// Does nothing when no round is in progress, because the next round
    /// holds every item anyway. An id that is queued already, or that the
    /// round showed, is left out: no item has two turns in a round.
    ///
    /// The first insert into a round from [`ShuffleBag::restore_queue_only`]
    /// adds nothing: it records the ids as shown, because the round began
    /// with every item that it does not hold.
    pub fn insert(&mut self, ids: impl IntoIterator<Item = ItemId>, rng: &mut impl Rng) {
        if self.queue.is_empty() {
            return;
        }
        let in_round: BTreeSet<&ItemId> = self.queue.iter().chain(&self.shown).collect();
        let newcomers: BTreeSet<ItemId> = ids
            .into_iter()
            .filter(|id| !in_round.contains(id))
            .collect();
        if mem::take(&mut self.is_shown_unknown) {
            self.shown.extend(newcomers);
            return;
        }
        for id in newcomers {
            let position = rng.random_range(0..=self.queue.len());
            self.queue.insert(position, id);
        }
    }

    /// Puts an item that was drawn but not shown back at the front of the
    /// round, so that it is drawn next.
    ///
    /// Does nothing when `id` is queued already: a round that started since
    /// the draw holds it. Nor when the round showed `id`: it had its turn.
    pub fn put_back(&mut self, id: ItemId) {
        if !self.queue.contains(&id) && !self.shown.contains(&id) {
            self.queue.push_front(id);
        }
    }

    fn pop_next(
        &mut self,
        catalog: &Catalog,
        on_screen: Option<&ItemId>,
        failed: &[ItemId],
    ) -> Option<ItemId> {
        let mut passed_over = Vec::new();
        let mut next = None;
        while let Some(id) = self.queue.pop_front() {
            let had_its_turn = self.shown.contains(&id);
            if had_its_turn || !catalog.is_candidate(&id) {
                continue;
            }
            if Some(&id) == on_screen || failed.contains(&id) {
                passed_over.push(id);
                continue;
            }
            next = Some(id);
            break;
        }
        for id in passed_over.into_iter().rev() {
            self.queue.push_front(id);
        }
        next
    }

    fn start_round(&mut self, catalog: &Catalog, on_screen: Option<&ItemId>, rng: &mut impl Rng) {
        let mut round: Vec<ItemId> = catalog.candidates().map(|item| item.id.clone()).collect();
        round.shuffle(rng);
        if round.len() > 1 && round.first() == on_screen {
            let later = rng.random_range(1..round.len());
            round.swap(0, later);
        }
        self.queue = round.into();
        self.shown.clear();
        self.is_shown_unknown = false;
    }
}

/// Alphabetical order: the first usable item sorting strictly after `cursor`
/// by [`compare_ids`], wrapping to the first item. `None` when the catalog has
/// no usable item.
///
/// `cursor` is the id of the current item and need not be in the catalog, so
/// a deleted or renamed current item does not restart the list. With no
/// cursor the first item is returned.
pub fn next_alphabetical(catalog: &Catalog, cursor: Option<&ItemId>) -> Option<ItemId> {
    let ids = || catalog.candidates().map(|item| &item.id);
    let after_cursor = cursor.and_then(|cursor| {
        ids()
            .filter(|id| compare_ids(id, cursor) == Ordering::Greater)
            .min_by(|a, b| compare_ids(a, b))
    });
    let first = || ids().min_by(|a, b| compare_ids(a, b));
    after_cursor.or_else(first).cloned()
}

/// Sort order of two ids: natural and case-insensitive on the file name (the
/// text after the last `/`), ties broken by the whole id.
///
/// It takes ids, not items, because the cursor may name a file that is gone.
pub fn compare_ids(a: &ItemId, b: &ItemId) -> Ordering {
    natord::compare_ignore_case(file_name(a), file_name(b)).then_with(|| a.cmp(b))
}

fn file_name(id: &ItemId) -> &str {
    let id = id.as_str();
    id.rsplit_once('/').map_or(id, |(_, name)| name)
}
