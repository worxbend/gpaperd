//! The config file: loading it, and watching it for hot reload.
//!
//! Rules are the plan's "Load and reload rules". Editors and dotfile tools
//! save by rename, and the real file is often behind a symlink. So the watch
//! is on directories, not on the file, and every load follows the symlink
//! again.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::future;
use std::io;
use std::panic;
use std::path::{self, Path, PathBuf};
use std::thread;
use std::time::Duration;

use gpaperd_core::config::{Config, ConfigError, ParseContext};
use notify::event::AccessKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::sleep;

/// How long the config file must be quiet before a change is loaded. One save
/// by an editor is a burst of events.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// How many names one config path is watched by at most. It ends the walk
/// through symlinks that lead in a circle: the kernel follows 40 symlinks on
/// one path, and the last name is not one.
const MAX_WATCHED_NAMES: usize = 41;

/// Where the config comes from: the file, and what its source paths expand
/// with.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// As given: it may be a symlink, and a `--config` override may be
    /// relative.
    pub path: PathBuf,
    /// Replaces a leading `~` in a source path.
    pub home_dir: PathBuf,
    /// Environment lookup for `$VAR` and `${VAR}` in a source path. `None`
    /// means undefined.
    pub env: fn(&str) -> Option<OsString>,
}

/// Why the config file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file does not parse or validate. Holds the line and the column.
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    /// The file is there and cannot be read.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl ConfigFile {
    /// Reads and validates the file. `Ok(None)` when there is no such file.
    /// Blocking I/O.
    ///
    /// Every symlink is followed first. Relative source paths resolve against
    /// the directory of the real file, so that a stow or chezmoi symlink
    /// behaves.
    ///
    /// # Errors
    ///
    /// [`LoadError::Io`] when the file cannot be read, and
    /// [`LoadError::Invalid`] when its content is refused.
    pub fn load(&self) -> Result<Option<Config>, LoadError> {
        let Some(real_file) = resolve(&self.path)? else {
            return Ok(None);
        };
        let text = fs::read_to_string(&real_file)?;
        let config_dir = real_file
            .parent()
            .ok_or_else(|| io::Error::other("not a file"))?;
        let context = ParseContext {
            config_dir,
            home_dir: &self.home_dir,
            env: &self.env,
        };
        Ok(Some(Config::parse(&text, &context)?))
    }
}

/// Where the config file really is, every symlink followed. `None` when
/// there is no such file.
fn resolve(file: &Path) -> io::Result<Option<PathBuf>> {
    match fs::canonicalize(file) {
        Ok(real_file) => Ok(Some(real_file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Watches the config file and loads it again after each change.
///
/// The watch is on the directory of the real file and on the directory of
/// every symlink on the way from [`ConfigFile::path`] to it, whether that
/// symlink stands for the file or for a directory: a dotfiles repository
/// changes at the far end, and a dotfile tool points a link elsewhere. A
/// directory that does not exist yet is watched through its nearest existing
/// ancestor. All of them are worked out again after every change, because a
/// save by rename and a re-linked dotfile move the real file.
///
/// Dropping the watch stops it and discards a reload that was not read.
#[derive(Debug)]
pub struct ConfigWatch {
    file: ConfigFile,
    /// `None` when inotify cannot be used at all.
    watcher: Option<RecommendedWatcher>,
    events: mpsc::UnboundedReceiver<notify::Result<Event>>,
    /// The names to watch, as [`names_to_watch`] gave them last. The
    /// directory of each is watched.
    watched_names: Vec<PathBuf>,
    /// A change was seen, and what loading gave after it was not returned
    /// yet. It outlives a cancelled [`ConfigWatch::reloaded`].
    change_seen: bool,
    /// Thread that last walked the config path. The walk has to leave the
    /// runtime thread; tests check this is not that thread.
    #[cfg(test)]
    walked_on: Option<thread::ThreadId>,
}

impl ConfigWatch {
    /// Starts the watch and returns at once. Nothing is loaded yet.
    ///
    /// For the first load the caller calls [`ConfigFile::load`] itself,
    /// after this call: a change between the two is then reported, not lost.
    ///
    /// A watch that cannot be set up is logged with its path, and nothing is
    /// ever reported: the daemon runs on without hot reload.
    pub fn start(file: ConfigFile) -> ConfigWatch {
        let (sender, events) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            // Nobody listens once the watch is dropped.
            let _ = sender.send(event);
        })
        .inspect_err(|error| {
            let path = file.path.display();
            tracing::error!(%path, reason = %error, "config file not watched: no hot reload");
        })
        .ok();
        let mut watch = ConfigWatch {
            file,
            watcher,
            events,
            watched_names: Vec::new(),
            change_seen: false,
            #[cfg(test)]
            walked_on: None,
        };
        watch.watch_names();
        watch
    }

    /// The file this watch follows.
    pub fn file(&self) -> &ConfigFile {
        &self.file
    }

    /// Waits until the file changed, then returns what loading it gave: the
    /// result of [`ConfigFile::load`], which runs off the caller's thread.
    /// The runtime must have timers enabled (`enable_time`).
    ///
    /// A change is any event on the file's name or on the real file's name:
    /// written, created, renamed onto, deleted, or a symlink that points
    /// elsewhere now. Events within [`RELOAD_DEBOUNCE`] of each other are one
    /// change. `Ok(None)` says the file is gone; when it comes back, that is
    /// the next change. A file that appears after a start without one is a
    /// change like any other.
    ///
    /// Waits forever when nothing changes. Cancel-safe: a change that is seen
    /// while nobody waits is returned by the next call.
    pub async fn reloaded(&mut self) -> Result<Option<Config>, LoadError> {
        if !self.change_seen {
            self.next_change().await;
            self.change_seen = true;
            // One event per poll, so shutdown and rotation are reached.
            tokio::task::yield_now().await;
        }
        // Every further change starts the quiet time again. Yielding returns
        // this poll to the interpreter instead of draining a ready burst.
        loop {
            tokio::select! {
                biased;
                () = self.next_change() => {}
                () = sleep(RELOAD_DEBOUNCE) => break,
            }
            tokio::task::yield_now().await;
        }
        // Before the load: a change that comes after it must find the watch.
        // The walk is the blocking work this function already moves off the
        // runtime for the load.
        let path = self.file.path.clone();
        let (names, walked_on) = tokio::task::spawn_blocking(move || {
            let walked_on = thread::current().id();
            (names_to_watch(&path), walked_on)
        })
        .await
        .unwrap_or_else(|failure| panic::resume_unwind(failure.into_panic()));
        #[cfg(test)]
        {
            self.walked_on = Some(walked_on);
        }
        #[cfg(not(test))]
        {
            let _ = walked_on;
        }
        if let Some(names) = self.kept_names(names) {
            self.apply_names(names);
        }
        let file = self.file.clone();
        let loaded = tokio::task::spawn_blocking(move || file.load())
            .await
            // Only a panic ends that task early. A panic is a defect: pass
            // it on.
            .unwrap_or_else(|failure| panic::resume_unwind(failure.into_panic()));
        self.change_seen = false;
        loaded
    }

    /// Waits for the next event that can mean another config. Waits forever
    /// when there is no watcher.
    async fn next_change(&mut self) {
        loop {
            match self.events.recv().await {
                Some(Ok(event)) if self.is_change(&event) => return,
                // An ignored event is still one queue item. Yield so a burst
                // of them does not run ahead of the rest of the interpreter.
                Some(Ok(_)) => tokio::task::yield_now().await,
                Some(Err(error)) => {
                    tracing::warn!(reason = %error, "config watch failed: loading the file again");
                    return;
                }
                None => future::pending().await,
            }
        }
    }

    /// Whether `event` is about a watched name or about a directory on the
    /// way to one.
    fn is_change(&self, event: &Event) -> bool {
        // Opening a file changes nothing, and every load opens the file.
        let is_open = matches!(event.kind, EventKind::Access(AccessKind::Open(_)));
        let leads_to_watched_name =
            |path: &PathBuf| self.watched_names.iter().any(|name| name.starts_with(path));
        // A rescan flag says that events were lost.
        !is_open && (event.need_rescan() || event.paths.iter().any(leads_to_watched_name))
    }

    /// Works out again which names to watch, and watches their directories.
    /// Blocking I/O, a few calls on the config file's path. [`ConfigWatch::start`]
    /// calls this on its own thread. [`ConfigWatch::reloaded`] walks on the
    /// blocking pool and then applies the names here.
    fn watch_names(&mut self) {
        if self.watcher.is_none() {
            return;
        }
        if let Some(names) = self.kept_names(names_to_watch(&self.file.path)) {
            self.apply_names(names);
        }
    }

    /// `None` keeps the names already watched. An absolute-path failure must
    /// not become an empty list: that unwatches everything and then waits.
    fn kept_names(&self, names: io::Result<Vec<PathBuf>>) -> Option<Vec<PathBuf>> {
        match names {
            Ok(names) => Some(names),
            Err(error) => {
                let path = self.file.path.display();
                tracing::error!(
                    %path,
                    reason = %error,
                    "config file not watched: keeping the previous names"
                );
                None
            }
        }
    }

    /// Watches the directories of `names` and forgets the previous ones.
    fn apply_names(&mut self, names: Vec<PathBuf>) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let dirs: BTreeSet<&Path> = names.iter().filter_map(|name| name.parent()).collect();
        if dirs.is_empty() {
            // An empty path, or the root directory.
            let path = self.file.path.display();
            tracing::error!(%path, "config file not watched: its path names no file");
        }
        let earlier_dirs = self.watched_names.iter().filter_map(|name| name.parent());
        for left in earlier_dirs.filter(|dir| !dirs.contains(dir)) {
            // It fails for a directory that was deleted: that one is not
            // watched any more, which is the goal.
            let _ = watcher.unwatch(left);
        }
        for dir in dirs {
            // Also a directory that is watched already: one that was deleted
            // and made again has lost its watch.
            if let Err(error) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                let path = dir.display();
                tracing::warn!(%path, reason = %error, "config directory not watched");
            }
        }
        self.watched_names = names;
    }
}

/// Every name whose change can change the config: each symlink on the way to
/// the file, and the name that the way ends at. That is the file's own name,
/// or the first name on the way that does not exist: it is the one that
/// appears first when the way is made.
///
/// A name is the real path of its directory joined with the name of the
/// entry, so no directory is named twice. Blocking I/O.
fn names_to_watch(file: &Path) -> io::Result<Vec<PathBuf>> {
    let mut names = Vec::new();
    let mut real_dir = PathBuf::new();
    let absolute = path::absolute(file)?;
    let mut ahead = steps_reversed(&absolute);
    while names.len() < MAX_WATCHED_NAMES
        && let Some(step) = ahead.pop()
    {
        if step == "." {
            continue;
        }
        if step == ".." {
            real_dir.pop();
            continue;
        }
        let name = real_dir.join(&step);
        match fs::read_link(&name) {
            Ok(target) => {
                ahead.extend(steps_reversed(&target));
                names.push(name);
            }
            Err(_) if !ahead.is_empty() && name.is_dir() => real_dir = name,
            Err(_) => {
                names.push(name);
                break;
            }
        }
    }
    Ok(names)
}

/// The components of `path`, the last one first: `pop` gives the next step
/// on the way.
fn steps_reversed(path: &Path) -> Vec<OsString> {
    let steps = path.components().rev();
    steps.map(|step| step.as_os_str().to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use std::future::Future;
    use std::task::{Context, Waker};

    use notify::event::CreateKind;

    use super::*;

    /// A temp directory by its real path, so that no symlink leads to it.
    fn real_temp_dir() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let real = fs::canonicalize(dir.path()).expect("real path of the temp dir");
        (dir, real)
    }

    #[test]
    fn file_is_watched_by_its_own_name_whether_it_exists_or_not() {
        let (_dir, root) = real_temp_dir();

        let names = names_to_watch(&root.join("config.toml")).expect("absolute path");

        assert_eq!(names, [root.join("config.toml")]);
    }

    #[test]
    fn empty_path_is_an_error() {
        let error = names_to_watch(Path::new("")).expect_err("empty path");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn way_through_missing_directories_ends_at_the_first_missing_name() {
        let (_dir, root) = real_temp_dir();

        let names = names_to_watch(&root.join("gpaperd/sub/config.toml")).expect("absolute path");

        assert_eq!(names, [root.join("gpaperd")]);
    }

    #[test]
    fn symlink_and_the_file_it_points_at_are_both_names() {
        let (_dir, root) = real_temp_dir();
        fs::create_dir_all(root.join("repo/dots")).expect("create the repo");
        fs::create_dir(root.join("config")).expect("create the config dir");
        symlink(
            "../repo/dots/./config.toml",
            root.join("config/config.toml"),
        )
        .expect("create symlink");

        let names = names_to_watch(&root.join("config/config.toml")).expect("absolute path");

        assert_eq!(
            names,
            [
                root.join("config/config.toml"),
                root.join("repo/dots/config.toml")
            ]
        );
    }

    #[test]
    fn symlinked_directory_on_the_way_is_a_name() {
        let (_dir, root) = real_temp_dir();
        fs::create_dir(root.join("repo")).expect("create the repo");
        symlink(root.join("repo"), root.join("config")).expect("create symlink");

        let names = names_to_watch(&root.join("config/config.toml")).expect("absolute path");

        assert_eq!(names, [root.join("config"), root.join("repo/config.toml")]);
    }

    #[test]
    fn path_with_parent_steps_is_walked_like_the_kernel_walks_it() {
        let (_dir, root) = real_temp_dir();
        fs::create_dir(root.join("repo")).expect("create the repo");
        symlink(root.join("repo"), root.join("link")).expect("create symlink");

        let names = names_to_watch(&root.join("link/../config.toml")).expect("absolute path");

        assert_eq!(names, [root.join("link"), root.join("config.toml")]);
    }

    #[test]
    fn symlinks_in_a_circle_end_the_walk() {
        let (_dir, root) = real_temp_dir();
        symlink(root.join("there"), root.join("here")).expect("create symlink");
        symlink(root.join("here"), root.join("there")).expect("create symlink");

        let names = names_to_watch(&root.join("here")).expect("absolute path");

        assert_eq!(names.len(), MAX_WATCHED_NAMES);
        assert_eq!(names[..2], [root.join("here"), root.join("there")]);
    }

    #[test]
    fn relative_path_is_walked_from_the_working_directory() {
        let working_dir = std::env::current_dir().expect("working directory");
        let real_working_dir = fs::canonicalize(working_dir).expect("real working directory");

        let names = names_to_watch(Path::new("no-such-config.toml")).expect("absolute path");

        assert_eq!(
            names.last(),
            Some(&real_working_dir.join("no-such-config.toml"))
        );
    }

    fn config_at(root: &Path, path: PathBuf) -> ConfigFile {
        ConfigFile {
            path,
            home_dir: root.to_owned(),
            env: |_| None,
        }
    }

    #[test]
    fn absolute_failure_keeps_the_names_already_watched() {
        let (_dir, root) = real_temp_dir();
        let path = root.join("config.toml");
        fs::write(&path, "[light]\ninterval = \"1m\"\n").expect("write config");
        let mut watch = ConfigWatch::start(config_at(&root, path));
        let watched = watch.watched_names.clone();

        watch.file.path.clear();
        watch.watch_names();

        assert!(!watched.is_empty(), "the config directory was not watched");
        assert_eq!(watch.watched_names, watched);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_walks_the_config_path_off_the_runtime() {
        let (_dir, root) = real_temp_dir();
        let path = root.join("config.toml");
        fs::write(&path, "[light]\ninterval = \"1m\"\n").expect("write config");
        let mut watch = ConfigWatch::start(config_at(&root, path));
        watch.change_seen = true;
        let runtime = thread::current().id();

        let loaded = tokio::time::timeout(Duration::from_secs(5), watch.reloaded())
            .await
            .expect("reload finished")
            .expect("load");

        assert!(loaded.is_some());
        assert_ne!(watch.walked_on.expect("the walk was recorded"), runtime);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_poll_does_not_drain_a_ready_burst() {
        let (sender, events) = mpsc::unbounded_channel();
        let path = PathBuf::from("/tmp/gpaperd-config.toml");
        let mut watch = ConfigWatch {
            file: config_at(Path::new("/tmp"), path.clone()),
            watcher: None,
            events,
            watched_names: vec![path.clone()],
            change_seen: false,
            walked_on: None,
        };
        for _ in 0..1_000 {
            let event = Event::new(EventKind::Create(CreateKind::Any)).add_path(path.clone());
            sender.send(Ok(event)).expect("enqueue");
        }

        let polled = {
            let mut waiting = std::pin::pin!(watch.reloaded());
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        };

        assert!(polled.is_pending());
        assert!(!watch.events.is_empty());
    }
}
