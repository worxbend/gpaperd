//! Effect interpreter and event loop: feeds events to the core's `step` and
//! executes the effects it returns.
//!
//! One task owns the [`Model`]. Adapters turn the outside world into
//! [`Event`]s, and every [`Effect`] runs through an adapter. A probe and a
//! slot write run off the event loop, and the answer comes back as an
//! event. A scan runs on the thread of its [`SourceWatch`]. A fetch of a
//! remote image runs as a task of its own. So a stalled network mount or a
//! slow server cannot hold up a style switch or a shutdown.
//!
//! A state save runs on the blocking pool, but the loop awaits it, so a
//! healthy save finishes before a key write that waits on it. The loop
//! reads no event while a save runs: a stalled `fsync` delays the next
//! event, a shutdown included.
//!
//! Control requests from the D-Bus server come in through the
//! [`ControlPort`]. Each one gets a [`ReplyId`] from [`PendingReplies`],
//! which keeps its reply channel until the core's `Effect::Reply` answers
//! it; the core itself never sees a channel.

use std::collections::HashMap;
use std::future::{self, Future};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::SystemTime;
use std::{io, mem, panic};

use gpaperd_core::catalog::{BadReason, FileId, FsChange, ItemId, RemoteBlob};
use gpaperd_core::config::{Config, ConfigError, LinkPolicy, Source};
use gpaperd_core::control::ReplyId;
use gpaperd_core::event::{
    BackgroundKeys, Effect, Event, FetchOutcome, Fired, GnomeEvent, ScreenLock, Status,
};
use gpaperd_core::model::{Model, ModelInit, Persisted};
use gpaperd_core::step::step;
use gpaperd_core::style::{PerStyle, Style};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use url::Url;

use crate::cache::Cache;
use crate::config_watch::{ConfigFile, ConfigWatch, LoadError};
use crate::dbus::{BusNews, ControlPort, ControlRequest, PendingReplies};
use crate::fetch::{FetchSettings, Fetcher};
use crate::gnome::{
    GnomeError, GnomePort, KeyWrite, PICTURE_OPTIONS, PICTURE_URI, PICTURE_URI_DARK,
};
use crate::paths::Paths;
use crate::probe::Prober;
use crate::scan::Scan;
use crate::timer::WallTimer;
use crate::watch::{SlotsWatch, SourceNews, SourceWatch};
use crate::{fetch, slots, state};

/// The wall clock and the one alarm set on it.
///
/// A port, so that tests can move time by hand. The real one is
/// [`WallTimer`].
pub trait Clock {
    fn now(&self) -> SystemTime;

    /// Sets the alarm, replacing any earlier one. `None` clears it.
    fn arm(&self, deadline: Option<SystemTime>) -> io::Result<()>;

    /// Waits until the alarm goes off or the clock is stepped. Must be
    /// cancel-safe: the event loop drops this future when another event
    /// comes first.
    fn wait(&self) -> impl Future<Output = io::Result<Fired>>;
}

impl Clock for WallTimer {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn arm(&self, deadline: Option<SystemTime>) -> io::Result<()> {
        WallTimer::arm(self, deadline)
    }

    fn wait(&self) -> impl Future<Output = io::Result<Fired>> {
        WallTimer::wait(self)
    }
}

/// News for the service manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The first apply is settled: `READY=1`.
    Ready,
    /// The status changed: `STATUS=`.
    Status(Status),
}

/// The interpreter's way to everything but the file system.
pub struct Ports<G, C> {
    pub gnome: G,
    pub clock: C,
    /// Called with each [`Notice`] as it happens.
    pub notify: Box<dyn FnMut(Notice)>,
    /// How the HTTP client that fetches remote images is built.
    pub fetch: FetchSettings,
    /// The control interface: [`ControlPort::detached`] without a bus.
    pub control: ControlPort,
    /// Each screen lock and unlock, from [`crate::screensaver::watch`]:
    /// [`crate::screensaver::detached`] without a bus.
    pub screen_lock: mpsc::UnboundedReceiver<ScreenLock>,
}

/// Why the interpreter cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("HTTP client for remote images cannot be built: {0}")]
    HttpClient(#[from] reqwest::Error),
    /// An existing state file could not be read. It is left in place.
    #[error("state file cannot be read: {0}")]
    State(#[from] state::StateError),
}

/// Why the event loop ended before a shutdown was asked for.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("wall-clock timer failed: {0}")]
    Clock(#[from] io::Error),
    #[error(transparent)]
    Gnome(#[from] GnomeError),
}

/// The daemon's event loop: owns the model and executes its effects.
pub struct Interpreter<G, C> {
    model: Model,
    paths: Paths,
    ports: Ports<G, C>,
    /// Probe and slot-write answers on their way back to the event loop.
    answers: mpsc::UnboundedReceiver<Event>,
    answer_sender: mpsc::UnboundedSender<Event>,
    /// One for the run: it remembers what glycin said about each file.
    prober: Arc<Prober>,
    sources: SourceWatches,
    slots_watch: SlotsWatch,
    /// Whether the state file holds the user's original keys. Once the keys
    /// are replaced, that file is their only copy.
    originals_saved: bool,
    /// `false` after the screen-lock channel closes. One close reports
    /// unlocked; further polls would be `None` every time.
    screen_lock_open: bool,
    /// The remote cache, shared with the fetches and the garbage collector.
    cache: Arc<Cache>,
    fetcher: Arc<Fetcher>,
    fetches: Fetches,
    /// What the cache held at start, until the core is told of it.
    cached: Vec<RemoteBlob>,
    /// The reply channels of the control requests the core has not
    /// answered yet.
    pending: PendingReplies,
}

/// What a run starts from, as read from disk.
struct Groundwork {
    model: Model,
    /// Whether the state file already holds the user's original keys.
    originals_saved: bool,
    slots_watch: SlotsWatch,
    cached: Vec<RemoteBlob>,
}

/// The fetches in flight, and their answers on the way back.
///
/// Each fetch has an id, so that an answer that arrives after its fetch was
/// dropped is never passed on, even when the URL is fetched again since.
struct Fetches {
    running: HashMap<Url, RunningFetch>,
    next_id: u64,
    answers: mpsc::UnboundedReceiver<FetchAnswer>,
    answer_sender: mpsc::UnboundedSender<FetchAnswer>,
}

struct RunningFetch {
    id: u64,
    task: AbortHandle,
}

struct FetchAnswer {
    id: u64,
    url: Url,
    outcome: FetchOutcome,
}

impl Fetches {
    fn new() -> Fetches {
        let (answer_sender, answers) = mpsc::unbounded_channel();
        Fetches {
            running: HashMap::new(),
            next_id: 0,
            answers,
            answer_sender,
        }
    }

    /// Fetches `url` off the event loop. The core asks for no second fetch
    /// of a URL while one is in flight, and for at most four at once; the
    /// fetcher's own limit stands behind that.
    fn start(&mut self, fetcher: Arc<Fetcher>, url: Url) {
        let id = self.next_id;
        self.next_id += 1;
        let work = {
            let url = url.clone();
            tokio::spawn(async move { fetcher.fetch(&url).await })
        };
        self.running.insert(
            url.clone(),
            RunningFetch {
                id,
                task: work.abort_handle(),
            },
        );
        let answers = self.answer_sender.clone();
        tokio::spawn(async move {
            let outcome = match work.await {
                Ok(outcome) => outcome,
                // Dropped: nobody waits for an answer.
                Err(failure) if failure.is_cancelled() => return,
                // The core waits for exactly one answer per fetch, so a
                // fetch that panics answers too.
                Err(failure) => {
                    tracing::error!(url = %fetch::redacted(&url), reason = %failure, "fetch panicked");
                    FetchOutcome::Failed {
                        retry_in: fetch::BACKOFF_CEILING,
                    }
                }
            };
            // Nobody listens once the event loop has ended.
            let _ = answers.send(FetchAnswer { id, url, outcome });
        });
    }

    /// Aborts the fetch of `url`, if one is in flight. Its answer, should
    /// it be on the way already, is not passed on.
    fn drop_fetch(&mut self, url: &Url) {
        if let Some(running) = self.running.remove(url) {
            running.task.abort();
        }
    }

    /// Waits for the answer of a fetch that is still in flight. Cancel-safe.
    async fn next(&mut self) -> Event {
        loop {
            let answer = self
                .answers
                .recv()
                .await
                .expect("`Fetches` holds a sender of its own channel");
            let awaited = self
                .running
                .get(&answer.url)
                .is_some_and(|running| running.id == answer.id);
            if awaited {
                self.running.remove(&answer.url);
                return Event::Fetched {
                    url: answer.url,
                    outcome: answer.outcome,
                };
            }
        }
    }
}

/// The watch on each style's local sources, as the core last asked for it.
#[derive(Default)]
struct SourceWatches {
    /// `None` until the core asks for the style's first watch.
    watches: PerStyle<Option<SourceWatch>>,
    /// How many image files the scan of each watch found. `None` while that
    /// scan is still to come.
    images_found: PerStyle<Option<usize>>,
    /// Whether the watch the core asked for includes a [`Source::Url`].
    /// A URL is not a file the scan can count.
    has_url: PerStyle<bool>,
}

impl SourceWatches {
    /// Scans and watches `sources` for `style` from now on. Dropping the
    /// earlier watch stops it and discards its unread news.
    fn replace(&mut self, style: Style, sources: Vec<Source>) {
        self.has_url[style] = sources
            .iter()
            .any(|source| matches!(source, Source::Url(_)));
        self.watches[style] = Some(SourceWatch::start(sources));
        self.images_found[style] = None;
    }

    /// Waits for the next news of either style. Cancel-safe.
    async fn next(&mut self) -> (Style, SourceNews) {
        let PerStyle { light, dark } = &mut self.watches;
        let (style, news) = tokio::select! {
            news = next_news(light) => (Style::Light, news),
            news = next_news(dark) => (Style::Dark, news),
        };
        if let SourceNews::Scanned(scan) = &news {
            self.record_scan(style, scan);
        }
        (style, news)
    }

    /// Records the scan of `style`'s watch. Once both scans are in and found
    /// nothing, and no watched style has a URL, the daemon has nothing to
    /// show: that is worth one warning. A URL-only style is not empty.
    fn record_scan(&mut self, style: Style, scan: &Scan) {
        self.images_found[style] = Some(scan.files.len());
        let every_scan_empty = Style::ALL
            .iter()
            .all(|&scanned| self.images_found[scanned] == Some(0));
        let any_url = Style::ALL.iter().any(|&watched| self.has_url[watched]);
        if every_scan_empty && !any_url {
            tracing::warn!("no image found in any source: the wallpaper stays as it is");
        }
    }
}

/// The next news of a watch. Waits forever where there is no watch yet.
async fn next_news(watch: &mut Option<SourceWatch>) -> SourceNews {
    match watch {
        Some(watch) => watch.next().await,
        None => future::pending().await,
    }
}

impl<G: GnomePort, C: Clock> Interpreter<G, C> {
    /// Prepares a run: removes the temp files of a crashed publish, loads the
    /// state file and the remote cache's `meta.toml` files, and starts to
    /// watch the slots directory. No source is scanned and nothing is
    /// fetched yet: the first apply uses the items that the state file
    /// names and the cached remote images, and each scan and fetch arrives
    /// later as an event.
    ///
    /// That is blocking I/O, so it runs on the blocking pool: the caller can
    /// serve a stop request while it waits. Must run inside a tokio runtime
    /// that has its I/O driver and its timers enabled, which the image check
    /// needs (see [`Prober::check`]).
    ///
    /// I/O on an existing state file is fatal: that file is the only copy of
    /// the user's original keys, and a failed read must not be replaced. A
    /// corrupt file or an unsupported version is logged and the daemon
    /// starts without state; the next save replaces it. A missing file is
    /// no state.
    ///
    /// `config` is the running config until `run` is told of another. `None`
    /// when there is no config file: the daemon then idles, and touches no
    /// slot and no key until the file appears.
    ///
    /// `seed` seeds the shuffle of a round that the state file does not
    /// carry over, and the jitter of the fetches' backoff.
    ///
    /// # Errors
    ///
    /// [`StartError::HttpClient`] when the HTTP client cannot be built from
    /// `ports.fetch`: its TLS backend failed to start.
    /// [`StartError::State`] when an existing state file cannot be read.
    ///
    /// # Panics
    ///
    /// When `paths.slots_dir` is not absolute.
    pub async fn start(
        config: Option<Config>,
        paths: Paths,
        seed: u64,
        ports: Ports<G, C>,
    ) -> Result<Interpreter<G, C>, StartError> {
        let cache = Arc::new(Cache::new(paths.remote_cache_dir.clone()));
        let groundwork = {
            let paths = paths.clone();
            let cache = Arc::clone(&cache);
            let settings = ports.fetch.clone();
            tokio::task::spawn_blocking(move || -> Result<_, StartError> {
                // Building the client reads the system's TLS roots.
                let fetcher = Fetcher::new(Arc::clone(&cache), settings, seed)?;
                Ok((lay_groundwork(config, &paths, &cache, seed)?, fetcher))
            })
        };
        // Only a panic ends that task early. A panic is a defect: pass it on.
        let (
            Groundwork {
                model,
                originals_saved,
                slots_watch,
                cached,
            },
            fetcher,
        ) = groundwork
            .await
            .unwrap_or_else(|failure| panic::resume_unwind(failure.into_panic()))?;
        let (answer_sender, answers) = mpsc::unbounded_channel();
        Ok(Interpreter {
            model,
            paths,
            ports,
            answers,
            answer_sender,
            prober: Arc::new(Prober::new()),
            sources: SourceWatches::default(),
            slots_watch,
            originals_saved,
            screen_lock_open: true,
            cache,
            fetcher: Arc::new(fetcher),
            fetches: Fetches::new(),
            cached,
            pending: PendingReplies::default(),
        })
    }

    /// Runs the daemon until `shutdown` completes, then saves the state.
    ///
    /// The first apply comes first: each slot that the state file names an
    /// image for is filled when it needs it, and the key of each slot that
    /// exists is pointed at it. The cached remote images join right after.
    /// Then [`Notice::Ready`] goes out; it never waits for a fetch. The
    /// scans of the sources and the fetches come in afterwards. From then on
    /// the loop sleeps until the alarm, a change to GNOME, to a source, to a
    /// slot file or to the config file, a fetch's answer, a control
    /// request, a screen lock or unlock, or `shutdown` wakes it.
    ///
    /// `config_watch` must watch the file that the running config was loaded
    /// from. A file that is gone or cannot be read is logged and leaves the
    /// running config in place, as an invalid one does. `gpaperctl reload`
    /// loads that same file again.
    ///
    /// Must run inside the tokio runtime of [`Interpreter::start`].
    ///
    /// # Errors
    ///
    /// [`RunError::Gnome`] when GNOME cannot be read at start or when
    /// `gnome_events` closes, because the daemon can no longer follow the
    /// style. [`RunError::Clock`] when the alarm fails.
    pub async fn run(
        mut self,
        mut gnome_events: mpsc::UnboundedReceiver<GnomeEvent>,
        mut config_watch: ConfigWatch,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), RunError> {
        let config_file = config_watch.file().clone();
        let started = self.started().await?;
        self.dispatch(started, &config_file).await?;
        let cached = mem::take(&mut self.cached);
        self.dispatch(Event::CacheLoaded(cached), &config_file)
            .await?;
        let mut shutdown = pin!(shutdown);
        loop {
            let listen_for_lock = self.screen_lock_open;
            let mut lock_watch_stopped = false;
            let event = tokio::select! {
                fired = self.ports.clock.wait() => Event::Fired(fired?),
                Some(answer) = self.answers.recv() => answer,
                fetched = self.fetches.next() => fetched,
                change = gnome_events.recv() => Event::Gnome(change.ok_or(GnomeError::Stopped)?),
                (style, news) = self.sources.next() => catalog_event(style, news),
                style = self.slots_watch.lost() => Event::SlotLost(style),
                Some(request) = self.ports.control.requests.recv() => self.control_event(request),
                lock = self.ports.screen_lock.recv(), if listen_for_lock => match lock {
                    Some(lock) => Event::ScreenLock(lock),
                    // A closed watch must not freeze the last lock. One
                    // unlock, then this arm stays off: a closed channel is
                    // ready on every poll.
                    None => {
                        lock_watch_stopped = true;
                        Event::ScreenLock(ScreenLock::Unlocked)
                    }
                },
                loaded = config_watch.reloaded() => {
                    match config_event(&self.paths.config_file, loaded) {
                        Some(reload) => reload,
                        None => continue,
                    }
                }
                () = &mut shutdown => Event::Shutdown,
            };
            if lock_watch_stopped {
                self.screen_lock_open = false;
                tracing::warn!("screen lock watch stopped");
            }
            let is_last = event == Event::Shutdown;
            self.dispatch(event, &config_file).await?;
            if is_last {
                return Ok(());
            }
        }
    }

    /// The first event: GNOME's state and the slot files as they are before
    /// the daemon changes anything.
    async fn started(&self) -> Result<Event, GnomeError> {
        let gnome = self.ports.gnome.snapshot().await?;
        let slots = PerStyle::from_fn(|style| slot_on_disk(&self.paths.slot(style)));
        Ok(Event::Started { gnome, slots })
    }

    /// The event for a control request. Its reply channel waits in
    /// `pending` until the core answers.
    fn control_event(&mut self, request: ControlRequest) -> Event {
        let ControlRequest { command, reply } = request;
        tracing::info!(?command, "control request");
        Event::Control {
            id: self.pending.register(reply),
            command,
        }
    }

    /// Feeds one event to the core and executes the effects it returns, in
    /// order. `config_file` is what `gpaperctl reload` reads.
    ///
    /// A config that is about to be applied is preceded by GNOME's key locks
    /// as they are now: a backend may change a lock without a signal, and
    /// the new config may depend on it.
    async fn dispatch(&mut self, event: Event, config_file: &ConfigFile) -> Result<(), RunError> {
        log_event(&event);
        if applies_a_config(&event) {
            let locked = self.ports.gnome.snapshot().await?.locked;
            let locks = Event::Gnome(GnomeEvent::LocksChanged(locked));
            self.feed(locks, config_file).await?;
        }
        self.feed(event, config_file).await
    }

    /// Steps the core with `event` and executes its effects, in order.
    async fn feed(&mut self, event: Event, config_file: &ConfigFile) -> Result<(), RunError> {
        let shutting_down = matches!(event, Event::Shutdown);
        let effects = step(&mut self.model, self.ports.clock.now(), event);
        if shutting_down {
            self.execute_shutdown(effects, config_file).await
        } else {
            for effect in effects {
                self.execute(effect, config_file).await?;
            }
            Ok(())
        }
    }

    /// Shutdown is `Arm(None)` then `Persist`. A failed disarm is logged and
    /// the save still runs. Any other arm error still fails the loop.
    async fn execute_shutdown(
        &mut self,
        effects: Vec<Effect>,
        config_file: &ConfigFile,
    ) -> Result<(), RunError> {
        for effect in effects {
            match effect {
                Effect::Arm(None) => {
                    if let Err(error) = self.ports.clock.arm(None) {
                        tracing::error!(reason = %error, "shutdown did not disarm the timer");
                    }
                }
                other => self.execute(other, config_file).await?,
            }
        }
        Ok(())
    }

    async fn execute(&mut self, effect: Effect, config_file: &ConfigFile) -> Result<(), RunError> {
        match effect {
            Effect::Probe {
                style,
                item,
                path,
                max_pixels,
            } => self.probe_later(style, item, path, max_pixels),
            Effect::Publish {
                style,
                item,
                path,
                link,
            } => self.publish_later(style, item, path, link),
            Effect::SetKeys {
                uris,
                picture_options,
            } => {
                let write = KeyWrite {
                    uris,
                    picture_options,
                };
                self.set_keys_once_saved(write).await;
            }
            Effect::Republish(style) => self.republish_later(style),
            Effect::Watch { style, sources } => self.sources.replace(style, sources),
            Effect::Arm(deadline) => self.ports.clock.arm(deadline)?,
            Effect::Persist => self.persist().await,
            Effect::Ready => (self.ports.notify)(Notice::Ready),
            Effect::Report(status) => {
                self.ports.control.tell(BusNews::Status(status.clone()));
                (self.ports.notify)(Notice::Status(status));
            }
            Effect::Fetch(url) => self.fetches.start(Arc::clone(&self.fetcher), url),
            Effect::DropRemote(url) => {
                self.fetches.drop_fetch(&url);
                self.fetcher.forget(&url);
            }
            Effect::CollectGarbage { keep } => self.collect_garbage_later(keep),
            Effect::Reply { id, reply } => self.pending.answer(id, reply),
            Effect::Changed { style, item } => {
                self.ports.control.tell(BusNews::Changed { style, item });
            }
            Effect::ReadConfig(id) => self.read_config_later(id, config_file.clone()),
            Effect::RestoreKeys { id, keys } => self.restore_keys(id, keys, config_file).await?,
        }
        Ok(())
    }

    /// Collects the remote cache's garbage on the blocking pool. Nothing
    /// waits for it; the outcome is logged. Skipped when this process does
    /// not own the bus name: another gpaperd may be sweeping the same cache.
    fn collect_garbage_later(&self, keep: Vec<Url>) {
        if self.ports.control.news.is_closed() {
            tracing::warn!("remote cache not collected: this process does not own the bus name");
            return;
        }
        let cache = Arc::clone(&self.cache);
        let now = self.ports.clock.now();
        tokio::task::spawn_blocking(move || match cache.collect_garbage(&keep, now) {
            Ok(report) => tracing::info!(
                entries = report.entries,
                blobs = report.blobs,
                "remote cache garbage collected"
            ),
            Err(error) => tracing::warn!(reason = %error, "remote cache garbage not collected"),
        });
    }

    /// Loads the config file again off the event loop, for `gpaperctl
    /// reload`, and answers with exactly one `Event::ConfigRead`.
    fn read_config_later(&self, id: ReplyId, file: ConfigFile) {
        let path = self.paths.config_file.clone();
        let read = tokio::task::spawn_blocking(move || file.load());
        self.answer_later(read, move |loaded| Event::ConfigRead {
            id,
            result: reread_config(&path, loaded),
        });
    }

    /// Writes the user's original keys back for `gpaperctl restore`, and
    /// feeds exactly one `Event::KeysRestored` before returning to the
    /// loop. The reply is then sent before a shutdown queued on the same
    /// select can answer `Failed`. The write is awaited here, so that no
    /// later key write overtakes it. `KeysRestored` does not emit
    /// `RestoreKeys`, so the feed does not re-enter this method. The feed
    /// is boxed: `feed` reaches here through `execute`, and an async cycle
    /// needs a heap step to have a finite size.
    async fn restore_keys(
        &mut self,
        id: ReplyId,
        keys: BackgroundKeys,
        config_file: &ConfigFile,
    ) -> Result<(), RunError> {
        let result = self.ports.gnome.restore_keys(keys).await;
        match &result {
            Ok(()) => tracing::info!("the user's background keys are restored"),
            Err(error) => {
                tracing::error!(reason = %error, "the user's background keys not restored");
            }
        }
        let restored = Event::KeysRestored {
            id,
            result: result.map_err(|error| error.to_string()),
        };
        Box::pin(self.feed(restored, config_file)).await
    }

    fn probe_later(&self, style: Style, item: ItemId, path: PathBuf, max_pixels: u64) {
        let prober = Arc::clone(&self.prober);
        // The check moves its own blocking I/O off this thread.
        let check = tokio::spawn(async move { prober.check(&path, max_pixels).await });
        self.answer_later(check, move |verdict| Event::Probed {
            style,
            item,
            verdict: verdict.map_err(BadReason::Unreadable).flatten(),
        });
    }

    fn publish_later(&self, style: Style, item: ItemId, path: PathBuf, link: LinkPolicy) {
        let slot = self.paths.slot(style);
        let write = tokio::task::spawn_blocking(move || {
            slots::publish(&path, &slot, link).map_err(|error| error.to_string())
        });
        self.answer_later(write, move |result| Event::Published {
            style,
            item,
            result: result.flatten(),
        });
    }

    fn republish_later(&self, style: Style) {
        let slot = self.paths.slot(style);
        let write = tokio::task::spawn_blocking(move || {
            slots::republish(&slot).map_err(|error| error.to_string())
        });
        self.answer_later(write, move |result| Event::Republished {
            style,
            result: result.flatten(),
        });
    }

    /// Sends the event that `into_event` makes of the outcome of `work`,
    /// which runs off the event loop.
    ///
    /// The core waits for exactly one answer per probe and per slot write,
    /// so work that panics answers too: `into_event` then gets the panic
    /// text.
    fn answer_later<T: Send + 'static>(
        &self,
        work: JoinHandle<T>,
        into_event: impl FnOnce(Result<T, String>) -> Event + Send + 'static,
    ) {
        let answers = self.answer_sender.clone();
        tokio::spawn(async move {
            let outcome = work.await.map_err(|failure| failure.to_string());
            // Nobody listens once the event loop has ended.
            let _ = answers.send(into_event(outcome));
        });
    }

    /// Writes the keys when the user's original keys are on disk. A write
    /// that arrives before that save is not kept: a later save must not
    /// flush it. The core hears `KeyWriteFailed` and retries the write.
    async fn set_keys_once_saved(&self, write: KeyWrite) {
        if self.originals_saved {
            self.set_keys(write).await;
        } else {
            tracing::error!("background keys not written: the user's keys are not saved yet");
            self.fail_key_write(&write, None);
        }
    }

    /// Writes the background keys as the core gave them: slot URIs in slots
    /// mode, image URIs in uri mode. A failure is logged and the daemon goes
    /// on.
    async fn set_keys(&self, write: KeyWrite) {
        let styles: Vec<Style> = Style::ALL
            .into_iter()
            .filter(|&style| write.uris[style].is_some())
            .collect();
        match self.ports.gnome.set_keys(write.clone()).await {
            Ok(()) => tracing::info!(?styles, "background keys written"),
            Err(error) => {
                tracing::error!(reason = %error, "background keys not written");
                match &error {
                    GnomeError::Write { unstored, .. } => {
                        self.fail_key_write(&write, Some(unstored))
                    }
                    GnomeError::NoAnswer => self.leave_key_write_unconfirmed(&write),
                    _ => self.fail_key_write(&write, None),
                }
            }
        }
    }

    /// Tells the core which keys of `write` were not stored. `None` means
    /// none was.
    fn fail_key_write(&self, write: &KeyWrite, unstored: Option<&[String]>) {
        let named = |key: &str| unstored.is_none_or(|names| names.iter().any(|name| name == key));
        let keeps = |key: &str, value: Option<String>| named(key).then_some(value).flatten();
        let failed = Event::KeyWriteFailed {
            uris: PerStyle {
                light: keeps(PICTURE_URI, write.uris.light.clone()),
                dark: keeps(PICTURE_URI_DARK, write.uris.dark.clone()),
            },
            picture_options: named(PICTURE_OPTIONS)
                .then_some(write.picture_options)
                .flatten(),
            unconfirmed: false,
        };
        let _ = self.answer_sender.send(failed);
    }

    /// Tells the core that settings did not answer `write`. It is not
    /// refused: it may still land, and GNOME then reports it as any other
    /// change.
    fn leave_key_write_unconfirmed(&self, write: &KeyWrite) {
        let unconfirmed = Event::KeyWriteFailed {
            uris: write.uris.clone(),
            picture_options: write.picture_options,
            unconfirmed: true,
        };
        let _ = self.answer_sender.send(unconfirmed);
    }

    /// Saves the model as it is after the step that asked for it. The save
    /// runs on the blocking pool and is awaited, so a healthy one finishes
    /// before a later key write in this step. Spawned tasks go on while it
    /// runs, but the loop reads no event until it ends, so a stalled
    /// `sync_all` delays the next event, a shutdown included. It does not
    /// flush a key write that was refused because an earlier save failed.
    async fn persist(&mut self) {
        let path = self.paths.state_file.clone();
        let saved = self.model.persisted();
        let written = tokio::task::spawn_blocking(move || state::save(&path, &saved))
            .await
            .unwrap_or_else(|failure| panic::resume_unwind(failure.into_panic()));
        match written {
            Ok(()) => self.originals_saved = true,
            Err(error) => tracing::error!(reason = %error, "state not saved"),
        }
    }
}

/// Blocking I/O: see [`Interpreter::start`].
fn lay_groundwork(
    config: Option<Config>,
    paths: &Paths,
    cache: &Cache,
    seed: u64,
) -> Result<Groundwork, StartError> {
    if let Err(error) = slots::sweep_temp_files(&paths.slots_dir) {
        tracing::warn!(reason = %error, "slots directory not made private or not swept");
    }
    let restored = restore_state(&paths.state_file)?;
    let originals_saved = restored
        .as_ref()
        .is_some_and(|saved| saved.original_keys.is_some());
    let model = Model::new(ModelInit {
        config,
        // Each scan arrives as an event, after the first apply.
        local_files: PerStyle::default(),
        restored,
        seed,
        slot_uris: PerStyle::from_fn(|style| slot_uri(paths, style)),
    });
    Ok(Groundwork {
        model,
        originals_saved,
        slots_watch: SlotsWatch::start(paths.slots_dir.clone()),
        cached: cached_blobs(cache),
    })
}

/// The remote cache's blobs. An unreadable cache is logged and starts as an
/// empty one, so local sources still rotate; each URL is then fetched again.
fn cached_blobs(cache: &Cache) -> Vec<RemoteBlob> {
    cache.load().unwrap_or_else(|error| {
        tracing::warn!(reason = %error, "remote cache not read: starting without it");
        Vec::new()
    })
}

/// The saved state of the last run. A missing file is no state. Corrupt
/// JSON and an unsupported version are logged and start as no state; the
/// next save replaces them. Any other error, including I/O on a file that
/// exists, is returned and the file is left as it is.
fn restore_state(state_file: &Path) -> Result<Option<Persisted>, state::StateError> {
    match state::load(state_file) {
        Ok(saved) => Ok(saved),
        Err(
            error @ (state::StateError::Corrupt(_) | state::StateError::UnsupportedVersion { .. }),
        ) => {
            tracing::warn!(reason = %error, "state file ignored");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Whether the core applies a config for `event`: a config file that was
/// read again and is valid.
fn applies_a_config(event: &Event) -> bool {
    matches!(
        event,
        Event::ConfigChanged(Ok(_)) | Event::ConfigRead { result: Ok(_), .. }
    )
}

/// The event for the news of `style`'s source watch, which is logged here.
fn catalog_event(style: Style, news: SourceNews) -> Event {
    match news {
        SourceNews::Scanned(scan) => {
            log_scan(style, &scan);
            Event::Scanned {
                style,
                files: scan.files,
            }
        }
        SourceNews::Changed(changes) => {
            let found = changes.iter().filter(|change| is_found(change)).count();
            let removed = changes.len() - found;
            tracing::info!(%style, found, removed, "image files changed");
            Event::Fs { style, changes }
        }
    }
}

/// Logs what the scan of `style`'s sources found and what it skipped.
fn log_scan(style: Style, scan: &Scan) {
    for warning in &scan.warnings {
        let path = warning.path.display();
        tracing::warn!(%style, %path, reason = %warning.reason, "source skipped");
    }
    tracing::info!(%style, images = scan.files.len(), "sources scanned");
}

fn is_found(change: &FsChange) -> bool {
    matches!(change, FsChange::Found(_))
}

/// The event for what loading the changed config file gave, which is logged
/// here. `None` for a file that is gone or cannot be read: the running
/// config stays, and the core has nothing to do.
fn config_event(file: &Path, loaded: Result<Option<Config>, LoadError>) -> Option<Event> {
    let path = file.display();
    match loaded {
        Ok(Some(config)) => {
            tracing::info!(%path, "config file changed: applying it");
            Some(Event::ConfigChanged(Ok(config)))
        }
        Ok(None) => {
            tracing::warn!(%path, "config file is gone: the running config stays");
            None
        }
        Err(LoadError::Invalid(error)) => {
            tracing::error!(%path, reason = %error, "config file is invalid: the running config stays");
            Some(Event::ConfigChanged(Err(error)))
        }
        Err(LoadError::Io(error)) => {
            tracing::error!(%path, reason = %error, "config file cannot be read: the running config stays");
            None
        }
    }
}

/// What `gpaperctl reload` gets from loading the config file again, which
/// is logged here. A file that is gone or cannot be read is an error with
/// no position in the file: the running config stays, as for an invalid
/// one. `loaded` is `Err` with the panic text when the load panicked.
fn reread_config(
    file: &Path,
    loaded: Result<Result<Option<Config>, LoadError>, String>,
) -> Result<Config, ConfigError> {
    let path = file.display();
    let result = match loaded {
        Ok(Ok(Some(config))) => Ok(config),
        Ok(Ok(None)) => Err(unplaced("the config file is gone".to_owned())),
        Ok(Err(LoadError::Invalid(error))) => Err(error),
        Ok(Err(LoadError::Io(error))) => {
            Err(unplaced(format!("cannot read the config file: {error}")))
        }
        Err(panic) => Err(unplaced(format!("loading the config file failed: {panic}"))),
    };
    match &result {
        Ok(_) => tracing::info!(%path, "config file reloaded: applying it"),
        Err(error) => {
            tracing::error!(%path, reason = %error, "config file not reloaded: the running config stays")
        }
    }
    result
}

/// A config error that belongs to no token: line 1, column 1.
fn unplaced(message: String) -> ConfigError {
    ConfigError {
        line: 1,
        column: 1,
        message,
    }
}

/// The inode of a slot file. A slot that cannot be read counts as missing, so
/// the first apply fills it again.
fn slot_on_disk(slot: &Path) -> Option<FileId> {
    slots::slot_id(slot).unwrap_or_else(|error| {
        tracing::warn!(path = %slot.display(), reason = %error, "slot file cannot be read");
        None
    })
}

/// The URI of a style's slot file: what the style's key holds in slots mode.
fn slot_uri(paths: &Paths, style: Style) -> String {
    // Only a relative path has no URI, and `Paths` holds absolute ones.
    slots::file_uri(&paths.slot(style)).expect("an absolute slots directory")
}

/// Logs what a probe, a slot write or the slots watch reports. The core
/// only acts on the event.
fn log_event(event: &Event) {
    match event {
        // An expected answer for a file that is still being copied in: the
        // core probes it again.
        Event::Probed {
            style,
            item,
            verdict: Err(BadReason::Truncated),
        } => {
            tracing::info!(%style, item = %fetch::loggable(item), "image is incomplete: checked again later")
        }
        Event::Probed {
            style,
            item,
            verdict: Err(reason),
        } => tracing::warn!(%style, item = %fetch::loggable(item), %reason, "image rejected"),
        Event::Published {
            style,
            item,
            result: Ok(write),
        } => {
            tracing::info!(%style, item = %fetch::loggable(item), method = %write.method, "wallpaper changed")
        }
        Event::Published {
            style,
            item,
            result: Err(reason),
        } => {
            tracing::warn!(%style, item = %fetch::loggable(item), %reason, "wallpaper not changed")
        }
        Event::Republished {
            style,
            result: Ok(write),
        } => {
            tracing::info!(%style, method = %write.method, "slot refreshed after the style switch")
        }
        Event::Republished {
            style,
            result: Err(reason),
        } => tracing::warn!(%style, %reason, "slot not refreshed"),
        Event::SlotLost(style) => tracing::warn!(%style, "slot file is gone: filling it again"),
        Event::ScreenLock(ScreenLock::Locked) => tracing::debug!("screen locked"),
        Event::ScreenLock(ScreenLock::Unlocked) => tracing::debug!("screen unlocked"),
        _ => {}
    }
}
