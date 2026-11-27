//! GNOME adapter: GSettings and `GNetworkMonitor` behind a port.
//!
//! The interpreter talks to GNOME only through [`GnomePort`], so integration
//! tests can use an in-memory fake. The real adapter, [`GnomeThread`], runs
//! GSettings and `GNetworkMonitor` on a dedicated GLib thread, because
//! `gio::Settings` is `!Send` and both signal on their thread's main
//! context. Commands go in over `async-channel`; events come out over a
//! tokio channel.
//!
//! Tests must never reach the user's dconf: run them only with
//! `GSETTINGS_BACKEND=keyfile` and temporary XDG directories.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use gio::prelude::*;
use gpaperd_core::config::PictureOptions;
use gpaperd_core::event::{BackgroundKeys, Connectivity, GnomeEvent, GnomeSnapshot};
use gpaperd_core::style::{PerStyle, Style};
use tokio::sync::{mpsc, oneshot};

const BACKGROUND_SCHEMA: &str = "org.gnome.desktop.background";
const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
pub(crate) const PICTURE_URI: &str = "picture-uri";
pub(crate) const PICTURE_URI_DARK: &str = "picture-uri-dark";
pub(crate) const PICTURE_OPTIONS: &str = "picture-options";
const COLOR_SCHEME: &str = "color-scheme";
/// Type name of GLib's in-memory backend. `GSETTINGS_BACKEND=memory` selects
/// it, and GLib falls back to it when dconf is not installed.
const MEMORY_BACKEND: &str = "GMemorySettingsBackend";

/// What the daemon asks of GNOME.
///
/// Both methods are safe to call from the tokio thread and do not block it.
pub trait GnomePort {
    /// Reads the visible style, the background keys and their locks in one
    /// go.
    ///
    /// The style is dark only when `org.gnome.desktop.interface`
    /// `color-scheme` is `prefer-dark`; a missing schema or key means light.
    /// A key counts as locked when GSettings says it is not writable.
    fn snapshot(&self) -> impl Future<Output = Result<GnomeSnapshot, GnomeError>>;

    /// Writes the background keys in one transaction: `picture-uri` and
    /// `picture-uri-dark` to the URIs that are given, and `picture-options`
    /// when given. A key that `write` does not name keeps its value.
    ///
    /// Exactly what `write` says is written; the port decides nothing. A
    /// schema without `picture-uri-dark` gets only the light key. Writing
    /// values that are already set changes nothing and raises no event.
    ///
    /// A locked key that already holds its value is fine. Any other locked
    /// key fails the call before any key is written, including a locked
    /// `picture-options` that is still `none` (the shell loads no file)
    /// even when this write does not name that key. The error names every
    /// key of the write that was not stored. A `picture-options` locked at
    /// any other value is the administrator's: it is left out of the write,
    /// and the URIs are still written.
    fn set_keys(&self, write: KeyWrite) -> impl Future<Output = Result<(), GnomeError>>;

    /// Writes `keys` back as they are, in one transaction: `picture-uri`,
    /// `picture-uri-dark` when it is `Some`, and `picture-options` as the
    /// raw enum nick. For `gpaperctl restore`, whose values come from the
    /// user and need not be ones that the daemon would write.
    ///
    /// Locked keys and refused writes fail as in [`GnomePort::set_keys`],
    /// except that a `picture-options` locked at `none` blocks only a
    /// restore that would change it.
    fn restore_keys(&self, keys: BackgroundKeys) -> impl Future<Output = Result<(), GnomeError>>;
}

/// One transaction on the background keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyWrite {
    /// The `file://` URI for each style's key: its slot file in slots mode,
    /// the image itself in uri mode. `None` leaves that style's key alone.
    pub uris: PerStyle<Option<String>>,
    /// `None` leaves `picture-options` alone.
    pub picture_options: Option<PictureOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GnomeError {
    /// Not a GNOME session. Fatal at start: the daemon exits with code 69.
    #[error("GSettings schema {schema} is not installed")]
    SchemaMissing { schema: String },
    /// Settings would not outlive the process, for example with
    /// `GSETTINGS_BACKEND=memory`. Fatal at start: exit code 69.
    #[error("GSettings backend {backend} does not persist settings")]
    VolatileBackend { backend: String },
    /// A key is locked, or the backend refused the write. `unstored` names
    /// the keys that were not stored.
    #[error("cannot write background keys: {reason}")]
    Write {
        reason: String,
        unstored: Vec<String>,
    },
    /// The settings thread did not answer in time. A write may still land
    /// later: it is unconfirmed, not refused.
    #[error("GNOME settings thread did not answer")]
    NoAnswer,
    /// The GLib thread is gone.
    #[error("GNOME settings thread stopped")]
    Stopped,
}

/// The real [`GnomePort`]: a handle to the GLib thread that owns GSettings.
///
/// Dropping the handle closes the command channel, which makes the thread
/// quit its main loop, and joins it. A thread that once did not answer is
/// not joined: it may stay blocked, and the process must still exit.
#[derive(Debug)]
pub struct GnomeThread {
    commands: async_channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    stalled: AtomicBool,
}

#[derive(Debug)]
enum Command {
    Snapshot(oneshot::Sender<GnomeSnapshot>),
    SetKeys(KeyWrite, oneshot::Sender<Result<(), GnomeError>>),
    RestoreKeys(BackgroundKeys, oneshot::Sender<Result<(), GnomeError>>),
}

impl GnomeThread {
    /// Starts the GLib thread and blocks until it has checked the session.
    ///
    /// The thread looks up `org.gnome.desktop.background` before creating any
    /// `gio::Settings` (which would abort on a missing schema) and checks
    /// that the backend persists settings. It writes nothing: the first
    /// [`GnomePort::snapshot`] still holds the user's own keys.
    ///
    /// Events sent to `events`:
    /// - [`GnomeEvent::StyleChanged`] when `color-scheme` changes the visible
    ///   style.
    /// - [`GnomeEvent::KeysChanged`] once per burst of changes to
    ///   `picture-uri`, `picture-uri-dark` or `picture-options`, with one
    ///   consistent read of all three taken after the burst. The daemon's
    ///   own writes count too.
    /// - [`GnomeEvent::LocksChanged`] when a burst of changes, or of lock
    ///   changes (`writable-changed`), leaves the locks of `picture-uri` or
    ///   `picture-uri-dark` otherwise than last reported. Each burst reads
    ///   the locks again, so a lock that came without its own signal is
    ///   seen with the next key change.
    /// - [`GnomeEvent::ConnectivityChanged`] once with `GNetworkMonitor`'s
    ///   level at start, queued before `spawn` returns, then each time the
    ///   level becomes full or stops being full. Without a usable backend
    ///   GLib's dummy monitor reports full once and never changes.
    ///
    /// A burst that ends in the state last reported raises no event. A
    /// closed `events` receiver is not an error.
    ///
    /// Call it once per process: GLib binds the default settings backend to
    /// the main context of the first thread that uses it.
    ///
    /// # Errors
    ///
    /// [`GnomeError::SchemaMissing`] or [`GnomeError::VolatileBackend`]. No
    /// thread is left running.
    pub fn spawn(events: mpsc::UnboundedSender<GnomeEvent>) -> Result<GnomeThread, GnomeError> {
        let (commands, inbox) = async_channel::unbounded();
        let (opened, open_outcome) = sync_channel(1);
        let thread = std::thread::spawn(move || run_settings_thread(&opened, &inbox, events));
        let gnome = GnomeThread {
            commands,
            thread: Some(thread),
            stalled: AtomicBool::new(false),
        };
        // No message means the thread panicked. On an error, dropping `gnome`
        // joins the thread, which is already on its way out.
        open_outcome.recv().unwrap_or(Err(GnomeError::Stopped))?;
        Ok(gnome)
    }

    /// Sends one command to the GLib thread and waits for its answer.
    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, GnomeError> {
        let (reply, answer) = oneshot::channel();
        let sent = self.commands.send(command(reply)).await;
        sent.map_err(|_| GnomeError::Stopped)?;
        let reply = wait_for_reply(answer, crate::dbus::REPLY_TIMEOUT).await;
        if matches!(reply, Err(GnomeError::NoAnswer)) {
            self.stalled.store(true, Ordering::Relaxed);
        }
        reply
    }
}

/// Waits for the GLib thread. `gio::Settings::sync` has no cancellable, so
/// the wait is what ends: after `timeout` the caller stops waiting and the
/// reply, if it arrives later, is dropped.
async fn wait_for_reply<T>(
    answer: oneshot::Receiver<T>,
    timeout: Duration,
) -> Result<T, GnomeError> {
    match tokio::time::timeout(timeout, answer).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(GnomeError::Stopped),
        Err(_elapsed) => Err(GnomeError::NoAnswer),
    }
}

impl GnomePort for GnomeThread {
    async fn snapshot(&self) -> Result<GnomeSnapshot, GnomeError> {
        self.ask(Command::Snapshot).await
    }

    async fn set_keys(&self, write: KeyWrite) -> Result<(), GnomeError> {
        self.ask(|reply| Command::SetKeys(write, reply)).await?
    }

    async fn restore_keys(&self, keys: BackgroundKeys) -> Result<(), GnomeError> {
        self.ask(|reply| Command::RestoreKeys(keys, reply)).await?
    }
}

impl Drop for GnomeThread {
    fn drop(&mut self) {
        self.commands.close();
        let Some(thread) = self.thread.take() else {
            return;
        };
        if self.stalled.load(Ordering::Relaxed) {
            // Joining could block for as long as `sync` does. The thread
            // ends with the process.
            tracing::warn!("GNOME settings thread left running: it did not answer");
        } else if thread.join().is_err() {
            tracing::error!("GNOME settings thread panicked");
        }
    }
}

/// Body of the GLib thread: opens the session, tells `spawn` how that went,
/// then serves commands until the handle closes the channel.
fn run_settings_thread(
    opened: &SyncSender<Result<(), GnomeError>>,
    commands: &async_channel::Receiver<Command>,
    events: mpsc::UnboundedSender<GnomeEvent>,
) {
    // `block_on` makes the new context this thread's default one, so
    // GSettings and its backend deliver their change notifications here.
    glib::MainContext::new().block_on(async {
        let _session_bus = session_bus_that_outlives_the_bus();
        // The connectivity reporter starts first, so its event at start is
        // queued before `spawn` returns. It lives until the thread ends.
        let reporters = Session::open().map(|session| {
            let connectivity = ConnectivityReporter::start(events.clone());
            (ChangeReporter::start(session, events), connectivity)
        });
        let outcome = match &reporters {
            Ok(_) => Ok(()),
            Err(error) => Err(error.clone()),
        };
        // `spawn` blocks on this message. If it is gone, nobody holds the
        // handle, so no command can arrive.
        if opened.send(outcome).is_err() {
            return;
        }
        if let Ok((changes, _connectivity)) = reporters {
            serve(&changes.session, commands).await;
        }
    });
}

/// GLib's shared session-bus connection, told not to end the process when
/// the bus goes away. GVfs opens that connection on the first file access,
/// and by default GLib raises `SIGTERM` once it closes: the daemon would
/// stop with exit code 0, which `Restart=on-failure` does not restart. The
/// daemon runs on instead, without `gpaperctl` control, as it does when no
/// bus is reachable at start (see `control_interface` in `main.rs`).
///
/// The connection is a singleton that lives only while someone holds it,
/// so the caller keeps it for the whole thread. `None` without a bus.
///
/// `bus_get_sync` blocks this thread. A cancellable on another thread
/// bounds that wait by [`crate::dbus::REPLY_TIMEOUT`]. A failed open is
/// tried once more, the same way. The connection this thread keeps,
/// including one opened on the second try, is told not to exit on close.
fn session_bus_that_outlives_the_bus() -> Option<gio::DBusConnection> {
    let bus = open_session_bus().or_else(open_session_bus)?;
    bus.set_exit_on_close(false);
    Some(bus)
}

/// One bounded `bus_get_sync`. `None` when the bus does not answer in time
/// or the open fails.
fn open_session_bus() -> Option<gio::DBusConnection> {
    let cancellable = gio::Cancellable::new();
    let (finished, wait) = sync_channel(1);
    let cancel = cancellable.clone();
    std::thread::spawn(move || {
        if wait.recv_timeout(crate::dbus::REPLY_TIMEOUT).is_err() {
            cancel.cancel();
        }
    });
    let bus = gio::bus_get_sync(gio::BusType::Session, Some(&cancellable)).ok();
    let _ = finished.send(());
    bus
}

/// Answers commands until the handle closes the channel.
async fn serve(session: &Session, commands: &async_channel::Receiver<Command>) {
    while let Ok(command) = commands.recv().await {
        // A reply that cannot be sent means the caller stopped waiting.
        match command {
            Command::Snapshot(reply) => {
                let _ = reply.send(session.snapshot());
            }
            Command::SetKeys(write, reply) => {
                let _ = reply.send(session.write(&write));
            }
            Command::RestoreKeys(keys, reply) => {
                let _ = reply.send(session.restore(&keys));
            }
        }
    }
}

/// A background key and the value it must hold.
type KeyChange<'a> = (&'static str, &'a str);

/// The GSettings objects. They are `!Send` and never leave the GLib thread.
struct Session {
    background: gio::Settings,
    /// GNOME < 42 has no `picture-uri-dark`: only the light key exists.
    has_dark_key: bool,
    /// `None` without a `color-scheme` key (GNOME < 42): always light.
    interface: Option<gio::Settings>,
}

impl Session {
    /// Checks the schema and the backend, then creates the settings objects.
    ///
    /// The checks come first because `gio::Settings` aborts the process when
    /// its schema, or a key it is asked for, does not exist.
    fn open() -> Result<Session, GnomeError> {
        let source = gio::SettingsSchemaSource::default();
        let lookup = |schema_id| source.as_ref().and_then(|s| s.lookup(schema_id, true));
        let background = checked_background_schema(lookup(BACKGROUND_SCHEMA))?;
        let backend = persistent_backend()?;
        let color_scheme = lookup(INTERFACE_SCHEMA).filter(|schema| schema.has_key(COLOR_SCHEME));
        let settings = |schema| gio::Settings::new_full(schema, Some(&backend), None);

        let session = Session {
            background: settings(&background),
            has_dark_key: background.has_key(PICTURE_URI_DARK),
            interface: color_scheme.as_ref().map(settings),
        };
        // From here on, every write to the background keys is a transaction.
        session.background.delay();
        Ok(session)
    }

    /// Calls `on_change` when a key that [`Session::snapshot`] reads changes,
    /// or a background key is locked or unlocked.
    fn connect_changed(&self, on_change: impl Fn() + Clone + 'static) {
        for key in [PICTURE_URI, PICTURE_URI_DARK, PICTURE_OPTIONS] {
            let on_change = on_change.clone();
            self.background
                .connect_changed(Some(key), move |_, _| on_change());
        }
        // Undetailed: a backend may report a lock on a whole path.
        let on_lock = on_change.clone();
        self.background
            .connect_writable_changed(None, move |_, _| on_lock());
        if let Some(interface) = &self.interface {
            interface.connect_changed(Some(COLOR_SCHEME), move |_, _| on_change());
        }
    }

    fn snapshot(&self) -> GnomeSnapshot {
        GnomeSnapshot {
            visible: self.visible_style(),
            keys: self.keys(),
            locked: PerStyle {
                light: self.is_locked(PICTURE_URI),
                dark: self.has_dark_key && self.is_locked(PICTURE_URI_DARK),
            },
        }
    }

    fn is_locked(&self, key: &str) -> bool {
        !self.background.is_writable(key)
    }

    fn visible_style(&self) -> Style {
        match &self.interface {
            Some(interface) => Style::from_color_scheme(&interface.string(COLOR_SCHEME)),
            None => Style::Light,
        }
    }

    fn keys(&self) -> BackgroundKeys {
        let read = |key| String::from(self.background.string(key));
        BackgroundKeys {
            picture_uri: read(PICTURE_URI),
            picture_uri_dark: self.has_dark_key.then(|| read(PICTURE_URI_DARK)),
            picture_options: read(PICTURE_OPTIONS),
        }
    }

    /// Writes, in one transaction, every key that does not hold its wanted
    /// value yet. A locked one, or `picture-options` locked while it is
    /// still `none`, fails the call before that transaction.
    fn write(&self, write: &KeyWrite) -> Result<(), GnomeError> {
        let current_options = self.background.string(PICTURE_OPTIONS);
        let blocking: &[&str] =
            if options_block_the_write(self.is_locked(PICTURE_OPTIONS), &current_options) {
                &[PICTURE_OPTIONS]
            } else {
                &[]
            };
        self.write_changes(self.wanted(write), blocking)
    }

    fn restore(&self, keys: &BackgroundKeys) -> Result<(), GnomeError> {
        let mut wanted = vec![(PICTURE_URI, keys.picture_uri.as_str())];
        if let Some(dark_uri) = &keys.picture_uri_dark
            && self.has_dark_key
        {
            wanted.push((PICTURE_URI_DARK, dark_uri.as_str()));
        }
        wanted.push((PICTURE_OPTIONS, keys.picture_options.as_str()));
        self.write_changes(wanted, &[])
    }

    /// Commits the changes that are not set yet. A locked key, or a key in
    /// `blocking`, fails the call before `commit`. `picture-options` locked
    /// at any value but `none` is the administrator's: it is left out, and
    /// the URIs are still written.
    fn write_changes(
        &self,
        mut wanted: Vec<KeyChange<'_>>,
        blocking: &[&'static str],
    ) -> Result<(), GnomeError> {
        let current_options = self.background.string(PICTURE_OPTIONS);
        if options_stay_as_locked(self.is_locked(PICTURE_OPTIONS), &current_options) {
            wanted.retain(|(key, _)| *key != PICTURE_OPTIONS);
        }
        let outstanding: Vec<_> = wanted
            .iter()
            .copied()
            .filter(|change| self.is_outstanding(change))
            .collect();
        if outstanding.is_empty() {
            return Ok(());
        }
        let mut locked: Vec<&str> = outstanding
            .iter()
            .map(|(key, _)| *key)
            .filter(|key| self.is_locked(key))
            .collect();
        for key in blocking {
            if !locked.contains(key) {
                locked.push(key);
            }
        }
        if !locked.is_empty() {
            return Err(refuse_locked(&wanted, &locked));
        }
        self.commit(&outstanding)
    }

    /// The value each key must hold after `write`.
    fn wanted<'a>(&self, write: &'a KeyWrite) -> Vec<KeyChange<'a>> {
        let mut wanted = Vec::new();
        if let Some(light_uri) = &write.uris.light {
            wanted.push((PICTURE_URI, light_uri.as_str()));
        }
        if let Some(dark_uri) = &write.uris.dark
            && self.has_dark_key
        {
            wanted.push((PICTURE_URI_DARK, dark_uri.as_str()));
        }
        if let Some(options) = write.picture_options {
            wanted.push((PICTURE_OPTIONS, options.as_str()));
        }
        wanted
    }

    fn is_outstanding(&self, (key, value): &KeyChange<'_>) -> bool {
        self.background.string(key) != *value
    }

    /// Hands `changes` to the backend as one transaction and waits until the
    /// backend has stored them.
    fn commit(&self, changes: &[KeyChange<'_>]) -> Result<(), GnomeError> {
        for (key, value) in changes {
            if let Err(error) = self.background.set_string(key, value) {
                self.background.revert();
                return Err(write_error(format!("{key}: {error}"), changes));
            }
        }
        self.background.apply();
        gio::Settings::sync();

        // `apply` returns nothing. A write the backend refused shows up as
        // the old value once `sync` is done. Only the refused keys are
        // unstored: the ones that stayed are the user's.
        let refused: Vec<_> = changes
            .iter()
            .copied()
            .filter(|change| self.is_outstanding(change))
            .collect();
        if refused.is_empty() {
            Ok(())
        } else {
            let reason = format!("refused by the backend: {}", key_names(&refused));
            Err(write_error(reason, &refused))
        }
    }
}

/// Nothing was stored. Every key of the wanted write is unstored, not only
/// the locked names, so the caller can clear each one it recorded.
fn refuse_locked(wanted: &[KeyChange<'_>], locked: &[&str]) -> GnomeError {
    write_error(format!("locked keys: {}", locked.join(", ")), wanted)
}

fn write_error(reason: String, changes: &[KeyChange<'_>]) -> GnomeError {
    GnomeError::Write {
        reason,
        unstored: changes.iter().map(|(key, _)| (*key).to_owned()).collect(),
    }
}

/// A locked `picture-options` that is still `none` means the shell loads no
/// file. No key of a [`GnomePort::set_keys`] transaction may be written,
/// including a URI write that does not name `picture-options`. Locked at
/// any other value does not block a write that leaves the key alone.
fn options_block_the_write(options_locked: bool, current_options: &str) -> bool {
    options_locked && current_options == "none"
}

/// A `picture-options` locked at a value the shell can show stays at that
/// value. Writing it anyway would fail every write that carries it.
fn options_stay_as_locked(options_locked: bool, current_options: &str) -> bool {
    options_locked && current_options != "none"
}

fn key_names(changes: &[KeyChange<'_>]) -> String {
    let names: Vec<_> = changes.iter().map(|(key, _)| *key).collect();
    names.join(", ")
}

/// The background schema, if it has the keys that every GNOME release has.
fn checked_background_schema(
    found: Option<gio::SettingsSchema>,
) -> Result<gio::SettingsSchema, GnomeError> {
    let missing = |schema| GnomeError::SchemaMissing { schema };
    let schema = found.ok_or_else(|| missing(BACKGROUND_SCHEMA.to_owned()))?;
    let required = [PICTURE_URI, PICTURE_OPTIONS];
    match required.into_iter().find(|key| !schema.has_key(key)) {
        Some(key) => Err(missing(format!("{BACKGROUND_SCHEMA} with key {key}"))),
        None => Ok(schema),
    }
}

/// The default backend, unless it forgets every write when the process ends.
fn persistent_backend() -> Result<gio::SettingsBackend, GnomeError> {
    let backend = gio::SettingsBackend::default();
    let type_name = backend.type_().name();
    if type_name == MEMORY_BACKEND {
        return Err(GnomeError::VolatileBackend {
            backend: type_name.to_owned(),
        });
    }
    Ok(backend)
}

/// Reports GLib's connectivity from its default `GNetworkMonitor`: the level
/// at start once, then each time it becomes full or stops being full. Runs
/// on the GLib thread next to the [`ChangeReporter`] and sends on the same
/// channel.
///
/// Changes are coalesced like key changes: the first `notify::connectivity`
/// of a burst queues one task, which reads the level and reports it only if
/// it differs from the last report.
struct ConnectivityReporter {
    monitor: gio::NetworkMonitor,
    /// The monitor is a process-wide singleton that outlives the reporter,
    /// so dropping the reporter disconnects this handler.
    handler: Option<glib::SignalHandlerId>,
    events: mpsc::UnboundedSender<GnomeEvent>,
    /// The level the daemon was last told about.
    last_reported: Cell<Connectivity>,
    report_queued: Cell<bool>,
}

impl ConnectivityReporter {
    /// Follows `gio::NetworkMonitor::default()` and sends its level now.
    ///
    /// Call it on the GLib thread: the first call binds the monitor's
    /// signals to the calling thread's default main context. GLib never
    /// fails here: without a usable backend it returns a dummy monitor that
    /// reports full connectivity, so fetches still run.
    fn start(events: mpsc::UnboundedSender<GnomeEvent>) -> Rc<ConnectivityReporter> {
        let monitor = gio::NetworkMonitor::default();
        tracing::debug!(backend = monitor.type_().name(), "network monitor");
        let reporter = Rc::new_cyclic(|weak: &Weak<ConnectivityReporter>| {
            let weak = weak.clone();
            let handler = monitor
                .connect_connectivity_notify(move |_| ConnectivityReporter::queue_report(&weak));
            ConnectivityReporter {
                last_reported: Cell::new(connectivity(monitor.connectivity())),
                monitor,
                handler: Some(handler),
                events,
                report_queued: Cell::new(false),
            }
        });
        reporter.send(reporter.last_reported.get());
        reporter
    }

    fn queue_report(reporter: &Weak<ConnectivityReporter>) {
        let Some(this) = reporter.upgrade() else {
            return;
        };
        if this.report_queued.replace(true) {
            return;
        }
        // The task holds a weak reference: the monitor keeps this thread's
        // context alive after the thread ends, and a task that owned the
        // reporter would keep the events channel open with it.
        let reporter = reporter.clone();
        glib::MainContext::ref_thread_default().spawn_local(async move {
            if let Some(this) = reporter.upgrade() {
                this.report();
            }
        });
    }

    fn report(&self) {
        self.report_queued.set(false);
        let current = connectivity(self.monitor.connectivity());
        if self.last_reported.replace(current) != current {
            self.send(current);
        }
    }

    fn send(&self, level: Connectivity) {
        // A closed receiver means the daemon is shutting down.
        let _ = self.events.send(GnomeEvent::ConnectivityChanged(level));
    }
}

impl Drop for ConnectivityReporter {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.monitor.disconnect(handler);
        }
    }
}

/// [`Connectivity::Full`] for GLib's full level, [`Connectivity::Limited`]
/// for every other one.
fn connectivity(level: gio::NetworkConnectivity) -> Connectivity {
    match level {
        gio::NetworkConnectivity::Full => Connectivity::Full,
        _ => Connectivity::Limited,
    }
}

/// Turns bursts of `changed` signals into events.
///
/// One transaction changes several keys, and a per-key handler can see it
/// half applied. So the first signal of a burst only queues a task. The task
/// runs after the burst, reads everything once and reports what differs from
/// the last report.
struct ChangeReporter {
    session: Session,
    events: mpsc::UnboundedSender<GnomeEvent>,
    /// The state the daemon was last told about.
    last_reported: RefCell<GnomeSnapshot>,
    report_queued: Cell<bool>,
}

impl ChangeReporter {
    fn start(session: Session, events: mpsc::UnboundedSender<GnomeEvent>) -> Rc<ChangeReporter> {
        Rc::new_cyclic(|weak: &Weak<ChangeReporter>| {
            // Weak, because the settings objects own the handlers and the
            // reporter owns the settings objects.
            let weak = weak.clone();
            session.connect_changed(move || {
                if let Some(reporter) = weak.upgrade() {
                    reporter.queue_report();
                }
            });
            // Read only now: GSettings signals a change to a key only if the
            // key was read while a handler was connected.
            let last_reported = RefCell::new(session.snapshot());
            ChangeReporter {
                session,
                events,
                last_reported,
                report_queued: Cell::new(false),
            }
        })
    }

    fn queue_report(self: Rc<Self>) {
        if self.report_queued.replace(true) {
            return;
        }
        // Not `glib::idle_add_local_once`: that targets the global default
        // context, which this thread does not run.
        glib::MainContext::ref_thread_default().spawn_local(async move { self.report() });
    }

    fn report(&self) {
        self.report_queued.set(false);
        let current = self.session.snapshot();
        let previous = self.last_reported.replace(current.clone());
        if current.visible != previous.visible {
            self.send(GnomeEvent::StyleChanged(current.visible));
        }
        if current.keys != previous.keys {
            self.send(GnomeEvent::KeysChanged(current.keys));
        }
        if current.locked != previous.locked {
            self.send(GnomeEvent::LocksChanged(current.locked));
        }
    }

    fn send(&self, event: GnomeEvent) {
        // A closed receiver means the daemon is shutting down.
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn only_glibs_full_level_counts_as_full() {
        use gio::NetworkConnectivity as Level;
        let levels = [Level::Local, Level::Limited, Level::Portal, Level::Full];

        let mapped = levels.map(connectivity);

        use Connectivity::{Full, Limited};
        assert_eq!(mapped, [Limited, Limited, Limited, Full]);
    }

    #[test]
    fn picture_options_locked_at_none_blocks_the_uri_write() {
        assert!(options_block_the_write(true, "none"));
        assert!(!options_block_the_write(true, "zoom"));
        assert!(!options_block_the_write(false, "none"));
    }

    #[test]
    fn picture_options_locked_at_another_value_stay_as_locked() {
        assert!(options_stay_as_locked(true, "scaled"));
        assert!(!options_stay_as_locked(true, "none"));
        assert!(!options_stay_as_locked(false, "scaled"));
    }

    #[tokio::test]
    async fn a_ready_settings_reply_returns_at_once() {
        let (sender, receiver) = oneshot::channel();
        sender.send(7).expect("the receiver is alive");

        let value = wait_for_reply(receiver, crate::dbus::REPLY_TIMEOUT)
            .await
            .expect("a ready reply");

        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn a_settings_reply_that_never_comes_ends_at_the_timeout_as_no_answer() {
        let (_sender, receiver) = oneshot::channel::<()>();
        let timeout = Duration::from_millis(20);
        let started = std::time::Instant::now();

        let outcome = wait_for_reply(receiver, timeout).await;

        assert!(started.elapsed() >= timeout);
        assert_eq!(outcome, Err(GnomeError::NoAnswer));
    }

    #[test]
    fn a_stalled_settings_thread_is_not_joined() {
        let (_release, blocked) = std::sync::mpsc::channel::<()>();
        let (dropped, drop_done) = std::sync::mpsc::channel();
        let (commands, _inbox) = async_channel::unbounded();
        let gnome = GnomeThread {
            commands,
            thread: Some(std::thread::spawn(move || {
                let _ = blocked.recv();
            })),
            stalled: AtomicBool::new(true),
        };

        std::thread::spawn(move || {
            drop(gnome);
            let _ = dropped.send(());
        });

        let outcome = drop_done.recv_timeout(Duration::from_secs(5));
        assert!(outcome.is_ok(), "dropping the handle waited on the thread");
    }
}
