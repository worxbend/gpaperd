//! Everything outside the daemon, for the end-to-end scenarios: real files
//! in a temp directory, GNOME's settings held in memory, and a wall clock
//! that the scenario moves by hand. All of it outlives a restart of the
//! daemon.
//!
//! The source watches, the config watch and the image check run in real
//! time. A scenario therefore never sleeps for a fixed time: it waits for
//! something that the daemon does, with [`PATIENCE`] as the limit.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::catalog::ItemId;
use gpaperd_core::config::{Config, DEFAULT_INTERVAL, ParseContext};
use gpaperd_core::event::{
    BackgroundKeys, Connectivity, Fired, GnomeEvent, GnomeSnapshot, ScreenLock, Status,
};
use gpaperd_core::model::Persisted;
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_daemon::cache::{Cache, Meta};
use gpaperd_daemon::config_watch::{ConfigFile, ConfigWatch};
use gpaperd_daemon::dbus::{self, ControlPort};
use gpaperd_daemon::fetch::{FetchSettings, ProxyPolicy};
use gpaperd_daemon::gnome::{GnomeError, GnomePort, KeyWrite};
use gpaperd_daemon::interp::{
    CheckFuture, Clock, ImageCheck, Interpreter, Notice, Ports, RunError,
};
use gpaperd_daemon::paths::Paths;
use gpaperd_daemon::probe::{self, Prober};
use gpaperd_daemon::{screensaver, slots, state};
use tempfile::TempDir;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::timeout;
use tracing::subscriber::DefaultGuard;
use url::Url;

/// Longer than any delay that a loaded test machine adds to a watch or to
/// an image check.
pub const PATIENCE: Duration = Duration::from_secs(10);
pub const MINUTE: Duration = Duration::from_secs(60);
pub const LIGHT_INTERVAL: Duration = DEFAULT_INTERVAL;
pub const DARK_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const SEED: u64 = 1;
/// A `[general]` table that makes the order of the images predictable.
pub const ALPHABETICAL: &str = r#"order = "alphabetical""#;

/// How often [`eventually`] looks again.
const LOOK_PERIOD: Duration = Duration::from_millis(5);

// A daemon that never settles must fail its scenario, not fill the memory.
// No scenario comes near these counts.
/// Notices that one daemon may send.
const MAX_NOTICES: usize = 1_000;
/// Key writes that the daemon may ask of GNOME in one scenario.
const MAX_KEY_WRITES: usize = 100;
/// Times that the clock's alarm may go off in one scenario.
const MAX_ALARMS: usize = 1_000;
/// Bytes of log that one scenario keeps.
const MAX_LOG_BYTES: usize = 1 << 20;

/// A 16 x 12 PNG that glycin decodes. A PNG ends with its `IEND` chunk, so
/// bytes after that change nothing in the picture.
const TINY_PNG: &[u8] = include_bytes!("../fixtures/tiny.png");
/// [`TINY_PNG`] cut off inside its pixel data.
const TRUNCATED_PNG: &[u8] = include_bytes!("../fixtures/truncated.png");
/// A PNG header that claims 30000 x 30000 pixels.
const HUGE_PNG: &[u8] = include_bytes!("../fixtures/huge.png");

/// Runs a scenario on a runtime that can host the interpreter, whose future
/// is not `Send`. The image check needs the I/O driver and the timers.
pub fn scenario(test: impl Future<Output = ()>) {
    run_scenario(tokio::runtime::Builder::new_current_thread(), test);
}

/// [`scenario`] on a runtime whose blocking pool has one thread, so that a
/// scenario that holds that thread stalls all blocking work behind it.
pub fn scenario_with_one_blocking_thread(test: impl Future<Output = ()>) {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.max_blocking_threads(1);
    run_scenario(builder, test);
}

fn run_scenario(mut builder: tokio::runtime::Builder, test: impl Future<Output = ()>) {
    let runtime = builder
        .enable_io()
        .enable_time()
        .build()
        .expect("build a tokio runtime");
    LocalSet::new().block_on(&runtime, test);
}

/// When every scenario starts.
pub fn start_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

/// The background keys before the daemon writes any.
pub fn users_keys() -> BackgroundKeys {
    BackgroundKeys {
        picture_uri: "file:///user/light.png".to_owned(),
        picture_uri_dark: Some("file:///user/dark.png".to_owned()),
        picture_options: "zoom".to_owned(),
    }
}

/// Whether a rotation is scheduled. A style with one image has none.
pub fn has_a_deadline(status: &Status) -> bool {
    status.next_change.is_some()
}

/// Waits until `holds` says yes. It is asked again every few milliseconds,
/// and the daemon runs in between.
pub async fn eventually(what: &str, holds: impl Fn() -> bool) {
    let waiting = async {
        while !holds() {
            tokio::time::sleep(LOOK_PERIOD).await;
        }
    };
    let waited = timeout(PATIENCE, waiting).await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

/// Lets the daemon handle what the scenario just sent to it: a GNOME event
/// or a tick of the clock. Its task runs until it waits again each time this
/// one yields. Work that the daemon hands to another thread is not waited
/// for.
pub async fn let_the_daemon_run() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// The inode number of a file.
pub fn inode(path: &Path) -> u64 {
    fs::metadata(path).expect("stat the file").ino()
}

/// The daemon's ways onto the session bus.
struct BusPorts {
    control: ControlPort,
    screen_lock: UnboundedReceiver<ScreenLock>,
}

impl BusPorts {
    /// No bus: no control interface, and no screen lock is heard.
    fn detached() -> BusPorts {
        BusPorts {
            control: ControlPort::detached(),
            screen_lock: screensaver::detached(),
        }
    }
}

/// Everything outside the daemon: the files, the time, GNOME's settings and
/// what the daemon logs.
pub struct World {
    /// Deletes the directory when the scenario ends.
    _temp_dir: TempDir,
    /// The temp directory by its real path: item ids are canonical paths.
    root: PathBuf,
    pub clock: FakeClock,
    pub gnome: FakeGnome,
    log: LogBook,
    /// Routes this thread's log lines to `log` for as long as the world is.
    _log_route: DefaultGuard,
    /// Makes the image check of each daemon that starts in this world.
    image_check: fn() -> Arc<dyn ImageCheck>,
}

impl World {
    /// A world whose daemon checks images by their magic bytes only, with
    /// [`MagicBytesOnly`]: for every scenario that is not about decoding.
    pub fn new() -> World {
        World::with_image_check(|| Arc::new(MagicBytesOnly), ".tmp")
    }

    /// A world like [`World::new`] whose files are all in a folder whose
    /// name starts with `prefix`: for a path that has more than one URI.
    pub fn in_a_folder_named(prefix: &str) -> World {
        World::with_image_check(|| Arc::new(MagicBytesOnly), prefix)
    }

    /// A world whose daemon decodes each image with glycin, as `main` does.
    /// For a scenario that needs a decode; it asks [`glycin_decodes`] first.
    pub fn decoding_with_glycin() -> World {
        World::with_image_check(|| Arc::new(Prober::new()), ".tmp")
    }

    fn with_image_check(image_check: fn() -> Arc<dyn ImageCheck>, prefix: &str) -> World {
        let temp_dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("create a temp dir");
        let root = fs::canonicalize(temp_dir.path()).expect("the real path of the temp dir");
        let log = LogBook::default();
        let world = World {
            _temp_dir: temp_dir,
            root,
            clock: FakeClock::at(start_time()),
            gnome: FakeGnome::showing(Style::Light),
            _log_route: log.collect_this_threads_log(),
            log,
            image_check,
        };
        for style in Style::ALL {
            fs::create_dir_all(world.images_dir(style)).expect("create an image folder");
        }
        world
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder that `style` reads its images from.
    pub fn images_dir(&self, style: Style) -> PathBuf {
        self.root.join("images").join(style.as_str())
    }

    /// Where the image `name` of `style` is, or would be.
    pub fn image(&self, style: Style, name: &str) -> PathBuf {
        self.images_dir(style).join(name)
    }

    /// Adds images that pass the check. Each one is marked with its own
    /// name, so the bytes of a slot tell which image it shows.
    pub fn add_images(&self, style: Style, names: &[&str]) {
        for name in names {
            write_image(&self.image(style, name), name);
        }
    }

    pub fn add_file_that_is_no_image(&self, style: Style, name: &str) {
        fs::write(self.image(style, name), "plain text").expect("write a file");
    }

    /// Adds a file whose header claims far more pixels than `max_pixels`
    /// allows.
    pub fn add_huge_image(&self, style: Style, name: &str) {
        fs::write(self.image(style, name), HUGE_PNG).expect("write an image");
    }

    /// Adds an image as it is while someone still copies it in: cut off, and
    /// modified just now.
    pub fn add_unfinished_image(&self, style: Style, name: &str) {
        fs::write(self.image(style, name), TRUNCATED_PNG).expect("write an image");
    }

    pub fn paths(&self) -> Paths {
        Paths {
            config_file: self.root.join("config/gpaperd/config.toml"),
            slots_dir: self.root.join("data/gpaperd/slots"),
            settings_tile: self
                .root
                .join("data/gnome-background-properties/gpaperd.xml"),
            state_file: self.root.join("state/gpaperd/state.json"),
            remote_cache_dir: self.root.join("cache/gpaperd/remote"),
            home_dir: self.root.clone(),
        }
    }

    /// The text of a config whose styles read the image folders. `general`
    /// is the body of its `[general]` table.
    pub fn config_text(&self, general: &str) -> String {
        self.config_text_reading(general, &self.images_dir(Style::Light))
    }

    /// As [`World::config_text`], with another folder for the light style.
    pub fn config_text_reading(&self, general: &str, light_dir: &Path) -> String {
        format!(
            "[general]\n{general}\n\
             [light]\nsources = [{light_dir:?}]\n\
             [dark]\ninterval = \"30m\"\nsources = [{dark_dir:?}]\n",
            dark_dir = self.images_dir(Style::Dark),
        )
    }

    /// The text of a config whose light style reads `light_sources`, each a
    /// path or a URL, and whose dark style reads its image folder.
    pub fn config_text_with_light_sources(&self, general: &str, light_sources: &[&str]) -> String {
        format!(
            "[general]\n{general}\n\
             [light]\nsources = {light_sources:?}\n\
             [dark]\ninterval = \"30m\"\nsources = [{dark_dir:?}]\n",
            dark_dir = self.images_dir(Style::Dark),
        )
    }

    /// A config whose styles read the image folders.
    pub fn config(&self, general: &str) -> Config {
        self.parse(&self.config_text(general))
    }

    pub fn parse(&self, config_text: &str) -> Config {
        let context = ParseContext {
            config_dir: &self.root,
            home_dir: &self.root,
            env: &|_| None,
        };
        Config::parse(config_text, &context).expect("a valid test config")
    }

    /// Saves the config file as an editor does: a new file, renamed onto
    /// the name.
    pub fn save_config_file(&self, text: &str) {
        let file = self.paths().config_file;
        let draft = file.with_extension("draft");
        fs::create_dir_all(file.parent().expect("a config directory"))
            .expect("create the config directory");
        fs::write(&draft, text).expect("write the config file");
        fs::rename(&draft, &file).expect("put the config file in place");
    }

    pub fn config_file(&self) -> ConfigFile {
        ConfigFile {
            path: self.paths().config_file,
            home_dir: self.root.clone(),
            env: no_environment,
        }
    }

    /// Starts the daemon on `config`. No scenario that uses this writes a
    /// config file, so the config watch stays silent.
    pub fn start_daemon(&self, config: Config, seed: u64) -> Daemon {
        let config_watch = ConfigWatch::start(self.config_file());
        self.launch(Some(config), config_watch, seed, BusPorts::detached())
    }

    /// Starts the daemon on `config` with `screen_lock` instead of a bus
    /// watch. The control interface is detached.
    pub fn start_daemon_with_screen_lock(
        &self,
        config: Config,
        seed: u64,
        screen_lock: UnboundedReceiver<ScreenLock>,
    ) -> Daemon {
        let config_watch = ConfigWatch::start(self.config_file());
        self.launch(
            Some(config),
            config_watch,
            seed,
            BusPorts {
                control: ControlPort::detached(),
                screen_lock,
            },
        )
    }

    /// Starts the daemon the way `main` does: on the config file, or idle
    /// when there is none.
    pub fn start_daemon_from_config_file(&self, seed: u64) -> Daemon {
        let file = self.config_file();
        let config_watch = ConfigWatch::start(file.clone());
        let config = file.load().expect("a config file that can be used");
        self.launch(config, config_watch, seed, BusPorts::detached())
    }

    /// Starts the daemon the way `main` does, with its control interface
    /// served on the private bus at `bus_address`. The daemon owns the bus
    /// name until it is dropped.
    pub async fn start_daemon_on_bus(&self, bus_address: &str, seed: u64) -> Daemon {
        let builder = zbus::connection::Builder::address(bus_address);
        let (connection, control) = dbus::serve(builder)
            .await
            .expect("serve the control interface");
        let file = self.config_file();
        let config_watch = ConfigWatch::start(file.clone());
        let config = file.load().expect("a config file that can be used");
        let bus = BusPorts {
            control,
            screen_lock: screensaver::watch(connection.clone()),
        };
        let mut daemon = self.launch(config, config_watch, seed, bus);
        daemon._bus = Some(connection);
        daemon
    }

    fn launch(
        &self,
        config: Option<Config>,
        config_watch: ConfigWatch,
        seed: u64,
        bus: BusPorts,
    ) -> Daemon {
        let gnome_inbox = self.gnome.connect();
        let (ports, notices) = self.ports_with(bus);
        let (stop, stopped) = oneshot::channel::<()>();
        let paths = self.paths();
        let run = async move {
            let shutdown = async {
                // A dropped sender stops the daemon too.
                let _ = stopped.await;
            };
            let interpreter = Interpreter::start(config, paths, seed, ports)
                .await
                .expect("the interpreter to start");
            interpreter.run(gnome_inbox, config_watch, shutdown).await
        };
        Daemon {
            notices,
            stop: Some(stop),
            run: tokio::task::spawn_local(run),
            _bus: None,
        }
    }

    /// The daemon's ports into this world, without a control interface,
    /// and the notices it sends.
    pub fn ports(&self) -> (Ports<FakeGnome, FakeClock>, UnboundedReceiver<Notice>) {
        self.ports_with(BusPorts::detached())
    }

    fn ports_with(
        &self,
        bus: BusPorts,
    ) -> (Ports<FakeGnome, FakeClock>, UnboundedReceiver<Notice>) {
        let (notice_sender, notices) = mpsc::unbounded_channel();
        let mut sent = 0;
        let ports = Ports {
            gnome: self.gnome.clone(),
            clock: self.clock.clone(),
            notify: Box::new(move |notice| {
                sent += 1;
                assert!(sent <= MAX_NOTICES, "the daemon sends notices without end");
                // Nobody listens once the scenario is over.
                let _ = notice_sender.send(notice);
            }),
            // No request leaves the machine, whatever proxy the
            // environment names.
            fetch: FetchSettings {
                proxy: ProxyPolicy::Direct,
                ..FetchSettings::default()
            },
            control: bus.control,
            screen_lock: bus.screen_lock,
            image_check: (self.image_check)(),
        };
        (ports, notices)
    }

    /// Waits until the daemon reports these images in the slots, by their
    /// names in the styles' own folders. `None`: no image.
    pub async fn showing<'a>(
        &self,
        daemon: &mut Daemon,
        light: impl Into<Option<&'a str>>,
        dark: impl Into<Option<&'a str>>,
    ) -> Status {
        let item = |style, name: Option<&str>| name.map(|name| self.item(style, name));
        let expected = PerStyle {
            light: item(Style::Light, light.into()),
            dark: item(Style::Dark, dark.into()),
        };
        let what = format!("the slots to show {expected:?}");
        daemon
            .status_where(&what, |status| status.current == expected)
            .await
    }

    /// The id that the daemon knows the image `name` of `style` by.
    pub fn item(&self, style: Style, name: &str) -> ItemId {
        ItemId::from_path(&self.image(style, name))
    }

    /// The remote cache as the daemon sees it.
    pub fn cache(&self) -> Cache {
        Cache::new(self.paths().remote_cache_dir)
    }

    /// Puts an image marked `mark` in the remote cache as `url`'s version
    /// with `etag`, as an earlier run would have. Returns the blob's path.
    pub async fn cache_remote_image(&self, url: &Url, mark: &str, etag: &str) -> PathBuf {
        let cache = self.cache();
        let mut draft = cache.draft(url).await.expect("start a blob");
        draft
            .append(&image_bytes(mark))
            .await
            .expect("write the blob");
        let meta = Meta {
            etag: Some(etag.to_owned()),
            mime: Some("image/png".to_owned()),
            ..Meta::new(url.clone())
        };
        cache
            .publish(url, draft, "png", meta)
            .await
            .expect("publish the blob")
    }

    /// `url`'s `meta.toml`, if there is one.
    pub async fn cached_meta(&self, url: &Url) -> Option<Meta> {
        self.cache().meta(url).await.expect("a readable meta.toml")
    }

    /// Waits until `url`'s `meta.toml` says what `holds` wants.
    pub async fn cached_meta_where(&self, url: &Url, what: &str, holds: impl Fn(&Meta) -> bool) {
        let waiting = async {
            while !self.cached_meta(url).await.as_ref().is_some_and(&holds) {
                tokio::time::sleep(LOOK_PERIOD).await;
            }
        };
        let waited = timeout(PATIENCE, waiting).await;
        assert!(waited.is_ok(), "timed out waiting for {what}");
    }

    /// The mark of the image whose content is in `style`'s slot: its name,
    /// unless the scenario wrote the file with another mark.
    pub fn shown(&self, style: Style) -> String {
        let content = fs::read(self.paths().slot(style)).expect("read the slot file");
        let mark = content
            .strip_prefix(TINY_PNG)
            .expect("an image in the slot");
        String::from_utf8(mark.to_vec()).expect("an image written by write_image")
    }

    /// Lets light's interval pass and returns the image light shows then.
    pub async fn shown_after_next_rotation(&self, daemon: &mut Daemon) -> String {
        self.clock.advance(LIGHT_INTERVAL);
        daemon.next_status().await;
        self.shown(Style::Light)
    }

    pub fn slot_inode(&self, style: Style) -> u64 {
        inode(&self.paths().slot(style))
    }

    pub fn slot_uri(&self, style: Style) -> String {
        slots::file_uri(&self.paths().slot(style)).expect("an absolute slot path")
    }

    /// The `file://` URI of the image `name` of `style`: what uri mode
    /// writes to the key.
    pub fn image_uri(&self, style: Style, name: &str) -> String {
        slots::file_uri(&self.image(style, name)).expect("an absolute image path")
    }

    /// The state file as the daemon last saved it.
    pub fn saved_state(&self) -> Persisted {
        state::load(&self.paths().state_file)
            .expect("a readable state file")
            .expect("a state file")
    }

    /// How often the daemon logged `text`.
    pub fn times_logged(&self, text: &str) -> usize {
        self.log.text().matches(text).count()
    }

    pub fn logged(&self, text: &str) -> bool {
        self.times_logged(text) > 0
    }

    /// Waits until the daemon logged `text`.
    pub async fn log_line(&self, text: &str) {
        eventually(&format!("the log line {text:?}"), || self.logged(text)).await;
    }
}

/// Writes an image that passes the check, with `mark` after its last chunk.
pub fn write_image(path: &Path, mark: &str) {
    fs::write(path, image_bytes(mark)).expect("write an image");
}

/// The bytes of an image that passes the check, with `mark` after its last
/// chunk.
pub fn image_bytes(mark: &str) -> Vec<u8> {
    [TINY_PNG, mark.as_bytes()].concat()
}

fn no_environment(_name: &str) -> Option<OsString> {
    None
}

/// What the daemon logs on the scenario's thread, as formatted lines.
#[derive(Clone, Default)]
struct LogBook {
    lines: Arc<Mutex<Vec<u8>>>,
}

impl LogBook {
    /// The interpreter runs on the scenario's thread, so its log lines come
    /// here. Lines from the threads of a watch do not.
    fn collect_this_threads_log(&self) -> DefaultGuard {
        let book = self.clone();
        let collector = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || book.clone())
            .finish();
        tracing::subscriber::set_default(collector)
    }

    fn text(&self) -> String {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&lines).into_owned()
    }
}

impl io::Write for LogBook {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        // A daemon that logs without end fails its scenario by the timeout.
        if lines.len() < MAX_LOG_BYTES {
            lines.extend_from_slice(line);
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A running interpreter and the scenario's hold on it.
pub struct Daemon {
    notices: UnboundedReceiver<Notice>,
    stop: Option<oneshot::Sender<()>>,
    run: JoinHandle<Result<(), RunError>>,
    /// The connection that owns the bus name, when the daemon serves its
    /// control interface.
    _bus: Option<zbus::Connection>,
}

impl Daemon {
    /// Waits for `READY=1` and returns the status that follows it. On a
    /// first run that is before any scan: no slot is filled yet.
    pub async fn ready(&mut self) -> Status {
        assert_eq!(self.next_notice().await, Notice::Ready);
        self.next_status().await
    }

    pub async fn next_status(&mut self) -> Status {
        match self.next_notice().await {
            Notice::Status(status) => status,
            Notice::Ready => panic!("a status was expected, not READY=1"),
        }
    }

    /// Waits for the first status that `holds` accepts and returns it.
    /// `READY=1` and the statuses before it are skipped. `what` names the
    /// status in the failure message.
    pub async fn status_where(&mut self, what: &str, holds: impl Fn(&Status) -> bool) -> Status {
        let mut skipped = Vec::new();
        loop {
            let notice = timeout(PATIENCE, self.notices.recv()).await;
            match notice {
                Ok(Some(Notice::Status(status))) if holds(&status) => return status,
                Ok(Some(other)) => skipped.push(other),
                Ok(None) => panic!("the daemon ended while waiting for {what}"),
                Err(_) => panic!("timed out waiting for {what}; skipped {skipped:#?}"),
            }
        }
    }

    async fn next_notice(&mut self) -> Notice {
        timeout(PATIENCE, self.notices.recv())
            .await
            .expect("a notice within the timeout")
            .expect("the daemon to be running")
    }

    /// Asks the daemon to stop the way `SIGTERM` does, and keeps the task
    /// so the scenario can wait for the answer to a request already in flight.
    pub fn signal_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).expect("the daemon to be running");
        }
    }

    /// Stops the daemon the way `SIGTERM` does and waits until it is gone.
    pub async fn terminate(mut self) {
        self.signal_stop();
        let outcome = stopped(self.run).await;
        assert!(outcome.is_ok(), "the daemon failed: {outcome:?}");
    }

    /// Waits until the daemon's run has ended by itself and returns how.
    pub async fn outcome(self) -> Result<(), RunError> {
        stopped(self.run).await
    }
}

/// Waits until the daemon's run has ended.
async fn stopped(run: JoinHandle<Result<(), RunError>>) -> Result<(), RunError> {
    timeout(PATIENCE, run)
        .await
        .expect("the daemon to stop within the timeout")
        .expect("the daemon not to panic")
}

/// GNOME's settings, held in memory. Like GNOME, it reports every change of
/// a key, the daemon's own writes included, and no write that changes
/// nothing.
#[derive(Clone)]
pub struct FakeGnome {
    session: Rc<FakeSession>,
}

struct FakeSession {
    snapshot: RefCell<GnomeSnapshot>,
    /// Where changes are reported: the daemon that runs now.
    events: RefCell<Option<UnboundedSender<GnomeEvent>>>,
    /// Every key write that a daemon asked for, oldest first.
    key_writes: RefCell<Vec<KeyWrite>>,
    /// Every restore of the keys that a daemon asked for, oldest first.
    restores: RefCell<Vec<BackgroundKeys>>,
    /// The next restore waits here. The sender tells the test it has
    /// entered; the receiver lets it continue. Before any key changes.
    restore_hold: RefCell<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    /// The next key write lands, but its caller hears no answer.
    unanswered_write: Cell<bool>,
}

impl FakeGnome {
    fn showing(visible: Style) -> FakeGnome {
        let session = FakeSession {
            snapshot: RefCell::new(GnomeSnapshot {
                visible,
                keys: users_keys(),
                locked: PerStyle::default(),
            }),
            events: RefCell::new(None),
            key_writes: RefCell::new(Vec::new()),
            restores: RefCell::new(Vec::new()),
            restore_hold: RefCell::new(None),
            unanswered_write: Cell::new(false),
        };
        FakeGnome {
            session: Rc::new(session),
        }
    }

    /// The event stream for a daemon that starts now.
    pub fn connect(&self) -> UnboundedReceiver<GnomeEvent> {
        let (events, inbox) = mpsc::unbounded_channel();
        self.session.events.replace(Some(events));
        inbox
    }

    /// Ends the event stream, as a dead settings thread does.
    pub fn disconnect(&self) {
        self.session.events.replace(None);
    }

    pub fn keys(&self) -> BackgroundKeys {
        self.session.snapshot.borrow().keys.clone()
    }

    /// What `style`'s key holds. `None` when the schema has no such key.
    pub fn key(&self, style: Style) -> Option<String> {
        self.keys().uri(style).map(str::to_owned)
    }

    /// Every key write that a daemon asked for, oldest first.
    pub fn key_writes(&self) -> Vec<KeyWrite> {
        self.session.key_writes.borrow().clone()
    }

    /// The next `restore_keys` tells the test it has entered, then waits
    /// for `release`, before it changes a key.
    pub fn hold_the_next_restore(&self) -> (oneshot::Sender<()>, oneshot::Receiver<()>) {
        let (release, released) = oneshot::channel();
        let (entered_sender, entered) = oneshot::channel();
        *self.session.restore_hold.borrow_mut() = Some((entered_sender, released));
        (release, entered)
    }

    /// The next `set_keys` stores its keys and reports them, then fails
    /// with [`GnomeError::NoAnswer`], as a settings thread that answers too
    /// late does.
    pub fn leave_the_next_write_unanswered(&self) {
        self.session.unanswered_write.set(true);
    }

    /// Every restore of the keys that a daemon asked for, oldest first.
    pub fn restores(&self) -> Vec<BackgroundKeys> {
        self.session.restores.borrow().clone()
    }

    /// Sets the keys before a daemon starts: nothing is reported.
    pub fn start_with_keys(&self, keys: BackgroundKeys) {
        self.session.snapshot.borrow_mut().keys = keys;
    }

    /// Locks both URI keys, as an administrator does.
    pub fn lock_keys(&self) {
        for style in Style::ALL {
            self.lock_key(style);
        }
    }

    /// Locks `style`'s URI key, as an administrator does.
    pub fn lock_key(&self, style: Style) {
        self.session.snapshot.borrow_mut().locked[style] = true;
    }

    /// Reports a change of connectivity, as `GNetworkMonitor` does.
    pub fn change_connectivity(&self, level: Connectivity) {
        self.report(GnomeEvent::ConnectivityChanged(level));
    }

    /// Switches GNOME's style, as the user does in Settings.
    pub fn switch_style(&self, style: Style) {
        self.session.snapshot.borrow_mut().visible = style;
        self.report(GnomeEvent::StyleChanged(style));
    }

    /// Puts `uri` in `style`'s key, as the user does when picking a
    /// wallpaper in Settings, or as another tool does.
    pub fn set_key_from_outside(&self, style: Style, uri: &str) {
        {
            let keys = &mut self.session.snapshot.borrow_mut().keys;
            match style {
                Style::Light => keys.picture_uri = uri.to_owned(),
                Style::Dark => keys.picture_uri_dark = Some(uri.to_owned()),
            }
        }
        self.report(GnomeEvent::KeysChanged(self.keys()));
    }

    /// Puts `keys` in all background keys in one apply, as Settings does
    /// when the user picks a tile with a light and a dark file.
    pub fn set_keys_from_outside(&self, keys: BackgroundKeys) {
        self.session.snapshot.borrow_mut().keys = keys;
        self.report(GnomeEvent::KeysChanged(self.keys()));
    }

    fn report(&self, event: GnomeEvent) {
        if let Some(events) = &*self.session.events.borrow() {
            // Nobody listens once the daemon has stopped.
            let _ = events.send(event);
        }
    }

    /// Writes `uri` to `style`'s key, unless the schema has no such key.
    /// `Err` with the key's name when the key is locked on another value.
    fn write_uri(&self, style: Style, uri: String) -> Result<(), &'static str> {
        let mut snapshot = self.session.snapshot.borrow_mut();
        let is_locked = snapshot.locked[style];
        let (key, name) = match style {
            Style::Light => (Some(&mut snapshot.keys.picture_uri), "picture-uri"),
            Style::Dark => (snapshot.keys.picture_uri_dark.as_mut(), "picture-uri-dark"),
        };
        match key {
            Some(held) if is_locked && *held != uri => Err(name),
            Some(held) => {
                *held = uri;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl GnomePort for FakeGnome {
    async fn snapshot(&self) -> Result<GnomeSnapshot, GnomeError> {
        Ok(self.session.snapshot.borrow().clone())
    }

    async fn set_keys(&self, write: KeyWrite) -> Result<(), GnomeError> {
        let writes_so_far = {
            let mut key_writes = self.session.key_writes.borrow_mut();
            key_writes.push(write.clone());
            key_writes.len()
        };
        assert!(
            writes_so_far <= MAX_KEY_WRITES,
            "the daemon writes the keys without end"
        );
        // A locked key on another value fails before any key of the write
        // changes, including `picture-options`.
        let locked = self.names_locked_on_another_value(&write);
        if !locked.is_empty() {
            return Err(GnomeError::Write {
                reason: format!("locked keys: {}", locked.join(", ")),
                unstored: names_of_write(&write),
            });
        }
        let before = self.keys();
        for style in Style::ALL {
            if let Some(uri) = write.uris[style].clone() {
                self.write_uri(style, uri).expect("the lock was checked");
            }
        }
        if let Some(options) = write.picture_options {
            self.session.snapshot.borrow_mut().keys.picture_options = options.as_str().to_owned();
        }
        let after = self.keys();
        if after != before {
            self.report(GnomeEvent::KeysChanged(after));
        }
        if self.session.unanswered_write.take() {
            return Err(GnomeError::NoAnswer);
        }
        Ok(())
    }

    async fn restore_keys(&self, keys: BackgroundKeys) -> Result<(), GnomeError> {
        self.session.restores.borrow_mut().push(keys.clone());
        let hold = self.session.restore_hold.borrow_mut().take();
        if let Some((entered, release)) = hold {
            let _ = entered.send(());
            let _ = release.await;
        }
        let before = self.keys();
        // As in `Session::restore`: every key that is not locked on another
        // value is written, and the call fails naming the locked ones.
        let mut locked = Vec::new();
        locked.extend(self.write_uri(Style::Light, keys.picture_uri).err());
        if let Some(dark) = keys.picture_uri_dark {
            locked.extend(self.write_uri(Style::Dark, dark).err());
        }
        self.session.snapshot.borrow_mut().keys.picture_options = keys.picture_options;
        let after = self.keys();
        if after != before {
            self.report(GnomeEvent::KeysChanged(after));
        }
        if locked.is_empty() {
            return Ok(());
        }
        Err(GnomeError::Write {
            reason: format!("locked keys: {}", locked.join(", ")),
            unstored: locked.into_iter().map(str::to_owned).collect(),
        })
    }
}

impl FakeGnome {
    /// URI keys of `write` that are locked on a different value.
    fn names_locked_on_another_value(&self, write: &KeyWrite) -> Vec<&'static str> {
        let snapshot = self.session.snapshot.borrow();
        let mut names = Vec::new();
        for style in Style::ALL {
            let Some(uri) = write.uris[style].as_deref() else {
                continue;
            };
            let held = snapshot.keys.uri(style);
            if snapshot.locked[style] && held.is_some_and(|current| current != uri) {
                names.push(match style {
                    Style::Light => "picture-uri",
                    Style::Dark => "picture-uri-dark",
                });
            }
        }
        names
    }
}

fn names_of_write(write: &KeyWrite) -> Vec<String> {
    let mut names = Vec::new();
    if write.uris.light.is_some() {
        names.push("picture-uri".to_owned());
    }
    if write.uris.dark.is_some() {
        names.push("picture-uri-dark".to_owned());
    }
    if write.picture_options.is_some() {
        names.push("picture-options".to_owned());
    }
    names
}

/// A wall clock that stands still until the scenario moves it.
#[derive(Clone)]
pub struct FakeClock {
    time: Rc<FakeTime>,
}

struct FakeTime {
    now: Cell<SystemTime>,
    armed: Cell<Option<SystemTime>>,
    /// How often the daemon set or cleared the alarm.
    times_armed: Cell<usize>,
    /// The clock was stepped and the daemon has not been told yet.
    was_stepped: Cell<bool>,
    /// How often the alarm went off.
    alarms: Cell<usize>,
    /// Signalled when `now`, `armed` or `was_stepped` changed.
    changed: Notify,
    /// The next `arm(None)` fails once. A test double for a disarm error.
    fail_disarm: Cell<bool>,
}

impl FakeClock {
    fn at(now: SystemTime) -> FakeClock {
        let time = FakeTime {
            now: Cell::new(now),
            armed: Cell::new(None),
            times_armed: Cell::new(0),
            was_stepped: Cell::new(false),
            alarms: Cell::new(0),
            changed: Notify::new(),
            fail_disarm: Cell::new(false),
        };
        FakeClock {
            time: Rc::new(time),
        }
    }

    pub fn time(&self) -> SystemTime {
        self.time.now.get()
    }

    /// Lets time pass, as it does while the daemon sleeps or the machine is
    /// suspended: an alarm that is due by then goes off.
    pub fn advance(&self, by: Duration) {
        self.time.now.set(self.time() + by);
        self.time.changed.notify_one();
    }

    /// Steps the clock, as NTP or the user does. The daemon is told that
    /// its alarm may be stale, not that it went off.
    pub fn step_to(&self, now: SystemTime) {
        self.time.now.set(now);
        self.time.was_stepped.set(true);
        self.time.changed.notify_one();
    }

    pub fn armed(&self) -> Option<SystemTime> {
        self.time.armed.get()
    }

    /// How often the daemon set or cleared the alarm so far.
    pub fn times_armed(&self) -> usize {
        self.time.times_armed.get()
    }

    /// The next disarm (`arm(None)`) fails once.
    pub fn fail_the_next_disarm(&self) {
        self.time.fail_disarm.set(true);
    }

    fn is_due(&self) -> bool {
        self.armed().is_some_and(|deadline| deadline <= self.time())
    }

    fn count_alarm(&self) {
        let alarms = self.time.alarms.get() + 1;
        self.time.alarms.set(alarms);
        assert!(
            alarms <= MAX_ALARMS,
            "the daemon arms an alarm that is due without end"
        );
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        self.time()
    }

    fn arm(&self, deadline: Option<SystemTime>) -> io::Result<()> {
        if deadline.is_none() && self.time.fail_disarm.replace(false) {
            return Err(io::Error::other("disarm failed"));
        }
        self.time.armed.set(deadline);
        self.time.times_armed.set(self.times_armed() + 1);
        self.time.changed.notify_one();
        Ok(())
    }

    async fn wait(&self) -> io::Result<Fired> {
        // A real timer never fires without the loop going to sleep first.
        // Without this, a loop that re-arms a due deadline would starve the
        // scenario's timeouts and hang the test instead of failing it.
        tokio::task::yield_now().await;
        loop {
            if self.time.was_stepped.take() {
                return Ok(Fired::ClockChanged);
            }
            if self.is_due() {
                break;
            }
            self.time.changed.notified().await;
        }
        // One shot, like a timerfd.
        self.time.armed.set(None);
        self.count_alarm();
        Ok(Fired::Deadline)
    }
}

/// The image check of a scenario that is not about decoding: the magic-byte
/// read of [`probe::probe`], and no glycin. Such a scenario then runs where
/// glycin has no loaders or no `bwrap`, where the real check refuses every
/// image. The read blocks the daemon's thread, which is fine for the small
/// local files of a scenario.
struct MagicBytesOnly;

impl ImageCheck for MagicBytesOnly {
    fn check<'a>(&'a self, path: &'a Path, _max_pixels: u64) -> CheckFuture<'a> {
        Box::pin(async move { probe::probe(path) })
    }
}

/// Whether glycin can decode in its sandbox here: asked of glycin itself,
/// once, on a thread and in a runtime of its own (glycin keeps its thread on
/// the first runtime that uses it, so that runtime is never dropped). Without
/// it the real check refuses every image, and a scenario that needs a decode
/// (pixel counts, a cut-off file) skips; tests/probe.rs covers that refusal.
pub fn glycin_decodes() -> bool {
    static DECODES: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        let ask = || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a tokio runtime");
            let tiny = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.png");
            let mut loader = glycin::Loader::new(gio::File::for_path(tiny));
            loader.sandbox_selector(glycin::SandboxSelector::Bwrap);
            let decodes = runtime.block_on(async { loader.load().await.is_ok() });
            std::mem::forget(runtime);
            decodes
        };
        std::thread::spawn(ask).join().expect("ask glycin")
    });
    if !*DECODES {
        eprintln!("skipped: glycin cannot decode in its sandbox here");
    }
    *DECODES
}
