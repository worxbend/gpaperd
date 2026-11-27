//! Keeps the catalogs and the slot files current while the daemon runs.
//!
//! A [`SourceWatch`] scans the local sources of one style and then follows
//! them: with inotify where that works, by polling where it does not. A
//! [`SlotsWatch`] reports a slot file that someone removed. The rules are the
//! "Watching" list in the plan's "Sources, catalog and ordering" section and
//! the Local and Slots rows of its corner-case table.
//!
//! Both watches take an inotify event only as a hint to look again. What they
//! report is read from the disk: a source is scanned again and compared with
//! its last scan, and a slot is looked up by its name. An event that is lost,
//! late or doubled can therefore delay a report but cannot make it wrong, and
//! a poll is the same look without the hint.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use gpaperd_core::catalog::{FileId, FileStamp, FsChange, LocalFile};
use gpaperd_core::config::{DirOptions, Source};
use gpaperd_core::style::{PerStyle, Style};
use notify_debouncer_full::notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};
use rustix::io::Errno;
use tokio::sync::mpsc;
use tokio::time::sleep_until;

use crate::scan::{self, Scan};
use crate::slots;

/// How long the debouncer holds an inotify event of a source back. A copy or
/// an editor's save is a burst of events.
pub const SOURCE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Poll period of a source that cannot use inotify and sets no `poll` option:
/// a network or FUSE file system, or a source that hit the inotify watch
/// limit.
pub const DEFAULT_POLL_PERIOD: Duration = Duration::from_secs(5 * 60);

/// How long the debouncer must hand over nothing for a source before its
/// burst counts as over: two of the debouncer's ticks. The debouncer alone
/// only delays events; a file that is written for ten seconds would be
/// reported ten seconds long.
const BURST_GAP: Duration = Duration::from_millis(250);

/// Longest time that a burst holds its report back. A download that never
/// pauses is then reported in parts, not never.
const MAX_BURST: Duration = Duration::from_secs(5);

/// How often a watch is compared with the directory that its path names now.
/// Nothing else notices a mount, an unmount or a renamed parent directory:
/// inotify stays on the old inode and reports nothing. (notify drops
/// `IN_UNMOUNT`.)
const WATCH_CHECK_PERIOD: Duration = Duration::from_secs(30);

/// How long the first scan of a watch waits for its remote scans. A remote
/// scan that takes longer is reported later, as found files, so a stuck
/// server does not hold the other sources.
const FIRST_SCAN_REMOTE_WAIT: Duration = Duration::from_secs(5);

// `f_type` values of statfs(2) for the file systems that another machine can
// change, so that inotify does not see every change. FUSE covers sshfs,
// rclone and the like.
const NFS_SUPER_MAGIC: u32 = 0x6969;
const CIFS_SUPER_MAGIC: u32 = 0xFF53_4D42;
const SMB2_SUPER_MAGIC: u32 = 0xFE53_4D42;
const FUSE_SUPER_MAGIC: u32 = 0x6573_5546;

/// What a [`SourceWatch`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceNews {
    /// The full scan of the sources. Always the first report of a watch, and
    /// sent only once. One path is listed once, with the greater stamp: the
    /// catalog keeps that stamp too, and it is the one later changes are
    /// compared with.
    Scanned(Scan),
    /// What changed since the last report: first every
    /// [`FsChange::Removed`], then every [`FsChange::Found`], each group in
    /// the order of the paths. A renamed file is one of each, and its
    /// removal comes first, so that its inode is never in the catalog under
    /// two paths. Never empty.
    Changed(Vec<FsChange>),
}

/// The local sources of one style: scanned once, then watched.
///
/// The scan and every file-system call run on a thread of the watch.
/// Dropping the watch stops it and discards the news that was not read, so
/// that no news of an old set of sources is ever mixed with a new one. To
/// replace the set of sources, start a new watch and drop this one: the scan
/// of the new watch holds whatever happened in between.
#[derive(Debug)]
pub struct SourceWatch {
    sources: Vec<Source>,
    news: mpsc::UnboundedReceiver<SourceNews>,
    _worker: StopOnDrop<DebounceEventResult>,
    /// When the worker was last started: the next start waits for the
    /// watch check after it. `None` before the first start.
    restarted_at: Option<tokio::time::Instant>,
}

impl SourceWatch {
    /// Starts to scan and to watch `sources`, and returns at once.
    ///
    /// Nothing here fails. A source that cannot be scanned is a warning in
    /// the [`Scan`]. A source that cannot be watched is polled, as listed
    /// below; a directory that inotify refuses for another reason, such as
    /// one that cannot be read, is tried again every 30 s.
    ///
    /// What is watched, per source:
    /// - A directory: that directory, and with `recursive` every directory
    ///   below it that the scan enters. The filters of the scan (extensions,
    ///   hidden entries, in-progress names, depth) decide which files count.
    /// - A file: its parent directory, for that one name.
    /// - A path that does not exist: its nearest existing ancestor. When the
    ///   path appears, its files are [`FsChange::Found`] and the watch moves
    ///   to it.
    /// - A directory that is removed or unmounted: every file of it is
    ///   [`FsChange::Removed`], and its nearest existing ancestor is watched
    ///   until it comes back. A removal is seen at once. An unmount, a mount
    ///   and a renamed parent directory raise no event, and are seen within
    ///   30 s.
    /// - A [`Source::Url`] is not watched.
    ///
    /// When a source is polled and not watched with inotify:
    /// - Its `poll` option is set: on that period, whatever the file system.
    /// - `statfs` says NFS, CIFS, SMB2 or FUSE: every
    ///   [`DEFAULT_POLL_PERIOD`], because changes made on another machine
    ///   raise no inotify event.
    /// - A watch fails with `ENOSPC`, which notify reports as
    ///   `ErrorKind::MaxFilesWatch`, or inotify has no instance left: every
    ///   [`DEFAULT_POLL_PERIOD`] for as long as this watch lives, after one
    ///   log line per source with the `fs.inotify.max_user_watches` hint.
    ///   The source gives its other watches back.
    ///
    /// A poll scans the source again and reports only what differs from the
    /// `path → (dev, ino, size, mtime)` table of the scan before it.
    pub fn start(sources: Vec<Source>) -> SourceWatch {
        let mut watch = SourceWatch {
            sources,
            news: mpsc::unbounded_channel().1,
            _worker: StopOnDrop::detached(),
            restarted_at: None,
        };
        watch.restart();
        watch
    }

    /// Waits for the next report.
    ///
    /// The first one is [`SourceNews::Scanned`]. Each later one is a
    /// [`SourceNews::Changed`] with what one look at the sources found. A
    /// look is one poll, or it follows a burst of inotify events: once the
    /// debouncer handed over nothing more for a quarter of a second, and at
    /// the latest 5 s after the burst began.
    ///
    /// A change is reported per path, over all sources of the watch:
    /// - [`FsChange::Found`] for a new image file, and for a known one whose
    ///   stamp differs from the one last reported: edited in place, or
    ///   replaced by an atomic save or a rename onto its name. The
    ///   [`LocalFile`] is built as the scan builds it: a canonical path and
    ///   a stamp from `stat`.
    /// - [`FsChange::Removed`] once no source reaches the path any more, with
    ///   the path as it was last reported.
    /// - Nothing for a path that ends a burst as it started it.
    ///
    /// Waits forever when nothing changes. Cancel-safe: news that arrives
    /// while nobody waits is returned by the next call.
    ///
    /// A thread that stops, or that never started, is replaced. The
    /// replacement begins with a new scan. A thread is started at most once
    /// per watch check (30 s), so one that dies at once does not spin.
    pub async fn next(&mut self) -> SourceNews {
        loop {
            match self.news.recv().await {
                Some(news) => return news,
                None => {
                    tracing::error!("source watch stopped: starting it again");
                    self.restart_or_wait().await;
                }
            }
        }
    }

    /// Starts the worker again. The previous thread, if it is still alive,
    /// is stopped.
    fn restart(&mut self) {
        let (news, news_reader) = mpsc::unbounded_channel();
        let (wake, wakes) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = StopOnDrop {
            wake: wake.clone(),
            stop: Arc::clone(&stop),
        };
        let sources = self.sources.clone();
        spawn_worker("gpaperd-sources", move || {
            Follower::start(&sources, news, wake, Instant::now(), stop).run(&wakes);
        });
        self.news = news_reader;
        self._worker = worker;
        self.restarted_at = Some(tokio::time::Instant::now());
    }

    async fn restart_or_wait(&mut self) {
        wait_for_restart(self.restarted_at).await;
        self.restart();
    }
}

/// Watches the slots directory for a slot file that someone removed.
///
/// Dropping the watch stops it.
#[derive(Debug)]
pub struct SlotsWatch {
    slots_dir: PathBuf,
    lost: mpsc::UnboundedReceiver<Style>,
    _worker: StopOnDrop<notify::Result<notify::Event>>,
    /// When the lookout was last started: the next start waits for the
    /// watch check after it.
    restarted_at: Option<tokio::time::Instant>,
}

impl SlotsWatch {
    /// Starts to watch `slots_dir`.
    ///
    /// The first look at the slots happens here, on the caller's thread: a
    /// few `stat` calls and one inotify watch. So a slot that is there when
    /// this returns is reported when it is gone, however soon that is.
    ///
    /// The directory need not exist: the first publish creates it. Until
    /// then its nearest existing ancestor is watched. A watch that cannot be
    /// set up is logged with its path; a lost slot is then seen within 30 s.
    pub fn start(slots_dir: PathBuf) -> SlotsWatch {
        let (lost, lost_reader) = mpsc::unbounded_channel();
        let (wake, wakes) = channel();
        let worker = StopOnDrop {
            wake: wake.clone(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let mut lookout = Lookout {
            slots_dir: slots_dir.clone(),
            inotify: None,
            watched: None,
            there: PerStyle::default(),
            lost,
            wake,
        };
        // Before the thread: a slot that is here when this returns is
        // reported when it goes, however soon that is.
        lookout.look();
        // A failed spawn drops `lookout` and closes `lost_reader`. `lost`
        // replaces the watch on the watch check.
        spawn_worker("gpaperd-slots", move || lookout.run(&wakes));
        SlotsWatch {
            slots_dir,
            lost: lost_reader,
            _worker: worker,
            restarted_at: Some(tokio::time::Instant::now()),
        }
    }

    /// Waits until a slot file is gone and returns its style.
    ///
    /// A slot is gone when its name was there and is not any more: the file
    /// was deleted or moved away, or the directory was. It is reported at
    /// once, with no debounce: the desktop is blank until the slot is back.
    /// A slot that was never there is not reported. Nor is a publish, which
    /// renames a new file onto the name: at no moment is the name missing.
    /// After a report, the same style is reported again only once its slot
    /// was there again.
    ///
    /// Waits forever when nothing happens. Cancel-safe: a loss that is seen
    /// while nobody waits is returned by the next call.
    ///
    /// A thread that stops, or that never started, is replaced. A thread is
    /// started at most once per watch check (30 s), so one that dies at once
    /// does not spin.
    pub async fn lost(&mut self) -> Style {
        loop {
            match self.lost.recv().await {
                Some(style) => return style,
                None => {
                    tracing::error!("slots watch stopped: starting it again");
                    self.restart_or_wait().await;
                }
            }
        }
    }

    /// Starts the lookout again. The first look runs on that thread, so a
    /// replacement does not stat on the caller's runtime thread.
    fn restart(&mut self) {
        let (lost, lost_reader) = mpsc::unbounded_channel();
        let (wake, wakes) = channel();
        let worker = StopOnDrop {
            wake: wake.clone(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let slots_dir = self.slots_dir.clone();
        spawn_worker("gpaperd-slots", move || {
            let mut lookout = Lookout {
                slots_dir,
                inotify: None,
                watched: None,
                there: PerStyle::default(),
                lost,
                wake,
            };
            lookout.look();
            lookout.run(&wakes);
        });
        self.lost = lost_reader;
        self._worker = worker;
        self.restarted_at = Some(tokio::time::Instant::now());
    }

    async fn restart_or_wait(&mut self) {
        wait_for_restart(self.restarted_at).await;
        self.restart();
    }
}

/// Why the thread of a watch wakes up.
enum Wake<Seen> {
    /// inotify reported something.
    Seen(Seen),
    /// The time that the thread asked for has come.
    Due,
    /// The watch was dropped.
    Stop,
}

/// The end of a watch's thread that the watch itself holds: dropping it
/// stops the thread, including a rescan that has not returned to its wake
/// loop yet.
#[derive(Debug)]
struct StopOnDrop<Seen> {
    wake: Sender<Wake<Seen>>,
    stop: Arc<AtomicBool>,
}

impl<Seen> StopOnDrop<Seen> {
    /// A handle that is not connected to a thread yet.
    fn detached() -> StopOnDrop<Seen> {
        StopOnDrop {
            wake: channel().0,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<Seen> Drop for StopOnDrop<Seen> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // An error says that the thread has ended already.
        let _ = self.wake.send(Wake::Stop);
    }
}

/// Waits until the watch check after `restarted_at`, so that a thread that
/// dies at once is not started again in a tight loop. Cancel-safe: a
/// cancelled wait leaves the channel closed, and the next call waits for the
/// same moment. `None`, never started, does not wait.
async fn wait_for_restart(restarted_at: Option<tokio::time::Instant>) {
    if let Some(restarted_at) = restarted_at {
        sleep_until(restarted_at + WATCH_CHECK_PERIOD).await;
    }
}

/// Runs `work` on a named thread. `false` when the system has no thread to
/// give: the closure is dropped, and whatever channel it held closes.
fn spawn_worker(name: &str, work: impl FnOnce() + Send + 'static) -> bool {
    match thread::Builder::new().name(name.to_owned()).spawn(work) {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(thread = name, reason = %error, "watch not started");
            false
        }
    }
}

/// Blocks until inotify reports, `due` passes or the watch is dropped.
/// Without a `due` time only the other two end the wait.
fn next_wake<Seen>(wakes: &Receiver<Wake<Seen>>, due: Option<Instant>) -> Wake<Seen> {
    let Some(due) = due else {
        return wakes.recv().unwrap_or(Wake::Stop);
    };
    match wakes.recv_timeout(due.saturating_duration_since(Instant::now())) {
        Ok(wake) => wake,
        Err(RecvTimeoutError::Timeout) => Wake::Due,
        Err(RecvTimeoutError::Disconnected) => Wake::Stop,
    }
}

/// Whether an inotify event of this kind leaves everything as it was.
/// Opening and reading changes nothing, and a scan opens every directory
/// that it lists: it would wake itself.
fn is_no_change(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Access(_))
}

/// Whether an event of this kind ends inotify's watch of its path, when
/// that path is a watched directory: the directory was removed or renamed
/// away. notify forgets the watch then. A directory that takes the place,
/// even on the same inode number, has to be watched anew.
fn ends_a_watch(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Whether inotify refused for want of a watch or of an instance. Asking
/// again does not help then, and takes from the sources that have theirs.
fn is_inotify_limit(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        // inotify_init(2): the user's limit of instances, or of open files.
        notify::ErrorKind::Io(error) => {
            let no_instance = [Errno::MFILE, Errno::NFILE].map(Errno::raw_os_error);
            error
                .raw_os_error()
                .is_some_and(|code| no_instance.contains(&code))
        }
        _ => false,
    }
}

/// Whether changes in `dir` can come from another machine. A `statfs` that
/// fails counts as "no": inotify is tried then, and says what is wrong.
fn is_on_remote_fs(dir: &Path) -> bool {
    // The magic number is 32 bits wide in a field of the machine's word size.
    rustix::fs::statfs(dir).is_ok_and(|fs| is_remote_fs_type(fs.f_type as u32))
}

fn is_remote_fs_type(fs_type: u32) -> bool {
    matches!(
        fs_type,
        NFS_SUPER_MAGIC | CIFS_SUPER_MAGIC | SMB2_SUPER_MAGIC | FUSE_SUPER_MAGIC
    )
}

/// A directory to put an inotify watch on, with the inode that its path
/// named when it was looked up. inotify watches the inode: another directory
/// under the same path needs a new watch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchPoint {
    dir: PathBuf,
    id: FileId,
}

impl WatchPoint {
    /// `path` itself, when it is a directory now. A path that cannot be
    /// looked up counts as missing.
    fn at(path: &Path) -> Option<WatchPoint> {
        let metadata = fs::metadata(path).ok()?;
        metadata.is_dir().then(|| WatchPoint {
            dir: path.to_owned(),
            id: scan::file_id(&metadata),
        })
    }

    /// The nearest directory above `path`: where `path` appears when it is
    /// created.
    fn above(path: &Path) -> Option<WatchPoint> {
        path.ancestors().skip(1).find_map(WatchPoint::at)
    }
}

/// `path → (dev, ino, size, mtime)`: the files of a scan.
type FileTable = BTreeMap<PathBuf, FileStamp>;

/// The directories that inotify watches, each with the inode it is on.
type Watched = BTreeMap<PathBuf, FileId>;

type SourceDebouncer = Debouncer<RecommendedWatcher, RecommendedCache>;

/// What turns the `before` table into `after`: removals first, see
/// [`SourceNews::Changed`].
fn changes_between(before: &FileTable, after: &FileTable) -> Vec<FsChange> {
    let removed = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .map(|path| FsChange::Removed(path.clone()));
    let found = after
        .iter()
        .filter(|(path, stamp)| before.get(*path) != Some(*stamp))
        .map(|(path, stamp)| {
            FsChange::Found(LocalFile {
                path: path.clone(),
                stamp: *stamp,
            })
        });
    removed.chain(found).collect()
}

/// How a source is kept current.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Following {
    /// inotify watches the directories of the source. While the source is no
    /// directory, it watches the nearest directory above it. Either way this
    /// is the first directory to watch.
    Watched(WatchPoint),
    /// The source is scanned again on this period.
    Polled(Duration),
}

/// How to follow the source at `path`, as the disk is now, and whether its
/// scan belongs off the follower: see [`Followed::place`]. `dir_options` is
/// `None` for a source that can only be a file.
fn locate(path: &Path, dir_options: Option<DirOptions>) -> (Following, bool) {
    let point = dir_options
        .and_then(|_| WatchPoint::at(path))
        .or_else(|| WatchPoint::above(path));
    let remote = point
        .as_ref()
        .is_some_and(|point| is_on_remote_fs(&point.dir));
    if let Some(period) = dir_options.and_then(|options| options.poll) {
        return (Following::Polled(period), remote);
    }
    match point {
        Some(_) if remote => (Following::Polled(DEFAULT_POLL_PERIOD), true),
        Some(point) => (Following::Watched(point), false),
        None => (Following::Polled(DEFAULT_POLL_PERIOD), false),
    }
}

/// A scan that ran off the follower, and where it found the source.
struct RemoteScan {
    index: usize,
    found: Scan,
    /// What [`locate`] said on the scan's thread.
    placed: (Following, bool),
}

/// One local source of a [`SourceWatch`], and how it is followed.
struct Followed {
    /// Never a [`Source::Url`].
    source: Source,
    /// The path that the source names.
    path: PathBuf,
    /// The options that the source is scanned with when it is a directory.
    /// `None` for a source that can only be a file.
    dir_options: Option<DirOptions>,
    /// The last scan of the source.
    found: Scan,
    following: Following,
    /// The directories that inotify has to watch for this source. Empty for
    /// a polled source.
    dirs: Watched,
    /// When to look at the source next: the end of a burst of events, the
    /// check of its watch, or its poll.
    due: Instant,
    /// When the burst of events that is not looked at yet began.
    burst_began: Option<Instant>,
    /// inotify had no watch left for the source. Asking again would take the
    /// watches of the sources that still have them.
    out_of_watches: bool,
    /// Where the scan runs. Aside and Out keep an NFS, CIFS, SMB, or FUSE
    /// read off the follower.
    seat: ScanSeat,
    /// A remote scan came back and has not been reported yet.
    report_scan: bool,
    /// Test double for a remote seat. A local temp dir is not a remote file
    /// system, and a stalled mount is not available here.
    #[cfg(test)]
    force_off_thread: bool,
}

/// Where a source is scanned.
enum ScanSeat {
    /// On the follower thread.
    Here,
    /// On its own thread, the next time it is due.
    Aside,
    /// On its own thread now.
    Out,
}

impl Followed {
    /// `None` for a remote source, which nothing watches.
    fn new(source: &Source, now: Instant) -> Option<Followed> {
        let (path, dir_options) = match source {
            Source::Path(path) => (path, Some(DirOptions::default())),
            Source::Dir { path, options } => (path, Some(*options)),
            Source::File(path) => (path, None),
            Source::Url(_) => return None,
        };
        Some(Followed {
            source: source.clone(),
            path: path.clone(),
            dir_options,
            found: Scan::default(),
            // Until the first look plans how to follow the source.
            following: Following::Polled(DEFAULT_POLL_PERIOD),
            dirs: Watched::new(),
            due: now,
            burst_began: None,
            out_of_watches: false,
            seat: ScanSeat::Here,
            report_scan: false,
            #[cfg(test)]
            force_off_thread: false,
        })
    }

    /// How the source should be followed, as the disk is now.
    fn plan(&self) -> Following {
        self.place().0
    }

    /// [`Followed::plan`] plus whether the scan belongs off the follower.
    /// One `statfs`: a poll option still asks, because an NFS, CIFS, SMB, or
    /// FUSE source with `poll` set is the same stuck scan.
    fn place(&self) -> (Following, bool) {
        if self.out_of_watches {
            return (Following::Polled(DEFAULT_POLL_PERIOD), false);
        }
        locate(&self.path, self.dir_options)
    }

    /// Takes `placed`, from [`Followed::place`]: how to follow the source,
    /// and whether its scan belongs off the follower.
    fn settle(&mut self, (planned, remote): (Following, bool)) {
        self.seat = if remote || self.forced_off_thread() {
            ScanSeat::Aside
        } else {
            ScanSeat::Here
        };
        self.follow(planned);
    }

    /// Follows the source as `planned`. A watched source starts over with
    /// the one directory of the plan; the next scan adds those below it.
    fn follow(&mut self, planned: Following) {
        if planned == self.following {
            return;
        }
        tracing::debug!(path = %self.path.display(), following = ?planned, "source followed");
        self.dirs = match &planned {
            Following::Watched(point) => Watched::from([(point.dir.clone(), point.id)]),
            Following::Polled(_) => Watched::new(),
        };
        self.following = planned;
    }

    /// Polls the source from now on: inotify has no watch left for it, as
    /// `limit` says.
    fn give_up_watching(&mut self, limit: &notify::Error) {
        tracing::warn!(
            path = %self.path.display(),
            reason = %limit,
            "inotify limit reached: the source is polled every 5 min; to watch it, raise the \
             sysctl fs.inotify.max_user_watches (or fs.inotify.max_user_instances), then run \
             `systemctl --user restart gpaperd`"
        );
        self.out_of_watches = true;
        self.seat = ScanSeat::Here;
        self.follow(Following::Polled(DEFAULT_POLL_PERIOD));
    }

    /// Whether the last scan may be out of date: the source is polled, has
    /// to be followed in another way, saw events, or a directory it needs
    /// watched has no watch. A nested directory inotify refused is included,
    /// so the watch check tries it again. A scan that is still off-thread
    /// is not stale: another one would only queue behind it.
    fn is_stale(&self, watched: &Watched) -> bool {
        if self.scan_is_out() {
            return false;
        }
        if self.scans_off_thread() {
            return true;
        }
        let planned = self.plan();
        match &planned {
            Following::Polled(_) => true,
            Following::Watched(_) => {
                self.burst_began.is_some()
                    || planned != self.following
                    || self.missing_watch(watched)
            }
        }
    }

    fn scans_off_thread(&self) -> bool {
        self.forced_off_thread() || !matches!(self.seat, ScanSeat::Here)
    }

    fn scan_is_out(&self) -> bool {
        matches!(self.seat, ScanSeat::Out)
    }

    fn forced_off_thread(&self) -> bool {
        #[cfg(test)]
        {
            self.force_off_thread
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// A directory this source wants watched, whose inode is not the one
    /// inotify is on. The root is one of those directories.
    fn missing_watch(&self, watched: &Watched) -> bool {
        self.dirs
            .iter()
            .any(|(dir, id)| watched.get(dir) != Some(id))
    }

    /// Scans the source, and takes the directories of the scan as the ones
    /// to watch.
    fn scan(&mut self) {
        self.found = scan::scan(slice::from_ref(&self.source));
        let Following::Watched(first) = &self.following else {
            return;
        };
        let scanned = self.found.dirs.iter().filter_map(|dir| WatchPoint::at(dir));
        self.dirs = [first.clone()]
            .into_iter()
            .chain(scanned)
            .map(|point| (point.dir, point.id))
            .collect();
    }

    /// Notes an event of the source at `now`. Its burst is over once
    /// nothing follows for [`BURST_GAP`], and after [`MAX_BURST`] whatever
    /// follows.
    fn stir(&mut self, now: Instant) {
        let began = *self.burst_began.get_or_insert(now);
        self.due = (now + BURST_GAP).min(began + MAX_BURST);
    }

    /// Ends a look at `now`: the next one is the check of the watch or the
    /// poll, unless events come first.
    fn rest(&mut self, now: Instant) {
        self.burst_began = None;
        self.due = now
            + match self.following {
                Following::Watched(_) => WATCH_CHECK_PERIOD,
                Following::Polled(period) => period,
            };
    }

    fn is_watched(&self) -> bool {
        matches!(self.following, Following::Watched(_))
    }
}

/// The thread behind a [`SourceWatch`]. It is a state machine that is fed
/// with wake-ups and the time, so that a test can drive it without waiting.
struct Follower {
    sources: Vec<Followed>,
    /// Made with the first watch, so that polled sources hold no inotify
    /// instance.
    inotify: Option<SourceDebouncer>,
    /// The directories that `inotify` watches now.
    watched: Watched,
    /// What the reader of the news knows: the files of all sources as they
    /// were last reported.
    reported: FileTable,
    news: mpsc::UnboundedSender<SourceNews>,
    /// Where a new debouncer sends what inotify reports.
    wake: Sender<Wake<DebounceEventResult>>,
    /// Scans of remote file systems, finished off this thread.
    remote_done: Receiver<RemoteScan>,
    remote_tx: Sender<RemoteScan>,
    /// Set when the watch is dropped, so a rescan returns before the wake
    /// loop would see [`Wake::Stop`].
    stop: Arc<AtomicBool>,
}

impl Follower {
    /// Watches and scans the local ones of `sources`, and reports the scan.
    fn start(
        sources: &[Source],
        news: mpsc::UnboundedSender<SourceNews>,
        wake: Sender<Wake<DebounceEventResult>>,
        now: Instant,
        stop: Arc<AtomicBool>,
    ) -> Follower {
        let mut follower = Follower::new(sources, news, wake, now, stop);
        follower.report_first_scan(now);
        follower
    }

    /// A follower that has looked at nothing yet.
    fn new(
        sources: &[Source],
        news: mpsc::UnboundedSender<SourceNews>,
        wake: Sender<Wake<DebounceEventResult>>,
        now: Instant,
        stop: Arc<AtomicBool>,
    ) -> Follower {
        let (remote_tx, remote_done) = channel();
        Follower {
            sources: sources
                .iter()
                .filter_map(|source| Followed::new(source, now))
                .collect(),
            inotify: None,
            watched: Watched::new(),
            reported: FileTable::new(),
            news,
            wake,
            remote_done,
            remote_tx,
            stop,
        }
    }

    /// Scans and watches every source, and reports the scan. The reader
    /// takes this report as the whole catalog, so it waits for the remote
    /// scans, up to [`FIRST_SCAN_REMOTE_WAIT`].
    fn report_first_scan(&mut self, now: Instant) {
        self.refresh((0..self.sources.len()).collect());
        self.wait_for_remote_scans(Instant::now() + FIRST_SCAN_REMOTE_WAIT);
        for followed in &mut self.sources {
            followed.rest(now);
        }
        self.reported = self.files_on_disk();
        self.report(SourceNews::Scanned(self.whole_scan()));
    }

    /// Follows the sources until the watch is dropped.
    fn run(mut self, wakes: &Receiver<Wake<DebounceEventResult>>) {
        loop {
            let next_look = self.sources.iter().map(|followed| followed.due).min();
            match next_wake(wakes, next_look) {
                Wake::Seen(seen) => self.note(seen, Instant::now()),
                Wake::Due => {}
                Wake::Stop => return,
            }
            self.look(Instant::now());
        }
    }

    /// Takes what the debouncer handed over at `now`: the watched sources
    /// that it concerns are looked at once their burst is over.
    fn note(&mut self, seen: DebounceEventResult, now: Instant) {
        let hit = match seen {
            Ok(events) => {
                let hit = self.dirs_hit(&events);
                self.forget_ended_watches(&events);
                hit
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!(reason = %error, "inotify failed: the sources are scanned again");
                }
                None
            }
        };
        for followed in &mut self.sources {
            let holds_a_hit =
                |hit: &BTreeSet<FileId>| followed.dirs.values().any(|id| hit.contains(id));
            if followed.is_watched() && hit.as_ref().is_none_or(holds_a_hit) {
                followed.stir(now);
            }
        }
    }

    /// The watched directories that `events` report a change in. `None`
    /// when inotify lost events: any directory can have changed.
    ///
    /// They are told by inode, not by path. Two paths can name one
    /// directory, and inotify then reports under one of them only.
    fn dirs_hit(&self, events: &[DebouncedEvent]) -> Option<BTreeSet<FileId>> {
        let mut hit = BTreeSet::new();
        for event in events {
            if event.need_rescan() {
                return None;
            }
            if is_no_change(&event.kind) {
                continue;
            }
            for path in &event.paths {
                // An entry of a watched directory, or such a directory
                // itself when it was removed or renamed.
                hit.extend(path.parent().and_then(|dir| self.watched.get(dir)));
                hit.extend(self.watched.get(path));
            }
        }
        Some(hit)
    }

    /// Forgets the watch of each watched directory that `events` report
    /// removed or renamed away, so that the next look watches it again. Its
    /// inode number tells nothing: a new directory can get the same one.
    fn forget_ended_watches(&mut self, events: &[DebouncedEvent]) {
        let ended = events
            .iter()
            .filter(|event| ends_a_watch(&event.kind))
            .flat_map(|event| &event.paths);
        for path in ended {
            if let Some(id) = self.watched.remove(path) {
                // Paths that name one directory shared the watch.
                self.watched.retain(|_, other| *other != id);
            }
        }
    }

    /// Looks at every source whose time has come at `now`, and reports what
    /// changed.
    fn look(&mut self, now: Instant) {
        self.take_remote_scans(now);
        let due: Vec<usize> = (0..self.sources.len())
            .filter(|&index| self.sources[index].due <= now)
            .collect();
        let stale: BTreeSet<usize> = due
            .iter()
            .copied()
            .filter(|&index| self.sources[index].is_stale(&self.watched))
            .collect();
        let nothing_scanned = stale.is_empty();
        self.refresh(stale);
        for index in due {
            self.sources[index].rest(now);
        }
        let pending_report = self.sources.iter().any(|followed| followed.report_scan);
        if nothing_scanned && !pending_report {
            return;
        }
        for followed in &mut self.sources {
            followed.report_scan = false;
        }
        self.report_changes();
    }

    /// Applies remote scans that finished, and schedules their next poll from
    /// `now` so a finished scan is not started again in this look.
    fn take_remote_scans(&mut self, now: Instant) {
        while let Ok(done) = self.remote_done.try_recv() {
            let index = done.index;
            self.apply_remote_scan(done);
            if let Some(followed) = self.sources.get_mut(index) {
                followed.report_scan = true;
                followed.rest(now);
            }
        }
    }

    /// Blocks until no remote scan is out, `deadline` passes, or the watch
    /// is dropped. A scan still out is taken by a later look.
    fn wait_for_remote_scans(&mut self, deadline: Instant) {
        while self.sources.iter().any(Followed::scan_is_out) && !self.stop.load(Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            // `Disconnected` cannot come: this follower holds a sender.
            let Ok(done) = self.remote_done.recv_timeout(left) else {
                return;
            };
            self.apply_remote_scan(done);
        }
    }

    fn apply_remote_scan(&mut self, done: RemoteScan) {
        let Some(followed) = self.sources.get_mut(done.index) else {
            return;
        };
        followed.found = done.found;
        // A source that ran out of watches meanwhile is placed without the
        // disk: polled, on this thread.
        let placed = if followed.out_of_watches {
            followed.place()
        } else {
            done.placed
        };
        followed.settle(placed);
    }

    fn report_changes(&mut self) {
        let on_disk = self.files_on_disk();
        let changes = changes_between(&self.reported, &on_disk);
        self.reported = on_disk;
        if !changes.is_empty() {
            self.report(SourceNews::Changed(changes));
        }
    }

    /// Scans the `stale` sources again, and makes inotify cover what they
    /// hold then.
    ///
    /// A directory is watched before it is scanned, so that nothing changes
    /// in it unseen. So every source that holds a directory which is watched
    /// only now is scanned too, a scan that finds new directories is
    /// followed by another one, and so is a scan of a source that moved
    /// meanwhile. [`Wake::Stop`] ends the loop between those passes.
    ///
    /// A remote file system is scanned on its own thread, after the passes
    /// have scanned the sources that stay on this one. Once a source scans
    /// there, its `stat` and `statfs` run there too. A stuck NFS, CIFS, SMB,
    /// or FUSE server then does not hold the other sources.
    fn refresh(&mut self, mut stale: BTreeSet<usize>) {
        while !stale.is_empty() {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            for &index in &stale {
                let followed = &mut self.sources[index];
                // A source that already scans aside is placed on its scan's
                // thread: a stat of a stalled mount blocks.
                if matches!(followed.seat, ScanSeat::Here) {
                    let placed = followed.place();
                    followed.settle(placed);
                }
            }
            let watched_before_the_scan = self.sync_watches();
            stale.extend(self.sources_holding(&watched_before_the_scan));
            let remote: Vec<usize> = stale
                .iter()
                .copied()
                .filter(|&index| self.sources[index].scans_off_thread())
                .collect();
            for &index in &stale {
                if !self.sources[index].scans_off_thread() {
                    self.sources[index].scan();
                }
            }
            for index in remote {
                self.start_remote_scan(index);
            }
            let watched_after_the_scan = self.sync_watches();
            stale.retain(|&index| {
                let followed = &self.sources[index];
                !followed.scans_off_thread() && followed.plan() != followed.following
            });
            let holding = self.sources_holding(&watched_after_the_scan);
            stale.extend(
                holding
                    .into_iter()
                    .filter(|&index| !self.sources[index].scan_is_out()),
            );
        }
    }

    /// Scans one remote source without waiting. A second call while the
    /// first scan is out does nothing.
    fn start_remote_scan(&mut self, index: usize) {
        if self.sources[index].scan_is_out() {
            return;
        }
        let followed = &self.sources[index];
        let source = followed.source.clone();
        let (path, dir_options) = (followed.path.clone(), followed.dir_options);
        let tx = self.remote_tx.clone();
        let wake = self.wake.clone();
        let spawned = spawn_worker("gpaperd-remote-scan", move || {
            #[cfg(test)]
            remote_scan_hold::wait();
            let placed = locate(&path, dir_options);
            let found = scan::scan(slice::from_ref(&source));
            let _ = tx.send(RemoteScan {
                index,
                found,
                placed,
            });
            let _ = wake.send(Wake::Due);
        });
        if spawned {
            self.sources[index].seat = ScanSeat::Out;
        }
    }

    /// Makes inotify watch the directories of the watched sources and no
    /// others. Returns the directories that are watched only from now on.
    fn sync_watches(&mut self) -> BTreeSet<PathBuf> {
        let before = self.watched.clone();
        // A source that runs out of watches gives up the ones it has. The
        // next pass hands them to the sources that still wait for theirs.
        while self.sync_pass().is_break() {}
        if self.watched.is_empty() {
            self.inotify = None;
        }
        self.watched
            .iter()
            .filter(|(dir, id)| before.get(*dir) != Some(*id))
            .map(|(dir, _)| dir.clone())
            .collect()
    }

    /// One try to watch what is wanted. Breaks off when inotify has no
    /// watch left.
    fn sync_pass(&mut self) -> ControlFlow<()> {
        let wanted = self.wanted_dirs();
        let unwanted: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(dir, id)| wanted.get(*dir) != Some(*id))
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in unwanted {
            self.unwatch(&dir);
        }
        for (dir, id) in wanted {
            if !self.watched.contains_key(&dir) {
                let answer = self.watch(&dir);
                self.watch_answered(WatchPoint { dir, id }, answer)?;
            }
        }
        ControlFlow::Continue(())
    }

    /// Every directory that a watched source wants inotify to watch.
    fn wanted_dirs(&self) -> Watched {
        self.sources
            .iter()
            .flat_map(|followed| &followed.dirs)
            .map(|(dir, id)| (dir.clone(), *id))
            .collect()
    }

    fn watch(&mut self, dir: &Path) -> notify::Result<()> {
        let inotify = match self.inotify.take() {
            Some(inotify) => inotify,
            None => self.new_inotify()?,
        };
        self.inotify
            .insert(inotify)
            .watch(dir, RecursiveMode::NonRecursive)
    }

    /// A debouncer on an inotify instance of its own, which reports to the
    /// thread of this follower.
    fn new_inotify(&self) -> notify::Result<SourceDebouncer> {
        // notify-debouncer-full 0.7.0 starts its thread before it opens
        // inotify, and leaves the thread running when that fails. So open
        // inotify once without it: a failure here leaves nothing behind.
        drop(notify::recommended_watcher(
            |_seen: notify::Result<notify::Event>| {},
        )?);
        let wake = self.wake.clone();
        new_debouncer(SOURCE_DEBOUNCE, None, move |seen: DebounceEventResult| {
            // An error says that the watch was dropped.
            let _ = wake.send(Wake::Seen(seen));
        })
    }

    fn unwatch(&mut self, dir: &Path) {
        if let Some(id) = self.watched.remove(dir) {
            // Paths that name one directory share one inotify watch. The
            // others lose it here too, and have to ask for it again.
            self.watched.retain(|_, other| *other != id);
        }
        if let Some(inotify) = &mut self.inotify {
            // An error says that the watch went away with its directory.
            let _ = inotify.unwatch(dir);
        }
    }

    /// Takes inotify's answer to the watch of one directory. Breaks off
    /// when inotify hit a limit: the sources that hold the directory are
    /// polled from now on, and want no watch any more.
    fn watch_answered(&mut self, point: WatchPoint, answer: notify::Result<()>) -> ControlFlow<()> {
        let error = match answer {
            Ok(()) => {
                self.watched.insert(point.dir, point.id);
                return ControlFlow::Continue(());
            }
            Err(error) => error,
        };
        if is_inotify_limit(&error) {
            let holders = self.sources.iter_mut();
            holders
                .filter(|followed| followed.dirs.contains_key(&point.dir))
                .for_each(|followed| followed.give_up_watching(&error));
            return ControlFlow::Break(());
        }
        // A directory that was removed since the scan: its event is on the
        // way. Or one that cannot be read: the scan warned about it, and the
        // check of the watch tries again.
        tracing::debug!(path = %point.dir.display(), reason = %error, "directory not watched");
        ControlFlow::Continue(())
    }

    /// The sources that want one of `dirs` watched.
    fn sources_holding(&self, dirs: &BTreeSet<PathBuf>) -> Vec<usize> {
        (0..self.sources.len())
            .filter(|&index| {
                dirs.iter()
                    .any(|dir| self.sources[index].dirs.contains_key(dir))
            })
            .collect()
    }

    /// The files of all sources, one stamp per path.
    ///
    /// Two sources can name one path and have been scanned apart, so the
    /// stamps differ. The greater [`FileStamp`] is the one kept: order is
    /// inode, then size, then mtime. The last scan in source order is not,
    /// or an older observation from a source that was not rescanned would
    /// hide the newer one. That is the observation reported for the path.
    fn files_on_disk(&self) -> FileTable {
        let mut by_path = FileTable::new();
        for file in self
            .sources
            .iter()
            .flat_map(|followed| &followed.found.files)
        {
            by_path
                .entry(file.path.clone())
                .and_modify(|stamp| {
                    if file.stamp > *stamp {
                        *stamp = file.stamp;
                    }
                })
                .or_insert(file.stamp);
        }
        by_path
    }

    /// The last scans as one report. One path is listed once, with the
    /// greater stamp from [`Self::files_on_disk`]. The catalog keeps the
    /// greatest stamp of a path in a scan (catalog.rs `record_local`), the
    /// same stamp that `files_on_disk` stores as `reported`.
    fn whole_scan(&self) -> Scan {
        let mut whole = Scan::default();
        for followed in &self.sources {
            whole.dirs.extend_from_slice(&followed.found.dirs);
            whole.warnings.extend_from_slice(&followed.found.warnings);
        }
        whole.files = self
            .files_on_disk()
            .into_iter()
            .map(|(path, stamp)| LocalFile { path, stamp })
            .collect();
        whole
    }

    fn report(&self, news: SourceNews) {
        // An error says that the watch was dropped: nobody reads its news.
        let _ = self.news.send(news);
    }
}

/// The thread behind a [`SlotsWatch`].
struct Lookout {
    slots_dir: PathBuf,
    /// Dropped and made again whenever the watch moves.
    inotify: Option<RecommendedWatcher>,
    /// What `inotify` watches: the slots directory, or the nearest directory
    /// above it while it is missing.
    watched: Option<WatchPoint>,
    /// Whether each slot was there at the last look, or arrived since.
    there: PerStyle<bool>,
    lost: mpsc::UnboundedSender<Style>,
    /// Where a new watcher sends what inotify reports.
    wake: Sender<Wake<notify::Result<notify::Event>>>,
}

impl Lookout {
    /// Looks at the slots after every event that concerns them, until the
    /// watch is dropped.
    fn run(mut self, wakes: &Receiver<Wake<notify::Result<notify::Event>>>) {
        let mut check_at = Instant::now() + WATCH_CHECK_PERIOD;
        loop {
            match next_wake(wakes, Some(check_at)) {
                Wake::Stop => return,
                Wake::Seen(Ok(event)) if !self.concerns(&event) => continue,
                Wake::Seen(Ok(event)) => {
                    self.note_arrivals(&event);
                    self.note_ended_watch(&event);
                }
                Wake::Seen(Err(error)) => {
                    tracing::warn!(reason = %error, "inotify failed on the slots directory");
                }
                Wake::Due => {}
            }
            self.look();
            check_at = Instant::now() + WATCH_CHECK_PERIOD;
        }
    }

    /// Whether an event can mean a change of a slot or of where to watch:
    /// its path is in the slots directory, or it is that directory or one
    /// on the way to it. An overflow of the inotify queue can mean anything.
    fn concerns(&self, event: &notify::Event) -> bool {
        let slots_dir = &self.slots_dir;
        let on_the_way =
            |path: &PathBuf| path.starts_with(slots_dir) || slots_dir.starts_with(path);
        !is_no_change(&event.kind) && (event.need_rescan() || event.paths.iter().any(on_the_way))
    }

    /// Notes each slot that the event says was just put in place: created,
    /// or renamed onto its name. The slot can be gone again before the next
    /// look, and that look has to report it.
    fn note_arrivals(&mut self, event: &notify::Event) {
        let arrival = EventKind::Modify(ModifyKind::Name(RenameMode::To));
        if !event.kind.is_create() && event.kind != arrival {
            return;
        }
        for style in Style::ALL {
            if event.paths.contains(&self.slot(style)) {
                self.there[style] = true;
            }
        }
    }

    /// Forgets the watch when the event says that its directory was
    /// removed or renamed away, so that the next look watches anew. A new
    /// directory there can have the same inode number.
    fn note_ended_watch(&mut self, event: &notify::Event) {
        let is_watched = |path: &PathBuf| {
            self.watched
                .as_ref()
                .is_some_and(|point| point.dir == *path)
        };
        if ends_a_watch(&event.kind) && event.paths.iter().any(is_watched) {
            self.watched = None;
        }
    }

    fn slot(&self, style: Style) -> PathBuf {
        self.slots_dir.join(style.as_str())
    }

    /// Moves the watch to where the slots directory is now, then reports
    /// each slot that was there at the last look and is gone.
    fn look(&mut self) {
        // The directory can appear while the watch above it is set up, and
        // no event tells: look again until the watch is where it belongs.
        loop {
            let wanted =
                WatchPoint::at(&self.slots_dir).or_else(|| WatchPoint::above(&self.slots_dir));
            if wanted == self.watched {
                break;
            }
            self.watch(wanted);
        }
        for style in Style::ALL {
            // A stat error is not "present" and not "gone": only a slot
            // that was seen can later be reported lost.
            match slots::slot_id(&self.slot(style)) {
                Ok(Some(_)) => self.there[style] = true,
                Ok(None) => {
                    if self.there[style] {
                        // An error says that the watch was dropped.
                        let _ = self.lost.send(style);
                    }
                    self.there[style] = false;
                }
                Err(_) => {}
            }
        }
    }

    /// Replaces the watch. One that cannot be set up is not tried again
    /// before the directory changes: the slots are then looked at every
    /// [`WATCH_CHECK_PERIOD`] only.
    fn watch(&mut self, wanted: Option<WatchPoint>) {
        self.inotify = None;
        if let Some(point) = &wanted {
            self.inotify = self
                .new_watcher(&point.dir)
                .inspect_err(|error| {
                    tracing::warn!(
                        path = %point.dir.display(),
                        reason = %error,
                        "slots directory not watched: a lost slot is seen late"
                    );
                })
                .ok();
        }
        self.watched = wanted;
    }

    fn new_watcher(&self, dir: &Path) -> notify::Result<RecommendedWatcher> {
        let wake = self.wake.clone();
        let mut inotify =
            notify::recommended_watcher(move |seen: notify::Result<notify::Event>| {
                // An error says that the watch was dropped.
                let _ = wake.send(Wake::Seen(seen));
            })?;
        inotify.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(inotify)
    }
}

/// Holds a remote scan at its start so a test can prove the follower moved
/// on. A temp directory is not NFS, and a stalled mount is not available here.
#[cfg(test)]
mod remote_scan_hold {
    use std::sync::{Condvar, Mutex};

    use std::sync::atomic::{AtomicBool, Ordering};

    static HELD: AtomicBool = AtomicBool::new(false);
    static LOCK: Mutex<()> = Mutex::new(());
    static RELEASED: Condvar = Condvar::new();

    pub(super) fn wait() {
        let mut guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        while HELD.load(Ordering::SeqCst) {
            guard = RELEASED
                .wait(guard)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    pub(super) fn set(held: bool) {
        let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        HELD.store(held, Ordering::SeqCst);
        if !held {
            RELEASED.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::io;
    use std::os::unix::fs::symlink;

    use gpaperd_core::catalog::{Catalog, Item, ItemId, Version};
    use gpaperd_core::config::LinkPolicy;
    use notify_debouncer_full::notify::event::{AccessKind, CreateKind, Flag, RemoveKind};
    use tempfile::TempDir;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);
    const MINUTE: Duration = Duration::from_secs(60);
    /// Longer than any scheduling delay a loaded test machine adds.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// A temp dir and its canonical path, which is what reported paths start
    /// with even when the temp root sits behind a symlink.
    fn temp_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical temp dir");
        (dir, root)
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("fixture has a parent")).expect("create dirs");
        fs::write(path, content).expect("write fixture");
    }

    fn flat(path: &Path) -> Source {
        Source::Dir {
            path: path.to_owned(),
            options: DirOptions::default(),
        }
    }

    fn recursive(path: &Path) -> Source {
        Source::Dir {
            path: path.to_owned(),
            options: DirOptions {
                recursive: true,
                ..DirOptions::default()
            },
        }
    }

    fn unfollowed(path: &Path) -> Source {
        Source::Dir {
            path: path.to_owned(),
            options: DirOptions {
                follow_symlinks: false,
                ..DirOptions::default()
            },
        }
    }

    /// A directory source with a `poll` option of one minute.
    fn polled(path: &Path) -> Source {
        let options = DirOptions {
            poll: Some(MINUTE),
            ..DirOptions::default()
        };
        Source::Dir {
            path: path.to_owned(),
            options,
        }
    }

    /// Changes in words, with paths relative to `root`: `found walls/a.png`,
    /// `removed walls/b.png`.
    fn told(root: &Path, changes: &[FsChange]) -> Vec<String> {
        let relative = |path: &Path| {
            let relative = path.strip_prefix(root).expect("path is under the root");
            relative.display().to_string()
        };
        changes
            .iter()
            .map(|change| match change {
                FsChange::Found(file) => format!("found {}", relative(&file.path)),
                FsChange::Removed(path) => format!("removed {}", relative(path)),
            })
            .collect()
    }

    /// The file of a report that holds one found file and nothing else.
    fn only_found(changes: &[FsChange]) -> &LocalFile {
        match changes {
            [FsChange::Found(file)] => file,
            other => panic!("expected one found file, got {other:?}"),
        }
    }

    /// A follower that the test drives by hand. It takes the time from the
    /// test, and it hears of inotify only what the test hands over.
    struct Driven {
        follower: Follower,
        news: mpsc::UnboundedReceiver<SourceNews>,
        scan: Scan,
        began: Instant,
        root: PathBuf,
        /// Where the debouncer's reports wait until the test hands them over.
        wakes: Receiver<Wake<DebounceEventResult>>,
    }

    impl Driven {
        fn start(root: &Path, sources: &[Source]) -> Driven {
            Driven::start_prepared(root, sources, |_| {})
        }

        /// [`Driven::start`], with `prepare` run on the follower before its
        /// first look.
        fn start_prepared(
            root: &Path,
            sources: &[Source],
            prepare: impl FnOnce(&mut Follower),
        ) -> Driven {
            let (news_writer, mut news) = mpsc::unbounded_channel();
            let (wake, wakes) = channel();
            let began = Instant::now();
            let stop = Arc::new(AtomicBool::new(false));
            let mut follower = Follower::new(sources, news_writer, wake, began, stop);
            prepare(&mut follower);
            follower.report_first_scan(began);
            let scan = match news.try_recv() {
                Ok(SourceNews::Scanned(scan)) => scan,
                other => panic!("expected the scan first, got {other:?}"),
            };
            Driven {
                follower,
                news,
                scan,
                began,
                root: root.to_owned(),
                wakes,
            }
        }

        /// Looks at the sources as if `elapsed` had passed since the start,
        /// and returns what that look reported.
        fn changes_after(&mut self, elapsed: Duration) -> Vec<FsChange> {
            self.follower.look(self.began + elapsed);
            match self.news.try_recv() {
                Ok(SourceNews::Changed(changes)) => changes,
                Ok(SourceNews::Scanned(scan)) => panic!("a second scan: {scan:?}"),
                Err(_) => Vec::new(),
            }
        }

        /// [`Driven::changes_after`] in words.
        fn told_after(&mut self, elapsed: Duration) -> Vec<String> {
            let changes = self.changes_after(elapsed);
            told(&self.root, &changes)
        }

        /// Hands over an inotify event, as the debouncer would `elapsed`
        /// after the start.
        fn see(&mut self, event: notify::Event, elapsed: Duration) {
            let now = self.began + elapsed;
            let seen = vec![DebouncedEvent::new(event, now)];
            self.follower.note(Ok(seen), now);
        }

        /// Hands over what the debouncer really reports, until a look
        /// reports something. Each look is as if `elapsed` and the gap that
        /// ends a burst had passed since the start.
        fn told_by_inotify(&mut self, elapsed: Duration) -> Vec<String> {
            loop {
                let wake = self.wakes.recv_timeout(PATIENCE);
                if let Wake::Seen(seen) = wake.expect("a report of inotify in time") {
                    self.follower.note(seen, self.began + elapsed);
                }
                let told = self.told_after(elapsed + BURST_GAP);
                if !told.is_empty() {
                    return told;
                }
            }
        }

        fn see_created(&mut self, path: &Path, elapsed: Duration) {
            let created = EventKind::Create(CreateKind::Any);
            self.see(
                notify::Event::new(created).add_path(path.to_owned()),
                elapsed,
            );
        }

        fn following(&self) -> &Following {
            &self.follower.sources[0].following
        }
    }

    #[test]
    fn poll_reports_a_new_file_when_its_period_is_over_and_not_before() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[polled(&walls)]);

        write(&walls.join("b.png"), "b");

        assert!(driven.told_after(MINUTE - SECOND).is_empty());
        assert_eq!(driven.told_after(MINUTE), ["found walls/b.png"]);
    }

    #[test]
    fn poll_reports_a_deleted_file() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        write(&walls.join("b.png"), "b");
        let mut driven = Driven::start(&root, &[polled(&walls)]);

        fs::remove_file(walls.join("a.png")).expect("delete fixture");

        assert_eq!(driven.told_after(MINUTE), ["removed walls/a.png"]);
    }

    #[test]
    fn poll_reports_a_file_edited_in_place_with_a_new_stamp_on_its_inode() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[polled(&walls)]);
        let before = driven.scan.files[0].clone();

        fs::write(walls.join("a.png"), "edited in place").expect("edit fixture");

        let changes = driven.changes_after(MINUTE);
        let after = only_found(&changes);
        assert_eq!(after.path, before.path);
        assert_eq!(after.stamp.id, before.stamp.id);
        assert_eq!(after.stamp.size, 15);
    }

    #[test]
    fn poll_reports_an_atomic_save_as_one_found_file_with_a_new_inode() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[polled(&walls)]);
        let before = driven.scan.files[0].clone();

        write(&walls.join("a.png.tmp"), "saved by rename");
        fs::rename(walls.join("a.png.tmp"), walls.join("a.png")).expect("rename fixture");

        let changes = driven.changes_after(MINUTE);
        let after = only_found(&changes);
        assert_eq!(after.path, before.path);
        assert_ne!(after.stamp.id, before.stamp.id);
    }

    #[test]
    fn poll_reports_a_renamed_file_as_removed_and_then_found() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        // The new name sorts first: the order is not the order of the paths.
        write(&walls.join("z.png"), "z");
        let mut driven = Driven::start(&root, &[polled(&walls)]);

        fs::rename(walls.join("z.png"), walls.join("a.png")).expect("rename fixture");

        assert_eq!(
            driven.told_after(MINUTE),
            ["removed walls/z.png", "found walls/a.png"]
        );
    }

    #[test]
    fn poll_reports_a_removed_directory_and_its_return() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[polled(&walls)]);

        fs::remove_dir_all(&walls).expect("remove the source");
        let dropped = driven.told_after(MINUTE);
        write(&walls.join("b.png"), "b");
        let back = driven.told_after(2 * MINUTE);

        assert_eq!(dropped, ["removed walls/a.png"]);
        assert_eq!(back, ["found walls/b.png"]);
    }

    #[test]
    fn polls_repeat_on_the_period_and_report_nothing_when_nothing_changed() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[polled(&walls)]);

        let unchanged = driven.told_after(MINUTE);
        write(&walls.join("b.png"), "b");
        let too_early = driven.told_after(2 * MINUTE - SECOND);
        let second_poll = driven.told_after(2 * MINUTE);

        assert!(unchanged.is_empty(), "{unchanged:?}");
        assert!(too_early.is_empty(), "{too_early:?}");
        assert_eq!(second_poll, ["found walls/b.png"]);
    }

    #[test]
    fn polled_source_holds_no_inotify_watch() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");

        let driven = Driven::start(&root, &[polled(&walls)]);

        assert_eq!(*driven.following(), Following::Polled(MINUTE));
        assert!(driven.follower.watched.is_empty());
        assert!(driven.follower.inotify.is_none());
    }

    #[test]
    fn watched_source_is_looked_at_once_its_burst_is_over() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);

        write(&walls.join("b.png"), "b");
        driven.see_created(&walls.join("b.png"), SECOND);

        let just_before = SECOND + BURST_GAP - Duration::from_millis(1);
        assert!(driven.told_after(just_before).is_empty());
        assert_eq!(driven.told_after(SECOND + BURST_GAP), ["found walls/b.png"]);
    }

    #[test]
    fn burst_that_never_pauses_is_looked_at_after_the_longest_wait() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        write(&walls.join("b.png"), "b");

        let step = Duration::from_millis(100);
        let mut elapsed = SECOND;
        while elapsed < SECOND + MAX_BURST {
            driven.see_created(&walls.join("b.png"), elapsed);
            let told = driven.told_after(elapsed);
            assert!(told.is_empty(), "{told:?} after {elapsed:?}");
            elapsed += step;
        }
        driven.see_created(&walls.join("b.png"), elapsed);

        assert_eq!(driven.told_after(elapsed), ["found walls/b.png"]);
    }

    #[test]
    fn event_that_does_not_concern_a_source_does_not_wake_it() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        write(&walls.join("b.png"), "b");

        let opened = EventKind::Access(AccessKind::Any);
        driven.see(
            notify::Event::new(opened).add_path(walls.join("b.png")),
            SECOND,
        );
        driven.see_created(&root.join("elsewhere/c.png"), SECOND);
        let unconcerned = driven.told_after(SECOND + BURST_GAP);
        driven.see_created(&walls.join("b.png"), 2 * SECOND);
        let concerned = driven.told_after(2 * SECOND + BURST_GAP);

        assert!(unconcerned.is_empty(), "{unconcerned:?}");
        assert_eq!(concerned, ["found walls/b.png"]);
    }

    #[test]
    fn event_on_the_way_to_a_missing_source_moves_the_watch_to_it() {
        let (_dir, root) = temp_root();
        let walls = root.join("pictures/walls");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        let watched_at_first: Vec<PathBuf> = driven.follower.watched.keys().cloned().collect();

        write(&walls.join("a.png"), "a");
        driven.see_created(&root.join("pictures"), SECOND);
        let appeared = driven.told_after(SECOND + BURST_GAP);

        assert_eq!(appeared, ["found pictures/walls/a.png"]);
        assert_eq!(watched_at_first, [root]);
        let watched: Vec<&PathBuf> = driven.follower.watched.keys().collect();
        assert_eq!(watched, [&walls]);
    }

    #[test]
    fn inotify_failure_makes_every_watched_source_look_again() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);

        write(&walls.join("b.png"), "b");
        let failed = vec![notify::Error::generic("events were lost")];
        driven.follower.note(Err(failed), driven.began + SECOND);
        let after_failure = driven.told_after(SECOND + BURST_GAP);
        write(&walls.join("c.png"), "c");
        let overflow = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        driven.see(overflow, 2 * SECOND);
        let after_overflow = driven.told_after(2 * SECOND + BURST_GAP);

        assert_eq!(after_failure, ["found walls/b.png"]);
        assert_eq!(after_overflow, ["found walls/c.png"]);
    }

    #[test]
    fn source_that_runs_out_of_watches_is_polled_and_gives_its_watch_back() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        let refused = WatchPoint::at(&walls).expect("the source directory");

        let no_watch_left = notify::Error::new(notify::ErrorKind::MaxFilesWatch);
        let flow = driven.follower.watch_answered(refused, Err(no_watch_left));
        driven.follower.sync_watches();
        write(&walls.join("b.png"), "b");
        let first_poll = driven.told_after(DEFAULT_POLL_PERIOD);
        write(&walls.join("c.png"), "c");
        let too_early = driven.told_after(2 * DEFAULT_POLL_PERIOD - SECOND);
        let second_poll = driven.told_after(2 * DEFAULT_POLL_PERIOD);

        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(*driven.following(), Following::Polled(DEFAULT_POLL_PERIOD));
        assert!(driven.follower.watched.is_empty());
        assert!(driven.follower.inotify.is_none());
        assert_eq!(first_poll, ["found walls/b.png"]);
        assert!(too_early.is_empty(), "{too_early:?}");
        assert_eq!(second_poll, ["found walls/c.png"]);
    }

    #[test]
    fn missing_watch_and_missing_instance_are_limits_and_other_refusals_are_not() {
        let from_errno = |errno: Errno| notify::Error::io(io::Error::from(errno));

        assert!(is_inotify_limit(&notify::Error::new(
            notify::ErrorKind::MaxFilesWatch
        )));
        assert!(is_inotify_limit(&from_errno(Errno::MFILE)));
        assert!(is_inotify_limit(&from_errno(Errno::NFILE)));
        assert!(!is_inotify_limit(&from_errno(Errno::ACCESS)));
        assert!(!is_inotify_limit(&notify::Error::path_not_found()));
    }

    #[test]
    fn watch_that_is_refused_for_another_reason_is_tried_again_at_the_check() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        let refused = WatchPoint::at(&walls).expect("the source directory");

        driven.follower.unwatch(&walls);
        let denied = notify::Error::io(io::ErrorKind::PermissionDenied.into());
        let flow = driven.follower.watch_answered(refused.clone(), Err(denied));
        let unwatched = driven.follower.watched.is_empty();
        write(&walls.join("b.png"), "b");
        let too_early = driven.told_after(WATCH_CHECK_PERIOD - SECOND);
        let at_the_check = driven.told_after(WATCH_CHECK_PERIOD);

        assert_eq!(flow, ControlFlow::Continue(()));
        assert!(unwatched);
        assert_eq!(*driven.following(), Following::Watched(refused.clone()));
        assert!(too_early.is_empty(), "{too_early:?}");
        assert_eq!(at_the_check, ["found walls/b.png"]);
        assert_eq!(driven.follower.watched.get(&walls), Some(&refused.id));
    }

    #[test]
    fn check_moves_the_watch_to_a_directory_that_replaced_the_source_unseen() {
        let (_dir, root) = temp_root();
        let walls = root.join("pictures/walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);

        // What a mount or a renamed parent directory does: no event reaches
        // the watch of `walls`.
        fs::rename(root.join("pictures"), root.join("old")).expect("rename the parent");
        write(&walls.join("b.png"), "b");
        let too_early = driven.told_after(WATCH_CHECK_PERIOD - SECOND);
        let at_the_check = driven.told_after(WATCH_CHECK_PERIOD);

        assert!(too_early.is_empty(), "{too_early:?}");
        assert_eq!(
            at_the_check,
            ["removed pictures/walls/a.png", "found pictures/walls/b.png"]
        );
        let replacement = WatchPoint::at(&walls).expect("the new source directory");
        assert_eq!(driven.follower.watched.get(&walls), Some(&replacement.id));
    }

    #[test]
    fn check_sees_a_source_that_fell_back_on_a_directory_of_another_source() {
        let (_dir, root) = temp_root();
        let (outer, inner) = (root.join("outer"), root.join("outer/inner"));
        write(&inner.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&outer), flat(&inner)]);

        // The watch that the inner source needs now is there already.
        fs::remove_dir_all(&inner).expect("remove the inner source");

        assert_eq!(
            driven.told_after(WATCH_CHECK_PERIOD),
            ["removed outer/inner/a.png"]
        );
    }

    #[test]
    fn watch_that_two_names_share_survives_when_one_name_goes_away() {
        let (_dir, root) = temp_root();
        let (walls, alias) = (root.join("walls"), root.join("alias"));
        write(&walls.join("a.png"), "a");
        symlink(&walls, &alias).expect("symlink");
        let mut driven = Driven::start(&root, &[flat(&walls), flat(&alias)]);

        // The source behind the alias moves its watch up to the root.
        fs::remove_file(&alias).expect("remove the symlink");
        let at_the_check = driven.told_after(WATCH_CHECK_PERIOD);
        write(&walls.join("b.png"), "b");
        let appeared = driven.told_by_inotify(WATCH_CHECK_PERIOD + SECOND);

        assert!(at_the_check.is_empty(), "{at_the_check:?}");
        assert_eq!(appeared, ["found walls/b.png"]);
    }

    #[test]
    fn source_directory_that_was_removed_and_made_again_is_watched_anew() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);

        // What the removal does to the watch, also when the new directory
        // gets the old inode number, as it can on ext4: inotify and notify
        // drop it. The directory is back before the look.
        let inotify = driven.follower.inotify.as_mut().expect("the source watch");
        inotify.unwatch(&walls).expect("drop the watch");
        let removed = EventKind::Remove(RemoveKind::Folder);
        driven.see(notify::Event::new(removed).add_path(walls.clone()), SECOND);
        let at_the_look = driven.told_after(SECOND + BURST_GAP);
        write(&walls.join("b.png"), "b");
        let appeared = driven.told_by_inotify(2 * SECOND);

        assert!(at_the_look.is_empty(), "{at_the_look:?}");
        assert_eq!(appeared, ["found walls/b.png"]);
    }

    #[test]
    fn network_and_fuse_file_systems_are_remote() {
        const EXT4: u32 = 0xEF53;
        const BTRFS: u32 = 0x9123_683E;
        const TMPFS: u32 = 0x0102_1994;

        for remote in [0x6969, 0xFF53_4D42, 0xFE53_4D42, 0x6573_5546] {
            assert!(is_remote_fs_type(remote), "{remote:#x}");
        }
        for local in [EXT4, BTRFS, TMPFS] {
            assert!(!is_remote_fs_type(local), "{local:#x}");
        }
    }

    #[test]
    fn temp_directory_is_watched_and_not_polled() {
        let (_dir, root) = temp_root();
        fs::create_dir(root.join("walls")).expect("create dir");

        let driven = Driven::start(&root, &[flat(&root.join("walls"))]);

        let own_dir = WatchPoint::at(&root.join("walls")).expect("the source directory");
        assert_eq!(*driven.following(), Following::Watched(own_dir));
    }

    #[test]
    fn watch_point_of_a_missing_path_is_the_nearest_directory_above_it() {
        let (_dir, root) = temp_root();
        write(&root.join("pictures/file.png"), "no directory");

        let above_missing = WatchPoint::above(&root.join("pictures/walls/light"));
        let above_file = WatchPoint::above(&root.join("pictures/file.png/walls"));

        let pictures = WatchPoint::at(&root.join("pictures"));
        assert!(pictures.is_some());
        assert_eq!(above_missing, pictures);
        assert_eq!(above_file, pictures);
        assert_eq!(WatchPoint::at(&root.join("pictures/file.png")), None);
    }

    /// Waits until a thread of a watch has ended.
    fn ended_in_time(worker: &thread::JoinHandle<()>) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        worker.is_finished()
    }

    #[test]
    fn thread_of_a_source_watch_ends_when_the_watch_is_dropped() {
        let (_dir, root) = temp_root();
        write(&root.join("walls/a.png"), "a");
        let sources = [flat(&root.join("walls"))];
        let (news, mut news_reader) = mpsc::unbounded_channel();
        let (wake, wakes) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = StopOnDrop {
            wake: wake.clone(),
            stop: Arc::clone(&stop),
        };
        let worker = thread::spawn(move || {
            Follower::start(&sources, news, wake, Instant::now(), stop).run(&wakes);
        });

        drop(handle);

        assert!(ended_in_time(&worker));
        let scanned = news_reader.try_recv();
        assert!(matches!(scanned, Ok(SourceNews::Scanned(_))), "{scanned:?}");
    }

    /// A lookout that the test drives by hand, and what it reported.
    fn lookout(slots_dir: &Path) -> (Lookout, mpsc::UnboundedReceiver<Style>) {
        let (lost, lost_reader) = mpsc::unbounded_channel();
        let (wake, _wakes) = channel();
        let lookout = Lookout {
            slots_dir: slots_dir.to_owned(),
            inotify: None,
            watched: None,
            there: PerStyle::default(),
            lost,
            wake,
        };
        (lookout, lost_reader)
    }

    fn publish(root: &Path, slot: &Path) {
        let image = root.join("image.png");
        write(&image, "image");
        slots::publish(&image, slot, LinkPolicy::AlwaysCopy).expect("publish");
    }

    #[test]
    fn thread_of_a_slots_watch_ends_when_the_watch_is_dropped() {
        let (_dir, root) = temp_root();
        let (lookout, _lost) = lookout(&root.join("slots"));
        let (wake, wakes) = channel();
        let handle = StopOnDrop {
            wake,
            stop: Arc::new(AtomicBool::new(false)),
        };
        let worker = thread::spawn(move || lookout.run(&wakes));

        drop(handle);

        assert!(ended_in_time(&worker));
    }

    #[test]
    fn look_finds_the_slots_gone_with_a_directory_above_them_and_watches_higher_up() {
        let (_dir, root) = temp_root();
        let slots_dir = root.join("gpaperd/slots");
        publish(&root, &slots_dir.join("light"));
        publish(&root, &slots_dir.join("dark"));
        let (mut lookout, mut lost) = lookout(&slots_dir);
        lookout.look();
        let watched_at_first = lookout.watched.clone();

        // No event reaches the watch of the slots directory.
        fs::rename(root.join("gpaperd"), root.join("moved")).expect("rename the parent");
        lookout.look();

        assert_eq!(watched_at_first.map(|point| point.dir), Some(slots_dir));
        assert_eq!(lost.try_recv(), Ok(Style::Light));
        assert_eq!(lost.try_recv(), Ok(Style::Dark));
        assert_eq!(lookout.watched.map(|point| point.dir), Some(root));
    }

    #[test]
    fn slots_directory_that_was_removed_is_watched_anew_at_the_next_look() {
        let (_dir, root) = temp_root();
        let slots_dir = root.join("slots");
        fs::create_dir(&slots_dir).expect("create dir");
        let (mut lookout, _lost) = lookout(&slots_dir);
        lookout.look();
        let removed = EventKind::Remove(RemoveKind::Folder);

        lookout.note_ended_watch(&notify::Event::new(removed).add_path(slots_dir.clone()));
        let forgotten = lookout.watched.is_none();
        lookout.look();

        assert!(forgotten);
        assert_eq!(lookout.watched.map(|point| point.dir), Some(slots_dir));
        assert!(lookout.inotify.is_some());
    }

    #[test]
    fn slot_that_came_and_went_between_two_looks_is_lost() {
        let (_dir, root) = temp_root();
        let slots_dir = root.join("slots");
        fs::create_dir(&slots_dir).expect("create dir");
        let (mut lookout, mut lost) = lookout(&slots_dir);
        lookout.look();
        let renamed_onto = EventKind::Modify(ModifyKind::Name(RenameMode::To));
        let removed = EventKind::Remove(RemoveKind::File);

        lookout.note_arrivals(&notify::Event::new(renamed_onto).add_path(slots_dir.join("light")));
        lookout.note_arrivals(&notify::Event::new(removed).add_path(slots_dir.join("dark")));
        lookout.look();

        assert_eq!(lost.try_recv(), Ok(Style::Light));
        assert!(lost.try_recv().is_err());
    }

    /// Replaces a directory with a file, so a stat of a child is `ENOTDIR`.
    /// `chmod 000` does not stop root; this does.
    fn block_with_file(path: &Path) {
        if path.is_dir() {
            fs::remove_dir_all(path).expect("remove directory");
        } else if path.exists() {
            fs::remove_file(path).expect("remove file");
        }
        write(path, "not a directory");
    }

    #[test]
    fn slot_stat_error_is_not_stored_as_present() {
        let (_dir, root) = temp_root();
        let slots_path = root.join("slots");
        block_with_file(&slots_path);
        let blocked = slots::slot_id(&slots_path.join("light")).expect_err("not a directory");
        let (mut lookout, mut lost) = lookout(&slots_path);

        lookout.look();
        let unseen = !lookout.there[Style::Light] && !lookout.there[Style::Dark];
        let no_loss = lost.try_recv().is_err();
        fs::remove_file(&slots_path).expect("remove the blocker");
        fs::create_dir(&slots_path).expect("create the slots directory");
        lookout.look();

        assert_eq!(blocked.kind(), io::ErrorKind::NotADirectory);
        assert!(unseen);
        assert!(no_loss);
        assert!(lost.try_recv().is_err());
        assert!(!lookout.there[Style::Light]);
        assert!(!lookout.there[Style::Dark]);
    }

    #[test]
    fn seen_slot_stays_present_when_a_later_stat_fails() {
        let (_dir, root) = temp_root();
        let slots_dir = root.join("slots");
        publish(&root, &slots_dir.join("light"));
        let (mut lookout, mut lost) = lookout(&slots_dir);
        lookout.look();

        block_with_file(&slots_dir);
        lookout.look();

        assert!(lookout.there[Style::Light]);
        assert!(lost.try_recv().is_err());
    }

    #[test]
    fn refused_nested_watch_is_tried_again_at_the_check() {
        let (_dir, root) = temp_root();
        let nested = root.join("walls/nested");
        write(&nested.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[recursive(&root.join("walls"))]);
        let nested_point = WatchPoint::at(&nested).expect("the nested directory");

        driven.follower.unwatch(&nested);
        let denied = notify::Error::io(io::ErrorKind::PermissionDenied.into());
        let flow = driven
            .follower
            .watch_answered(nested_point.clone(), Err(denied));
        write(&nested.join("b.png"), "b");
        let too_early = driven.told_after(WATCH_CHECK_PERIOD - SECOND);
        let at_the_check = driven.told_after(WATCH_CHECK_PERIOD);

        assert_eq!(flow, ControlFlow::Continue(()));
        assert!(too_early.is_empty(), "{too_early:?}");
        assert_eq!(at_the_check, ["found walls/nested/b.png"]);
        assert_eq!(driven.follower.watched.get(&nested), Some(&nested_point.id));
    }

    #[test]
    fn followed_file_symlink_watches_the_targets_directory() {
        let (_dir, root) = temp_root();
        let (walls, store) = (root.join("walls"), root.join("store"));
        write(&store.join("real.png"), "real");
        fs::create_dir(&walls).expect("create the link directory");
        symlink(store.join("real.png"), walls.join("link.png")).expect("symlink");
        let mut driven = Driven::start(&root, &[flat(&walls)]);

        fs::write(store.join("real.png"), "edited").expect("edit the target");
        driven.see_created(&store.join("real.png"), SECOND);
        let told = driven.told_after(SECOND + BURST_GAP);

        assert!(driven.follower.sources[0].dirs.contains_key(&store));
        assert!(driven.follower.watched.contains_key(&store));
        assert_eq!(told, ["found store/real.png"]);
    }

    #[test]
    fn file_source_symlink_watches_the_targets_directory() {
        let (_dir, root) = temp_root();
        let (walls, store) = (root.join("walls"), root.join("store"));
        write(&store.join("real.png"), "real");
        fs::create_dir(&walls).expect("create the link directory");
        symlink(store.join("real.png"), walls.join("link.png")).expect("symlink");
        let mut driven = Driven::start(&root, &[Source::File(walls.join("link.png"))]);

        fs::write(store.join("real.png"), "edited").expect("edit the target");
        driven.see_created(&store.join("real.png"), SECOND);
        let told = driven.told_after(SECOND + BURST_GAP);

        assert!(driven.follower.sources[0].dirs.contains_key(&store));
        assert!(driven.follower.watched.contains_key(&store));
        assert_eq!(told, ["found store/real.png"]);
    }

    #[test]
    fn file_symlink_target_is_not_watched_when_following_is_off() {
        let (_dir, root) = temp_root();
        let (walls, store) = (root.join("walls"), root.join("store"));
        write(&store.join("real.png"), "real");
        write(&walls.join("own.png"), "own");
        symlink(store.join("real.png"), walls.join("link.png")).expect("symlink");

        let driven = Driven::start(&root, &[unfollowed(&walls)]);

        assert!(!driven.follower.sources[0].dirs.contains_key(&store));
        assert!(!driven.follower.watched.contains_key(&store));
        assert!(
            driven
                .scan
                .files
                .iter()
                .all(|file| !file.path.ends_with("real.png"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_source_channel_is_replaced_with_a_scan() {
        let (_dir, root) = temp_root();
        write(&root.join("walls/a.png"), "a");
        let mut watch = SourceWatch {
            sources: vec![flat(&root.join("walls"))],
            news: mpsc::unbounded_channel().1,
            _worker: StopOnDrop::detached(),
            restarted_at: None,
        };

        let news = tokio::time::timeout(PATIENCE, watch.next())
            .await
            .expect("the replacement scan arrives");

        assert!(matches!(news, SourceNews::Scanned(_)), "{news:?}");
        drop(watch);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dead_source_worker_is_not_started_again_before_the_watch_check() {
        let (_dir, root) = temp_root();
        write(&root.join("walls/a.png"), "a");
        let mut watch = SourceWatch {
            sources: vec![flat(&root.join("walls"))],
            news: mpsc::unbounded_channel().1,
            _worker: StopOnDrop::detached(),
            restarted_at: Some(tokio::time::Instant::now()),
        };

        tokio::select! {
            biased;
            news = watch.next() => panic!("restarted at once: {news:?}"),
            _ = future::ready(()) => {}
        }

        assert!(watch.news.is_closed());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_slots_channel_is_replaced_with_an_open_one() {
        let (_dir, root) = temp_root();
        let mut watch = SlotsWatch {
            slots_dir: root.join("slots"),
            lost: mpsc::unbounded_channel().1,
            _worker: StopOnDrop::detached(),
            restarted_at: None,
        };

        assert!(watch.lost.is_closed());
        // `lost` must replace the channel. Polling `ready` first would
        // observe the closed one and let a pending wait pass.
        tokio::select! {
            biased;
            style = watch.lost() => panic!("closed watch reported {style:?}"),
            _ = future::ready(()) => {}
        }
        assert!(!watch.lost.is_closed());
        drop(watch);
    }

    #[test]
    fn stop_returns_from_a_rescan_before_the_stale_source_is_scanned() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&walls)]);
        write(&walls.join("b.png"), "b");
        driven.follower.stop.store(true, Ordering::SeqCst);

        driven.follower.refresh(BTreeSet::from([0]));

        let names: Vec<_> = driven.follower.sources[0]
            .found
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        assert!(
            names.iter().all(|path| !path.ends_with("b.png")),
            "{names:?}"
        );
    }

    #[test]
    fn overlapping_sources_keep_the_greater_stamp() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        write(&walls.join("a.png"), "aa");
        let mut driven = Driven::start(&root, &[flat(&walls), flat(&walls)]);
        let path = driven.follower.sources[0].found.files[0].path.clone();
        let greater = driven.follower.sources[0].found.files[0].stamp;
        let lesser = FileStamp {
            size: greater.size - 1,
            ..greater
        };
        driven.follower.sources[1].found.files[0].stamp = lesser;

        let scan = driven.follower.whole_scan();
        let listed: Vec<_> = scan
            .files
            .iter()
            .filter(|file| file.path == path)
            .map(|file| file.stamp)
            .collect();
        let catalog = Catalog::from_local_files(scan.files);
        let kept = catalog.get(&ItemId::from_path(&path)).map(Item::version);
        let on_disk = driven.follower.files_on_disk();
        driven.follower.reported.insert(path.clone(), lesser);
        let changes = changes_between(&driven.follower.reported, &on_disk);

        assert_eq!(listed, vec![greater]);
        assert_eq!(kept, Some(Version::File(greater)));
        assert_eq!(on_disk.get(&path), Some(&greater));
        match changes.as_slice() {
            [FsChange::Found(file)] => assert_eq!(file.stamp, greater),
            other => panic!("expected the greater stamp, got {other:?}"),
        }
    }

    /// Clears the hold even when the test panics, so later tests are not stuck.
    /// One test at a time holds the remote scans: the hold is global, and
    /// one test's release would free another's scan.
    static HOLDER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ReleaseHold {
        _holder: std::sync::MutexGuard<'static, ()>,
    }

    /// Holds every remote scan at its start until the guard drops.
    fn hold_remote_scans() -> ReleaseHold {
        let holder = HOLDER.lock().unwrap_or_else(|poison| poison.into_inner());
        super::remote_scan_hold::set(true);
        ReleaseHold { _holder: holder }
    }

    impl Drop for ReleaseHold {
        fn drop(&mut self) {
            super::remote_scan_hold::set(false);
        }
    }

    #[test]
    fn remote_scan_does_not_block_the_other_source() {
        let (_dir, root) = temp_root();
        let (remote_dir, local_dir) = (root.join("remote"), root.join("local"));
        write(&remote_dir.join("a.png"), "a");
        write(&local_dir.join("a.png"), "a");
        let mut driven = Driven::start(&root, &[flat(&remote_dir), flat(&local_dir)]);
        let release = hold_remote_scans();
        driven.follower.sources[0].force_off_thread = true;
        driven.follower.sources[0].due = driven.began;
        driven.follower.sources[1].due = driven.began;
        driven.follower.sources[1].burst_began = Some(driven.began);
        write(&remote_dir.join("b.png"), "b");
        write(&local_dir.join("b.png"), "b");

        let told = driven.told_after(Duration::ZERO);

        assert!(
            told.iter().any(|line| line == "found local/b.png"),
            "{told:?}"
        );
        assert!(
            told.iter().all(|line| !line.contains("remote/b.png")),
            "{told:?}"
        );
        assert!(driven.follower.sources[0].scan_is_out());
        drop(release);
        let deadline = Instant::now() + PATIENCE;
        while driven.follower.sources[0].scan_is_out() && Instant::now() < deadline {
            driven.follower.take_remote_scans(driven.began + MINUTE);
            if driven.follower.sources[0].scan_is_out() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(!driven.follower.sources[0].scan_is_out());
    }

    #[test]
    fn first_scan_holds_the_files_of_a_remote_source() {
        let (_dir, root) = temp_root();
        let remote_dir = root.join("remote");
        write(&remote_dir.join("a.png"), "a");

        let driven = Driven::start_prepared(&root, &[flat(&remote_dir)], |follower| {
            follower.sources[0].force_off_thread = true;
        });

        let files: Vec<_> = driven.scan.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, [&remote_dir.join("a.png")]);
        assert_eq!(driven.scan.dirs, [remote_dir]);
        assert!(!driven.follower.sources[0].scan_is_out());
    }

    #[test]
    fn first_scan_stops_waiting_for_a_stuck_remote_scan_at_its_deadline() {
        let (_dir, root) = temp_root();
        let (remote_dir, local_dir) = (root.join("remote"), root.join("local"));
        write(&remote_dir.join("a.png"), "a");
        write(&local_dir.join("a.png"), "a");
        let _release = hold_remote_scans();
        let (news, _news) = mpsc::unbounded_channel();
        let (wake, _wakes) = channel();
        let sources = [flat(&remote_dir), flat(&local_dir)];
        let stop = Arc::new(AtomicBool::new(false));
        let mut follower = Follower::new(&sources, news, wake, Instant::now(), stop);
        follower.sources[0].force_off_thread = true;
        follower.refresh(BTreeSet::from([0, 1]));

        follower.wait_for_remote_scans(Instant::now() + Duration::from_millis(50));

        assert!(follower.sources[0].scan_is_out());
        let local: Vec<_> = follower.sources[1]
            .found
            .files
            .iter()
            .map(|file| &file.path)
            .collect();
        assert_eq!(local, [&local_dir.join("a.png")]);
    }
}
