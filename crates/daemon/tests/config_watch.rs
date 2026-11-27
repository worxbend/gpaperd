//! The config file watch against real files and real inotify events.
//!
//! Every test waits with a timeout, so a reload that never comes fails the
//! test and does not hang it.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpaperd_core::config::{Config, Source};
use gpaperd_daemon::config_watch::{ConfigFile, ConfigWatch, LoadError, RELOAD_DEBOUNCE};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for a second reload to show up, if there was one.
const QUIET_PERIOD: Duration = Duration::from_millis(1000);

/// A temp directory that stands for the user's home: `config/` is where the
/// config file is looked for, and `repo/` is a dotfiles repository.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        let root = TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join("config")).expect("create the config dir");
        fs::create_dir(root.path().join("repo")).expect("create the repo dir");
        Home { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// The config file that the daemon is told to use.
    fn config_file(&self) -> ConfigFile {
        self.config_file_at("config/config.toml")
    }

    fn config_file_at(&self, relative: &str) -> ConfigFile {
        ConfigFile {
            path: self.path(relative),
            home_dir: self.root.path().to_owned(),
            env: undefined,
        }
    }

    /// Writes into the file itself, as `echo > file` does.
    fn write(&self, relative: &str, text: &str) {
        fs::write(self.path(relative), text).expect("write file");
    }

    /// Replaces the file by a rename, as an editor saves.
    fn save(&self, relative: &str, text: &str) {
        let staged = self.path(relative).with_extension("staged");
        fs::write(&staged, text).expect("stage file");
        fs::rename(staged, self.path(relative)).expect("rename over file");
    }

    /// Points `link` at `target` in one step, as `ln -sfn` does.
    fn relink(&self, link: &str, target: &str) {
        let staged = self.path(link).with_extension("staged");
        symlink(self.path(target), &staged).expect("stage symlink");
        fs::rename(staged, self.path(link)).expect("rename over symlink");
    }
}

fn undefined(_name: &str) -> Option<OsString> {
    None
}

/// Config text that differs from every other in the light style's interval.
fn with_interval(minutes: u64) -> String {
    format!("[light]\ninterval = \"{minutes}m\"\n")
}

/// What [`with_interval`] parses to, as far as a test tells configs apart.
fn interval_minutes(loaded: Result<Option<Config>, LoadError>) -> Option<u64> {
    let config = loaded.expect("a config that loads")?;
    Some(config.styles.light.interval.as_secs() / 60)
}

async fn next_reload(watch: &mut ConfigWatch) -> Result<Option<Config>, LoadError> {
    timeout(RELOAD_TIMEOUT, watch.reloaded())
        .await
        .expect("a reload within the timeout")
}

async fn assert_no_reload(watch: &mut ConfigWatch) {
    let reload = timeout(QUIET_PERIOD, watch.reloaded()).await;
    assert!(reload.is_err(), "unexpected reload: {reload:?}");
}

#[tokio::test]
async fn file_that_is_missing_at_start_is_loaded_when_it_appears() {
    let home = Home::new();
    let mut watch = ConfigWatch::start(home.config_file());

    home.write("config/config.toml", &with_interval(2));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(2));
}

#[tokio::test]
async fn file_in_a_directory_that_is_missing_at_start_is_loaded_when_it_appears() {
    let home = Home::new();
    let mut watch = ConfigWatch::start(home.config_file_at("config/gpaperd/sub/config.toml"));

    fs::create_dir_all(home.path("config/gpaperd/sub")).expect("create the missing dirs");
    home.write("config/gpaperd/sub/config.toml", &with_interval(2));
    let first = interval_minutes(next_reload(&mut watch).await);
    home.save("config/gpaperd/sub/config.toml", &with_interval(3));
    let second = interval_minutes(next_reload(&mut watch).await);

    assert_eq!((first, second), (Some(2), Some(3)));
}

#[tokio::test]
async fn write_into_the_file_is_one_reload() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    home.write("config/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
    // Loading the file opens it, and that must not count as a change.
    assert_no_reload(&mut watch).await;
}

#[tokio::test]
async fn save_by_rename_is_a_reload() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("config/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn burst_of_saves_is_one_reload_of_the_last_one() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    let mut saved_at = Vec::new();
    for minutes in 3..=7 {
        home.save("config/config.toml", &with_interval(minutes));
        saved_at.push(Instant::now());
        sleep(RELOAD_DEBOUNCE / 6).await;
    }
    let mut reloads = Vec::new();
    while reloads.last() != Some(&Some(7)) && reloads.len() < 5 {
        reloads.push(interval_minutes(next_reload(&mut watch).await));
    }

    assert_eq!(reloads.last(), Some(&Some(7)), "{reloads:?}");
    assert_no_reload(&mut watch).await;
    // A save that a stalled test thread made late may end the debounce
    // early, and then it rightly reloads. Without such a stall the burst is
    // one reload.
    let is_one_burst = saved_at
        .windows(2)
        .all(|pair| pair[1] - pair[0] < RELOAD_DEBOUNCE / 2);
    if is_one_burst {
        assert_eq!(reloads, [Some(7)]);
    }
}

#[tokio::test]
async fn other_file_in_the_directory_is_no_reload() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("config/other.toml", &with_interval(3));

    assert_no_reload(&mut watch).await;
}

#[tokio::test]
async fn deleted_file_is_reported_as_gone_and_loaded_when_it_is_back() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    fs::remove_file(home.path("config/config.toml")).expect("delete the config");
    let gone = interval_minutes(next_reload(&mut watch).await);
    home.write("config/config.toml", &with_interval(3));
    let back = interval_minutes(next_reload(&mut watch).await);

    assert_eq!((gone, back), (None, Some(3)));
}

#[tokio::test]
async fn invalid_file_gives_line_and_column_and_a_later_save_loads() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("config/config.toml", "[light]\ninterval = \"8d\"\n");
    let invalid = next_reload(&mut watch).await;
    home.save("config/config.toml", &with_interval(3));
    let repaired = interval_minutes(next_reload(&mut watch).await);

    assert!(
        matches!(&invalid, Err(LoadError::Invalid(error)) if (error.line, error.column) == (2, 12)),
        "{invalid:?}"
    );
    assert_eq!(repaired, Some(3));
}

#[tokio::test]
async fn symlink_pointed_at_another_file_is_a_reload() {
    let home = Home::new();
    home.write("repo/day.toml", &with_interval(2));
    home.write("repo/night.toml", &with_interval(3));
    home.relink("config/config.toml", "repo/day.toml");
    let mut watch = ConfigWatch::start(home.config_file());

    home.relink("config/config.toml", "repo/night.toml");

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn after_a_relink_the_new_target_is_watched_and_the_old_one_is_not() {
    let home = Home::new();
    home.write("repo/day.toml", &with_interval(2));
    home.write("repo/night.toml", &with_interval(3));
    home.relink("config/config.toml", "repo/day.toml");
    let mut watch = ConfigWatch::start(home.config_file());
    home.relink("config/config.toml", "repo/night.toml");
    next_reload(&mut watch).await.expect("the relinked config");

    home.save("repo/night.toml", &with_interval(4));
    let new_target = interval_minutes(next_reload(&mut watch).await);
    home.save("repo/day.toml", &with_interval(5));

    assert_eq!(new_target, Some(4));
    assert_no_reload(&mut watch).await;
}

#[tokio::test]
async fn symlink_target_saved_in_its_own_directory_is_a_reload() {
    let home = Home::new();
    home.write("repo/config.toml", &with_interval(2));
    home.relink("config/config.toml", "repo/config.toml");
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("repo/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn symlink_target_written_in_its_own_directory_is_a_reload() {
    let home = Home::new();
    home.write("repo/config.toml", &with_interval(2));
    home.relink("config/config.toml", "repo/config.toml");
    let mut watch = ConfigWatch::start(home.config_file());

    home.write("repo/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn file_behind_two_symlinks_is_watched_in_every_directory() {
    let home = Home::new();
    fs::create_dir(home.path("store")).expect("create the store dir");
    home.write("store/config.toml", &with_interval(2));
    home.relink("repo/config.toml", "store/config.toml");
    home.relink("config/config.toml", "repo/config.toml");
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("store/config.toml", &with_interval(3));
    let real_file = interval_minutes(next_reload(&mut watch).await);
    home.write("store/other.toml", &with_interval(4));
    home.relink("repo/config.toml", "store/other.toml");
    let middle_link = interval_minutes(next_reload(&mut watch).await);

    assert_eq!((real_file, middle_link), (Some(3), Some(4)));
}

#[tokio::test]
async fn symlink_whose_target_is_missing_at_start_is_loaded_when_the_target_appears() {
    let home = Home::new();
    home.relink("config/config.toml", "repo/later/config.toml");
    let mut watch = ConfigWatch::start(home.config_file());

    fs::create_dir(home.path("repo/later")).expect("create the target's dir");
    home.write("repo/later/config.toml", &with_interval(2));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(2));
}

#[tokio::test]
async fn config_directory_that_is_a_symlink_is_watched_where_it_leads() {
    let home = Home::new();
    fs::create_dir(home.path("repo/gpaperd")).expect("create the real config dir");
    home.write("repo/gpaperd/config.toml", &with_interval(2));
    home.relink("config/gpaperd", "repo/gpaperd");
    let mut watch = ConfigWatch::start(home.config_file_at("config/gpaperd/config.toml"));

    home.save("repo/gpaperd/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn config_directory_symlink_pointed_at_another_directory_is_a_reload() {
    let home = Home::new();
    for (dir, minutes) in [("repo/home", 2), ("repo/work", 3)] {
        fs::create_dir(home.path(dir)).expect("create a config dir in the repo");
        home.write(&format!("{dir}/config.toml"), &with_interval(minutes));
    }
    home.relink("config/gpaperd", "repo/home");
    let mut watch = ConfigWatch::start(home.config_file_at("config/gpaperd/config.toml"));

    home.relink("config/gpaperd", "repo/work");
    let relinked = interval_minutes(next_reload(&mut watch).await);
    home.save("repo/work/config.toml", &with_interval(4));
    let saved = interval_minutes(next_reload(&mut watch).await);

    assert_eq!((relinked, saved), (Some(3), Some(4)));
}

#[tokio::test]
async fn relative_symlink_is_followed_from_its_own_directory() {
    let home = Home::new();
    home.write("repo/config.toml", &with_interval(2));
    symlink("../repo/config.toml", home.path("config/config.toml")).expect("create symlink");
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("repo/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn config_directory_that_is_deleted_and_made_again_is_watched_again() {
    let home = Home::new();
    fs::create_dir(home.path("config/gpaperd")).expect("create the config dir");
    home.write("config/gpaperd/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file_at("config/gpaperd/config.toml"));

    fs::remove_dir_all(home.path("config/gpaperd")).expect("delete the config dir");
    let gone = interval_minutes(next_reload(&mut watch).await);
    fs::create_dir(home.path("config/gpaperd")).expect("create the config dir again");
    home.write("config/gpaperd/config.toml", &with_interval(3));
    let back = interval_minutes(next_reload(&mut watch).await);
    home.save("config/gpaperd/config.toml", &with_interval(4));
    let saved = interval_minutes(next_reload(&mut watch).await);

    assert_eq!((gone, back, saved), (None, Some(3), Some(4)));
}

#[tokio::test]
async fn change_seen_by_a_cancelled_wait_is_returned_by_the_next_one() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());
    home.save("config/config.toml", &with_interval(3));

    // Long enough to see the change, too short for the quiet time after it.
    let cancelled = timeout(RELOAD_DEBOUNCE / 2, watch.reloaded()).await;

    assert!(cancelled.is_err(), "{cancelled:?}");
    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn change_while_nobody_waits_is_returned_by_the_next_wait() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let mut watch = ConfigWatch::start(home.config_file());

    home.save("config/config.toml", &with_interval(3));
    sleep(RELOAD_DEBOUNCE * 2).await;

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

#[tokio::test]
async fn relative_config_path_is_watched() {
    let home = Home::new();
    home.write("config/config.toml", &with_interval(2));
    let from_working_dir = relative_to_working_dir(&home.path("config/config.toml"));
    let mut watch = ConfigWatch::start(ConfigFile {
        path: from_working_dir,
        ..home.config_file()
    });

    home.save("config/config.toml", &with_interval(3));

    assert_eq!(interval_minutes(next_reload(&mut watch).await), Some(3));
}

/// `absolute` as a path with `..` components that leads there from the
/// working directory.
fn relative_to_working_dir(absolute: &Path) -> PathBuf {
    let working_dir = std::env::current_dir().expect("working directory");
    let up_to_root: PathBuf = working_dir.components().skip(1).map(|_| "..").collect();
    up_to_root.join(absolute.strip_prefix("/").expect("an absolute path"))
}

#[test]
fn load_of_a_missing_file_is_none() {
    let home = Home::new();

    let loaded = home.config_file().load();

    assert!(matches!(loaded, Ok(None)), "{loaded:?}");
}

#[test]
fn load_resolves_relative_sources_against_the_directory_of_the_real_file() {
    let home = Home::new();
    home.write("repo/config.toml", "[light]\nsources = [\"walls\"]\n");
    home.relink("config/config.toml", "repo/config.toml");

    let loaded = home.config_file().load();

    let config = loaded.expect("a config that loads").expect("a config");
    let repo = fs::canonicalize(home.path("repo")).expect("canonical repo dir");
    assert_eq!(
        config.styles.light.sources,
        [Source::Path(repo.join("walls"))]
    );
}

#[test]
fn load_of_a_directory_is_an_io_error() {
    let home = Home::new();
    fs::create_dir(home.path("config/config.toml")).expect("create a dir in the file's place");

    let loaded = home.config_file().load();

    assert!(matches!(loaded, Err(LoadError::Io(_))), "{loaded:?}");
}
