//! The source watch and the slots watch on real directories: inotify, the
//! debouncer and the worker threads, through the public API only.
//!
//! Every wait has a limit, so a report that never comes fails the test
//! instead of hanging it.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpaperd_core::catalog::{FsChange, LocalFile};
use gpaperd_core::config::{DirOptions, LinkPolicy, Source};
use gpaperd_core::style::Style;
use gpaperd_daemon::scan::Scan;
use gpaperd_daemon::slots;
use gpaperd_daemon::watch::{SlotsWatch, SourceNews, SourceWatch};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};

/// Longer than the debounce plus any delay a loaded test machine adds.
const PATIENCE: Duration = Duration::from_secs(10);
/// Longer than a change takes to be reported, so that silence for this long
/// means "not reported".
const QUIET: Duration = Duration::from_millis(1500);

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

fn dir_source(path: &Path, options: DirOptions) -> Source {
    Source::Dir {
        path: path.to_owned(),
        options,
    }
}

fn flat(path: &Path) -> Source {
    dir_source(path, DirOptions::default())
}

fn recursive(path: &Path) -> Source {
    let options = DirOptions {
        recursive: true,
        ..DirOptions::default()
    };
    dir_source(path, options)
}

async fn news(watch: &mut SourceWatch) -> SourceNews {
    timeout(PATIENCE, watch.next())
        .await
        .expect("news of the sources in time")
}

/// Starts a watch and waits for its scan. From then on every change is
/// reported.
async fn watching(sources: Vec<Source>) -> (SourceWatch, Scan) {
    let mut watch = SourceWatch::start(sources);
    match news(&mut watch).await {
        SourceNews::Scanned(scan) => (watch, scan),
        SourceNews::Changed(changes) => panic!("changes before the scan: {changes:?}"),
    }
}

async fn changes(watch: &mut SourceWatch) -> Vec<FsChange> {
    match news(watch).await {
        SourceNews::Changed(changes) => changes,
        SourceNews::Scanned(scan) => panic!("a second scan: {scan:?}"),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).expect("path is under the root");
    relative
        .to_str()
        .expect("fixture names are UTF-8")
        .to_owned()
}

/// Changes in words, with paths relative to `root`: `found walls/a.png`,
/// `removed walls/b.png`.
fn told(root: &Path, changes: &[FsChange]) -> Vec<String> {
    changes
        .iter()
        .map(|change| match change {
            FsChange::Found(file) => format!("found {}", relative(root, &file.path)),
            FsChange::Removed(path) => format!("removed {}", relative(root, path)),
        })
        .collect()
}

/// Scanned paths relative to `root`, sorted, because readdir order is not
/// stable.
fn scanned_paths(root: &Path, scan: &Scan) -> Vec<String> {
    let mut paths: Vec<String> = scan
        .files
        .iter()
        .map(|file| relative(root, &file.path))
        .collect();
    paths.sort();
    paths
}

/// The file of a report that holds one found file and nothing else.
fn only_found(changes: &[FsChange]) -> &LocalFile {
    match changes {
        [FsChange::Found(file)] => file,
        other => panic!("expected one found file, got {other:?}"),
    }
}

#[tokio::test]
async fn first_news_is_the_scan_of_all_sources() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    write(&root.join("hero.jxl"), "hero");
    let missing = root.join("not-there");
    let sources = vec![
        flat(&root.join("walls")),
        Source::File(root.join("hero.jxl")),
        flat(&missing),
    ];

    let (_watch, scan) = watching(sources).await;

    assert_eq!(scanned_paths(&root, &scan), ["hero.jxl", "walls/a.png"]);
    let warned: Vec<&Path> = scan.warnings.iter().map(|w| w.path.as_path()).collect();
    assert_eq!(warned, [&missing]);
}

#[tokio::test]
async fn created_file_is_found() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    let (mut watch, _scan) = watching(vec![flat(&root.join("walls"))]).await;

    write(&root.join("walls/b.png"), "b");

    let changes = changes(&mut watch).await;
    assert_eq!(told(&root, &changes), ["found walls/b.png"]);
}

#[tokio::test]
async fn deleted_file_is_removed() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    write(&root.join("walls/b.png"), "b");
    let (mut watch, _scan) = watching(vec![flat(&root.join("walls"))]).await;

    fs::remove_file(root.join("walls/a.png")).expect("delete fixture");

    let changes = changes(&mut watch).await;
    assert_eq!(told(&root, &changes), ["removed walls/a.png"]);
}

#[tokio::test]
async fn file_edited_in_place_is_found_with_a_new_stamp_on_its_inode() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    let (mut watch, scan) = watching(vec![flat(&root.join("walls"))]).await;
    let before = &scan.files[0];

    fs::write(root.join("walls/a.png"), "edited in place").expect("edit fixture");

    let changes = changes(&mut watch).await;
    let after = only_found(&changes);
    assert_eq!(after.path, before.path);
    assert_eq!(after.stamp.id, before.stamp.id);
    assert_eq!(after.stamp.size, 15);
}

#[tokio::test]
async fn atomic_save_is_one_found_file_with_a_new_inode() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    let (mut watch, scan) = watching(vec![flat(&root.join("walls"))]).await;
    let before = &scan.files[0];

    write(&root.join("walls/a.png.tmp"), "saved by rename");
    fs::rename(root.join("walls/a.png.tmp"), root.join("walls/a.png")).expect("rename fixture");

    let changes = changes(&mut watch).await;
    let after = only_found(&changes);
    assert_eq!(after.path, before.path);
    assert_ne!(after.stamp.id, before.stamp.id);
}

#[tokio::test]
async fn file_moved_in_is_found_and_file_moved_out_is_removed() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    write(&root.join("elsewhere/b.png"), "b");
    let (mut watch, _scan) = watching(vec![flat(&root.join("walls"))]).await;

    fs::rename(root.join("elsewhere/b.png"), root.join("walls/b.png")).expect("move in");
    let moved_in = changes(&mut watch).await;
    fs::rename(root.join("walls/a.png"), root.join("elsewhere/a.png")).expect("move out");
    let moved_out = changes(&mut watch).await;

    assert_eq!(told(&root, &moved_in), ["found walls/b.png"]);
    assert_eq!(told(&root, &moved_out), ["removed walls/a.png"]);
}

#[tokio::test]
async fn renamed_file_is_removed_before_it_is_found() {
    let (_dir, root) = temp_root();
    // The new name sorts first: the order is not the order of the paths.
    write(&root.join("walls/z.png"), "z");
    let (mut watch, _scan) = watching(vec![flat(&root.join("walls"))]).await;

    fs::rename(root.join("walls/z.png"), root.join("walls/a.png")).expect("rename fixture");

    let changes = changes(&mut watch).await;
    assert_eq!(
        told(&root, &changes),
        ["removed walls/z.png", "found walls/a.png"]
    );
}

#[tokio::test]
async fn directory_created_after_the_start_is_scanned_and_followed() {
    let (_dir, root) = temp_root();
    let walls = root.join("pictures/walls");
    let (mut watch, scan) = watching(vec![flat(&walls)]).await;
    assert_eq!(scan.files, []);

    fs::create_dir(root.join("pictures")).expect("create the directory above");
    // Lets the watch move from the root to `pictures` before the next step.
    sleep(QUIET).await;
    write(&walls.join("a.png"), "a");
    let appeared = changes(&mut watch).await;
    write(&walls.join("b.png"), "b");
    let followed = changes(&mut watch).await;

    assert_eq!(told(&root, &appeared), ["found pictures/walls/a.png"]);
    assert_eq!(told(&root, &followed), ["found pictures/walls/b.png"]);
}

#[tokio::test]
async fn removed_directory_drops_its_files_and_is_followed_when_it_is_back() {
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    write(&walls.join("a.png"), "a");
    write(&walls.join("b.png"), "b");
    let (mut watch, _scan) = watching(vec![flat(&walls)]).await;

    fs::remove_dir_all(&walls).expect("remove the source");
    let dropped = changes(&mut watch).await;
    write(&walls.join("c.png"), "c");
    let back = changes(&mut watch).await;
    write(&walls.join("d.png"), "d");
    let followed = changes(&mut watch).await;

    assert_eq!(
        told(&root, &dropped),
        ["removed walls/a.png", "removed walls/b.png"]
    );
    assert_eq!(told(&root, &back), ["found walls/c.png"]);
    assert_eq!(told(&root, &followed), ["found walls/d.png"]);
}

#[tokio::test]
async fn subdirectory_created_after_the_start_is_followed_when_recursive() {
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    write(&walls.join("a.png"), "a");
    let (mut watch, _scan) = watching(vec![recursive(&walls)]).await;

    write(&walls.join("nature/forest.png"), "forest");
    let appeared = changes(&mut watch).await;
    write(&walls.join("nature/sea.png"), "sea");
    let followed = changes(&mut watch).await;

    assert_eq!(told(&root, &appeared), ["found walls/nature/forest.png"]);
    assert_eq!(told(&root, &followed), ["found walls/nature/sea.png"]);
}

#[tokio::test]
async fn entries_that_the_scan_skips_are_not_reported() {
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    write(&walls.join("a.png"), "a");
    let (mut watch, _scan) = watching(vec![flat(&walls)]).await;

    write(&walls.join(".hidden.png"), "hidden");
    write(&walls.join("notes.txt"), "no image");
    write(&walls.join("download.png.part"), "in progress");
    write(&walls.join("sub/deep.png"), "below a flat source");
    write(&walls.join("shown.png"), "shown");

    let changes = changes(&mut watch).await;
    assert_eq!(told(&root, &changes), ["found walls/shown.png"]);
}

#[tokio::test]
async fn file_source_is_followed_by_its_name_only() {
    let (_dir, root) = temp_root();
    write(&root.join("pictures/other.png"), "other");
    let hero = root.join("pictures/hero.jxl");
    let (mut watch, scan) = watching(vec![Source::File(hero.clone())]).await;
    assert_eq!(scan.files, []);

    write(&root.join("pictures/sibling.png"), "sibling");
    write(&hero, "hero");
    let appeared = changes(&mut watch).await;
    fs::remove_file(&hero).expect("delete fixture");
    let gone = changes(&mut watch).await;

    assert_eq!(told(&root, &appeared), ["found pictures/hero.jxl"]);
    assert_eq!(told(&root, &gone), ["removed pictures/hero.jxl"]);
}

#[tokio::test]
async fn path_that_two_sources_reach_is_removed_when_the_last_one_loses_it() {
    let (_dir, root) = temp_root();
    write(&root.join("store/real.png"), "real");
    fs::create_dir(root.join("walls")).expect("create dir");
    symlink(root.join("store/real.png"), root.join("walls/link.png")).expect("symlink");
    let sources = vec![flat(&root.join("walls")), flat(&root.join("store"))];
    let (mut watch, scan) = watching(sources).await;
    assert_eq!(
        scanned_paths(&root, &scan),
        ["store/real.png", "store/real.png"]
    );

    fs::remove_file(root.join("walls/link.png")).expect("delete the link");
    write(&root.join("walls/marker.png"), "marker");
    let one_source_left = changes(&mut watch).await;
    fs::remove_file(root.join("store/real.png")).expect("delete the file");
    let no_source_left = changes(&mut watch).await;

    assert_eq!(told(&root, &one_source_left), ["found walls/marker.png"]);
    assert_eq!(told(&root, &no_source_left), ["removed store/real.png"]);
}

#[tokio::test]
async fn directory_that_two_sources_name_differently_is_followed_for_both() {
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    write(&walls.join("a.png"), "a");
    symlink(&walls, root.join("alias")).expect("symlink");
    let sources = vec![flat(&walls), flat(&root.join("alias"))];
    let (mut watch, _scan) = watching(sources).await;

    // inotify reports under one of the two names only.
    fs::remove_file(walls.join("a.png")).expect("delete fixture");
    let gone = changes(&mut watch).await;
    write(&walls.join("b.png"), "b");
    let appeared = changes(&mut watch).await;

    assert_eq!(told(&root, &gone), ["removed walls/a.png"]);
    assert_eq!(told(&root, &appeared), ["found walls/b.png"]);
}

#[tokio::test]
async fn source_with_a_poll_option_is_followed_on_that_period() {
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    write(&walls.join("a.png"), "a");
    let options = DirOptions {
        poll: Some(Duration::from_millis(100)),
        ..DirOptions::default()
    };
    let (mut watch, _scan) = watching(vec![dir_source(&walls, options)]).await;

    write(&walls.join("b.png"), "b");

    let changes = changes(&mut watch).await;
    assert_eq!(told(&root, &changes), ["found walls/b.png"]);
}

#[tokio::test]
async fn burst_of_a_thousand_files_arrives_in_a_few_reports() {
    /// One report is the aim. A slow machine may cut the burst in two.
    const MOST_REPORTS: usize = 3;
    const FILES: usize = 1000;
    let (_dir, root) = temp_root();
    let walls = root.join("walls");
    fs::create_dir(&walls).expect("create dir");
    let (mut watch, _scan) = watching(vec![flat(&walls)]).await;

    for number in 0..FILES {
        write(&walls.join(format!("{number:04}.png")), "burst");
    }

    let mut found = BTreeSet::new();
    let mut reports = 0;
    while found.len() < FILES {
        reports += 1;
        for change in changes(&mut watch).await {
            let FsChange::Found(file) = change else {
                panic!("nothing was removed, got {change:?}");
            };
            assert!(found.insert(file.path.clone()), "found twice: {file:?}");
        }
    }
    assert!(reports <= MOST_REPORTS, "{reports} reports");
    assert!(timeout(QUIET, watch.next()).await.is_err());
}

#[tokio::test]
async fn nothing_is_reported_while_nothing_changes() {
    let (_dir, root) = temp_root();
    write(&root.join("walls/a.png"), "a");
    let (mut watch, _scan) = watching(vec![flat(&root.join("walls"))]).await;

    // Reading is no change, and neither is a write that leaves no trace.
    fs::read(root.join("walls/a.png")).expect("read fixture");
    write(&root.join("walls/b.png"), "b");
    fs::remove_file(root.join("walls/b.png")).expect("delete fixture");

    assert!(timeout(QUIET, watch.next()).await.is_err());
}

#[tokio::test]
async fn new_watch_replaces_the_set_of_sources() {
    let (_dir, root) = temp_root();
    let (light, dark) = (root.join("light"), root.join("dark"));
    write(&light.join("a.png"), "a");
    write(&dark.join("n.png"), "n");
    let (watch_of_light, _scan) = watching(vec![flat(&light)]).await;
    // A change that the first watch has not reported when it is replaced.
    write(&light.join("b.png"), "b");

    let (mut watch_of_both, scan_of_both) = watching(vec![flat(&light), flat(&dark)]).await;
    drop(watch_of_light);
    write(&light.join("c.png"), "c");
    let kept_source = changes(&mut watch_of_both).await;
    write(&dark.join("o.png"), "o");
    let added_source = changes(&mut watch_of_both).await;

    drop(watch_of_both);
    let (mut watch_of_dark, _scan) = watching(vec![flat(&dark)]).await;
    write(&light.join("d.png"), "d");
    write(&dark.join("p.png"), "p");
    let dropped_source = changes(&mut watch_of_dark).await;

    assert_eq!(
        scanned_paths(&root, &scan_of_both),
        ["dark/n.png", "light/a.png", "light/b.png"]
    );
    assert_eq!(told(&root, &kept_source), ["found light/c.png"]);
    assert_eq!(told(&root, &added_source), ["found dark/o.png"]);
    assert_eq!(told(&root, &dropped_source), ["found dark/p.png"]);
}

#[tokio::test]
async fn style_without_local_sources_is_scanned_and_then_silent() {
    let (mut watch, scan) = watching(Vec::new()).await;

    assert_eq!(scan, Scan::default());
    assert!(timeout(QUIET, watch.next()).await.is_err());
}

/// The slots directory of a test, with the content that publishes come from.
struct Slots {
    _dir: TempDir,
    root: PathBuf,
    dir: PathBuf,
}

impl Slots {
    fn new() -> Slots {
        let (_dir, root) = temp_root();
        let dir = root.join("data/gpaperd/slots");
        Slots { _dir, root, dir }
    }

    fn slot(&self, style: Style) -> PathBuf {
        self.dir.join(style.as_str())
    }

    /// Fills a slot the way the daemon does: a new file renamed onto it.
    fn publish(&self, style: Style, content: &str) {
        let image = self.root.join("image.png");
        write(&image, content);
        slots::publish(&image, &self.slot(style), LinkPolicy::AlwaysCopy).expect("publish");
    }
}

async fn lost(watch: &mut SlotsWatch) -> Style {
    timeout(PATIENCE, watch.lost())
        .await
        .expect("a lost slot in time")
}

#[tokio::test]
async fn deleted_slot_is_lost() {
    let slots = Slots::new();
    slots.publish(Style::Light, "day");
    slots.publish(Style::Dark, "night");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    fs::remove_file(slots.slot(Style::Dark)).expect("delete the slot");

    assert_eq!(lost(&mut watch).await, Style::Dark);
    assert!(timeout(QUIET, watch.lost()).await.is_err());
}

#[tokio::test]
async fn slot_replaced_by_a_publish_is_not_lost() {
    let slots = Slots::new();
    slots.publish(Style::Light, "day");
    slots.publish(Style::Dark, "night");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    slots.publish(Style::Light, "another day");
    // A loss of the light slot would be reported before this one.
    fs::remove_file(slots.slot(Style::Dark)).expect("delete the slot");

    assert_eq!(lost(&mut watch).await, Style::Dark);
    assert!(timeout(QUIET, watch.lost()).await.is_err());
}

#[tokio::test]
async fn slot_moved_away_is_lost() {
    let slots = Slots::new();
    slots.publish(Style::Light, "day");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    fs::rename(slots.slot(Style::Light), slots.root.join("taken")).expect("move the slot");

    assert_eq!(lost(&mut watch).await, Style::Light);
}

#[tokio::test]
async fn slot_is_lost_again_only_after_it_was_back() {
    let slots = Slots::new();
    slots.publish(Style::Light, "day");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    fs::remove_file(slots.slot(Style::Light)).expect("delete the slot");
    let first = lost(&mut watch).await;
    // More events in the directory while the slot is still gone.
    write(&slots.dir.join("stray"), "stray");
    let while_gone = timeout(QUIET, watch.lost()).await;
    slots.publish(Style::Light, "day again");
    fs::remove_file(slots.slot(Style::Light)).expect("delete the slot again");
    let second = lost(&mut watch).await;

    assert_eq!(first, Style::Light);
    assert!(while_gone.is_err());
    assert_eq!(second, Style::Light);
}

#[tokio::test]
async fn slots_directory_that_the_first_publish_creates_is_watched() {
    let slots = Slots::new();
    fs::create_dir_all(&slots.root).expect("create the root");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    slots.publish(Style::Light, "day");
    // Lets the watch move from the root to the new slots directory.
    sleep(QUIET).await;
    fs::remove_file(slots.slot(Style::Light)).expect("delete the slot");

    assert_eq!(lost(&mut watch).await, Style::Light);
}

#[tokio::test]
async fn removed_slots_directory_loses_every_slot_and_is_watched_when_it_is_back() {
    let slots = Slots::new();
    slots.publish(Style::Light, "day");
    slots.publish(Style::Dark, "night");
    let mut watch = SlotsWatch::start(slots.dir.clone());

    fs::remove_dir_all(&slots.dir).expect("remove the slots directory");
    let both = BTreeSet::from([lost(&mut watch).await, lost(&mut watch).await]);
    slots.publish(Style::Dark, "night again");
    sleep(QUIET).await;
    fs::remove_file(slots.slot(Style::Dark)).expect("delete the slot");
    let again = lost(&mut watch).await;

    assert_eq!(both, BTreeSet::from(Style::ALL));
    assert_eq!(again, Style::Dark);
}
