//! Remote images in the reducer: which URLs are cached, and when each one
//! is fetched. Plain bookkeeping; `step` turns it into effects.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use url::Url;

use crate::catalog::RemoteBlob;
use crate::config::RemoteRefresh;
use crate::event::FetchOutcome;
use crate::step::{GC_PERIOD, MAX_FETCHES_IN_FLIGHT, no_later_than};

/// What the reducer knows of the remote cache and of the fetches.
///
/// Until the cache is loaded, which URLs are cached is unknown, nothing is
/// fetched and no garbage is collected. From then on every URL that the
/// running config lists has a [`UrlState`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Remotes {
    is_loaded: bool,
    /// The blob of every cached URL, listed in the config or not: a reload
    /// may list it again.
    blobs: BTreeMap<Url, PathBuf>,
    /// Every URL that the running config lists, once the cache is loaded.
    urls: BTreeMap<Url, UrlState>,
    /// URLs that wait for a place among the fetches in flight, oldest
    /// first.
    queue: VecDeque<Url>,
    /// When garbage was collected last in this run.
    last_gc: Option<SystemTime>,
    /// Cached URLs that a garbage collection did not keep: their entry may
    /// be deleted, so the blob the reducer knows may be gone.
    collectable: BTreeSet<Url>,
}

/// One listed URL's fetches.
#[derive(Debug, Clone, Default)]
struct UrlState {
    stage: Stage,
    /// When a fetch of it was last answered with anything but a failure,
    /// in this run.
    revalidated_at: Option<SystemTime>,
    /// The backoff after a failed fetch, until the next answer.
    retry: Option<Retry>,
    /// The last answer was `404` or `410`.
    is_gone: bool,
    /// The blob was lost while a fetch was in flight. That fetch may have
    /// asked for a `304` against the lost blob, so it does not bring the
    /// blob back.
    refetch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    /// No fetch is asked for.
    #[default]
    Idle,
    /// In [`Remotes::queue`].
    Queued,
    /// An `Effect::Fetch` is unanswered.
    InFlight,
}

#[derive(Debug, Clone, Copy)]
struct Retry {
    /// When to fetch again. `None` when that is too far ahead for the
    /// clock: then only full connectivity brings the next fetch.
    due: Option<SystemTime>,
    /// The whole backoff, for the clock-went-back rule.
    delay: Duration,
    /// The server was not reached: full connectivity ends the backoff.
    ends_on_reconnect: bool,
}

impl UrlState {
    /// When the URL is to be fetched next: at the end of its backoff, or a
    /// refresh period after its last answer. `None` while a fetch is asked
    /// for, and when nothing is planned.
    fn due(&self, policy: RemoteRefresh) -> Option<SystemTime> {
        if self.stage != Stage::Idle {
            return None;
        }
        if let Some(retry) = self.retry {
            return retry.due;
        }
        match (policy, self.revalidated_at) {
            (RemoteRefresh::Every(period), Some(answered)) => answered.checked_add(period),
            // Not revalidated in this run yet: at once.
            (RemoteRefresh::Every(_), None) => Some(SystemTime::UNIX_EPOCH),
            (RemoteRefresh::Startup | RemoteRefresh::Never, _) => None,
        }
    }
}

impl Remotes {
    /// Whether `CacheLoaded` came.
    pub(crate) fn is_loaded(&self) -> bool {
        self.is_loaded
    }

    /// Takes what the cache held at start, and plans the first fetch of
    /// every URL in `listed`. Returns `false` for a second report, which
    /// changes nothing.
    pub(crate) fn load(
        &mut self,
        blobs: Vec<RemoteBlob>,
        listed: &[Url],
        policy: RemoteRefresh,
    ) -> bool {
        if self.is_loaded {
            return false;
        }
        self.is_loaded = true;
        self.blobs = blobs
            .into_iter()
            .map(|remote| (remote.url, remote.blob))
            .collect();
        self.follow_config(listed, policy);
        true
    }

    /// Makes `listed` the URLs of the running config. A new one is fetched
    /// as at start: always, except with `never` when it is cached. Returns
    /// the URLs that are no longer listed, whose fetch and backoff are to be
    /// dropped. Before the cache is loaded nothing is planned.
    pub(crate) fn follow_config(&mut self, listed: &[Url], policy: RemoteRefresh) -> Vec<Url> {
        if !self.is_loaded {
            return Vec::new();
        }
        let left: Vec<Url> = self
            .urls
            .keys()
            .filter(|url| !listed.contains(url))
            .cloned()
            .collect();
        for url in &left {
            self.urls.remove(url);
        }
        let urls = &self.urls;
        self.queue.retain(|url| urls.contains_key(url));
        for url in listed {
            if self.urls.contains_key(url) {
                continue;
            }
            self.urls.insert(url.clone(), UrlState::default());
            let may_be_collected = self.collectable.remove(url);
            if policy != RemoteRefresh::Never || !self.blobs.contains_key(url) || may_be_collected {
                self.enqueue(url);
            }
        }
        left
    }

    /// Records the answer to the fetch of `url`. Returns whether the URL
    /// has another blob now. An answer that no fetch waits for is ignored.
    pub(crate) fn answer(&mut self, url: &Url, outcome: FetchOutcome, now: SystemTime) -> bool {
        let Some(state) = self
            .urls
            .get_mut(url)
            .filter(|state| state.stage == Stage::InFlight)
        else {
            return false;
        };
        state.stage = Stage::Idle;
        // A failure's backoff ends in a fetch that sees the loss anyway.
        let refetch = std::mem::take(&mut state.refetch);
        let failure = match outcome {
            FetchOutcome::Unreachable { retry_in } => Some((retry_in, true)),
            FetchOutcome::Failed { retry_in } => Some((retry_in, false)),
            FetchOutcome::Stored { .. } | FetchOutcome::NotModified | FetchOutcome::Gone => None,
        };
        if let Some((retry_in, ends_on_reconnect)) = failure {
            state.retry = Some(Retry {
                due: now.checked_add(retry_in),
                delay: retry_in,
                ends_on_reconnect,
            });
            return false;
        }
        state.retry = None;
        state.revalidated_at = Some(now);
        state.is_gone = outcome == FetchOutcome::Gone;
        match outcome {
            FetchOutcome::Stored { blob } => {
                self.blobs.insert(url.clone(), blob.clone()) != Some(blob)
            }
            FetchOutcome::NotModified if refetch => {
                self.enqueue(url);
                false
            }
            _ => false,
        }
    }

    /// `remote`'s blob is no longer in the cache. Returns whether that was
    /// its URL's blob. Then the URL is fetched again: now, or after the
    /// answer to the fetch in flight when that answer is a `304`.
    pub(crate) fn lose_blob(&mut self, remote: &RemoteBlob) -> bool {
        if self.blobs.get(&remote.url) != Some(&remote.blob) {
            return false;
        }
        self.blobs.remove(&remote.url);
        if let Some(state) = self.urls.get_mut(&remote.url)
            && state.stage == Stage::InFlight
        {
            state.refetch = true;
        }
        self.enqueue(&remote.url);
        true
    }

    /// Connectivity became full: every URL whose last fetch did not reach
    /// its server is fetched again now. A backoff that the server asked
    /// for runs on.
    pub(crate) fn retry_failed(&mut self) {
        let failed: Vec<Url> = self
            .urls
            .iter()
            .filter(|(_, state)| {
                state.stage == Stage::Idle
                    && state.retry.is_some_and(|retry| retry.ends_on_reconnect)
            })
            .map(|(url, _)| url.clone())
            .collect();
        for url in &failed {
            self.enqueue(url);
        }
    }

    /// `gpaperctl refresh`: queues every listed URL that no fetch is asked
    /// for yet, backoff or not.
    pub(crate) fn refresh_all(&mut self) {
        let listed: Vec<Url> = self.urls.keys().cloned().collect();
        for url in &listed {
            self.enqueue(url);
        }
    }

    /// Queues every URL whose fetch is due at `now`, then takes as many
    /// queued URLs as there is room for in flight, oldest first. Returns
    /// the URLs to fetch now.
    pub(crate) fn start_due_fetches(&mut self, now: SystemTime, policy: RemoteRefresh) -> Vec<Url> {
        let due: Vec<Url> = self
            .urls
            .iter()
            .filter(|(_, state)| state.due(policy).is_some_and(|due| due <= now))
            .map(|(url, _)| url.clone())
            .collect();
        for url in &due {
            self.enqueue(url);
        }
        let in_flight = self
            .urls
            .values()
            .filter(|state| state.stage == Stage::InFlight);
        let room = MAX_FETCHES_IN_FLIGHT.saturating_sub(in_flight.count());
        let started: Vec<Url> = self.queue.drain(..room.min(self.queue.len())).collect();
        for url in &started {
            if let Some(state) = self.urls.get_mut(url) {
                state.stage = Stage::InFlight;
            }
        }
        started
    }

    /// When the next fetch is due that is not queued yet.
    pub(crate) fn next_fetch(&self, policy: RemoteRefresh) -> Option<SystemTime> {
        self.urls
            .values()
            .filter_map(|state| state.due(policy))
            .min()
    }

    /// When garbage is to be collected: at once once the cache is loaded,
    /// and then [`GC_PERIOD`] after the last time, for as long as any
    /// remote image is known. `None` before the cache is loaded.
    pub(crate) fn garbage_due(&self) -> Option<SystemTime> {
        let knows_an_image = !self.urls.is_empty() || !self.blobs.is_empty();
        match self.last_gc {
            _ if !self.is_loaded => None,
            None => Some(SystemTime::UNIX_EPOCH),
            Some(last) if knows_an_image => last.checked_add(GC_PERIOD),
            Some(_) => None,
        }
    }

    /// A garbage collection that keeps `keep` is asked for at `now`. It may
    /// delete the entry of any other cached URL.
    pub(crate) fn record_garbage_collection(&mut self, now: SystemTime, keep: &[Url]) {
        self.last_gc = Some(now);
        let collectable = self.blobs.keys().filter(|url| !keep.contains(url));
        self.collectable.extend(collectable.cloned());
    }

    /// The clock-went-back rule: no backoff ends further ahead than its
    /// whole delay, and no refresh period or day counts from the future.
    pub(crate) fn absorb_clock_step(&mut self, now: SystemTime) {
        for state in self.urls.values_mut() {
            if let Some(retry) = &mut state.retry {
                retry.due = retry.due.map(|due| no_later_than(due, now, retry.delay));
            }
            state.revalidated_at = state.revalidated_at.map(|answered| answered.min(now));
        }
        self.last_gc = self.last_gc.map(|last| last.min(now));
    }

    /// The cached version of `url`, when there is one.
    pub(crate) fn cached(&self, url: &Url) -> Option<RemoteBlob> {
        let blob = self.blobs.get(url)?;
        Some(RemoteBlob {
            url: url.clone(),
            blob: blob.clone(),
        })
    }

    /// Whether `url` may still bring a first image: it has no blob, and its
    /// last answer did not say that it is gone.
    pub(crate) fn awaits_blob(&self, url: &Url) -> bool {
        let is_gone = self.urls.get(url).is_some_and(|state| state.is_gone);
        !self.blobs.contains_key(url) && !is_gone
    }

    /// Puts an idle listed URL at the end of the queue.
    fn enqueue(&mut self, url: &Url) {
        if let Some(state) = self.urls.get_mut(url)
            && state.stage == Stage::Idle
        {
            state.stage = Stage::Queued;
            self.queue.push_back(url.clone());
        }
    }
}
