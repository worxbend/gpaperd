//! The remote cache on a temp directory: layout, `meta.toml`, loading, and
//! garbage collection.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::catalog::RemoteBlob;
use gpaperd_daemon::cache::{Cache, GC_GRACE, GcReport, META_FILE, Meta};
use tempfile::TempDir;
use url::Url;

const PNG_START: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

struct Harness {
    _dir: TempDir,
    root: PathBuf,
    cache: Cache,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        Harness {
            cache: Cache::new(root.clone()),
            root,
            _dir: dir,
        }
    }

    async fn publish(&self, url: &Url, body: &[u8]) -> PathBuf {
        let mut draft = self.cache.draft(url).await.unwrap();
        draft.append(body).await.unwrap();
        self.cache
            .publish(url, draft, "png", Meta::new(url.clone()))
            .await
            .unwrap()
    }

    fn entry_files(&self, url: &Url) -> Vec<String> {
        file_names(&self.cache.entry_dir(url))
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn blob_name(body: &[u8]) -> String {
    format!("{}.png", blake3::hash(body).to_hex())
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn missing_root_loads_empty_and_collects_nothing() {
    let harness = Harness::new();

    assert_eq!(harness.cache.load().unwrap(), Vec::new());
    assert_eq!(
        harness.cache.collect_garbage(&[], at(1)).unwrap(),
        GcReport::default()
    );
    assert!(!harness.root.exists());
}

#[tokio::test]
async fn recorded_meta_reads_back() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png?k=v");
    let mut meta = Meta::new(url.clone());
    meta.etag = Some("\"x\"".into());
    meta.fetched_at = Some(at(1_800_000_000));

    harness.cache.record(&url, &meta).await.unwrap();

    assert_eq!(harness.cache.meta(&url).await.unwrap(), Some(meta));
    assert_eq!(harness.entry_files(&url), [META_FILE]);
}

#[tokio::test]
async fn meta_of_an_unknown_url_is_none() {
    let harness = Harness::new();

    assert_eq!(
        harness
            .cache
            .meta(&url("https://example.com/a"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn meta_naming_another_url_is_a_collision() {
    let harness = Harness::new();
    let mine = url("https://example.com/mine.png");
    let other = url("https://example.com/other.png");
    let dir = harness.cache.entry_dir(&mine);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(META_FILE), format!("url = \"{other}\"\n")).unwrap();

    assert_eq!(harness.cache.meta(&mine).await.unwrap(), None);
}

#[tokio::test]
async fn corrupt_meta_is_an_error() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let dir = harness.cache.entry_dir(&url);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(META_FILE), "url = ").unwrap();

    assert!(harness.cache.meta(&url).await.is_err());
}

#[tokio::test]
async fn published_blob_is_named_by_its_content() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");

    let blob = harness.publish(&url, PNG_START).await;

    assert_eq!(
        blob,
        harness.cache.entry_dir(&url).join(blob_name(PNG_START))
    );
    assert_eq!(fs::read(&blob).unwrap(), PNG_START);
    assert_eq!(
        harness.entry_files(&url),
        [blob_name(PNG_START), META_FILE.into()]
    );
    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, Some(blob_name(PNG_START)));
}

#[tokio::test]
async fn publishing_the_same_bytes_keeps_the_blob_file() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let first = harness.publish(&url, PNG_START).await;
    let inode = fs::metadata(&first).unwrap().ino();

    let second = harness.publish(&url, PNG_START).await;

    assert_eq!(second, first);
    assert_eq!(fs::metadata(&second).unwrap().ino(), inode);
    assert_eq!(
        harness.entry_files(&url),
        [blob_name(PNG_START), META_FILE.into()]
    );
}

#[tokio::test]
async fn new_version_leaves_the_old_blob_alone() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let old = harness.publish(&url, PNG_START).await;
    let newer = [PNG_START, b"v2"].concat();

    let new = harness.publish(&url, &newer).await;

    assert_ne!(new, old);
    assert_eq!(fs::read(old).unwrap(), PNG_START);
    assert_eq!(fs::read(new).unwrap(), newer);
}

#[tokio::test]
async fn dropped_draft_deletes_its_temp_file() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let mut draft = harness.cache.draft(&url).await.unwrap();
    draft.append(PNG_START).await.unwrap();
    assert_eq!(draft.len(), PNG_START.len() as u64);
    assert_eq!(harness.entry_files(&url).len(), 1);

    drop(draft);

    assert_eq!(harness.entry_files(&url), Vec::<String>::new());
}

#[tokio::test]
async fn load_returns_every_entry_with_a_blob_and_skips_the_rest() {
    let harness = Harness::new();
    let good = url("https://example.com/good.png");
    let no_blob = url("https://example.com/none.png");
    let lost = url("https://example.com/lost.png");
    let corrupt = url("https://example.com/corrupt.png");
    let good_blob = harness.publish(&good, PNG_START).await;
    harness
        .cache
        .record(&no_blob, &Meta::new(no_blob.clone()))
        .await
        .unwrap();
    fs::remove_file(harness.publish(&lost, b"lost").await).unwrap();
    fs::create_dir_all(harness.cache.entry_dir(&corrupt)).unwrap();
    fs::write(harness.cache.entry_dir(&corrupt).join(META_FILE), "]]").unwrap();
    fs::write(harness.root.join("stray-file"), "x").unwrap();

    assert_eq!(
        harness.cache.load().unwrap(),
        [RemoteBlob {
            url: good,
            blob: good_blob
        }]
    );
}

#[tokio::test]
async fn load_skips_a_meta_in_the_wrong_directory() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    fs::rename(
        harness.cache.entry_dir(&url),
        harness.root.join("elsewhere"),
    )
    .unwrap();

    assert_eq!(harness.cache.load().unwrap(), Vec::new());
}

#[tokio::test]
async fn load_does_not_follow_a_symlinked_blob() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let blob = harness.publish(&url, PNG_START).await;
    let target = harness.root.join("outside");
    fs::rename(&blob, &target).unwrap();
    std::os::unix::fs::symlink(&target, &blob).unwrap();

    assert_eq!(harness.cache.load().unwrap(), Vec::new());
}

#[tokio::test]
async fn gc_deletes_unnamed_blobs_and_stale_temp_files_only() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let old = harness.publish(&url, b"old").await;
    let current = harness.publish(&url, PNG_START).await;
    // A temp file of an earlier run: pid 1 is never this test.
    fs::write(harness.cache.entry_dir(&url).join(".tmp-1-0"), "partial").unwrap();

    let report = harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), at(1))
        .unwrap();

    assert_eq!(
        report,
        GcReport {
            entries: 0,
            blobs: 2
        }
    );
    assert!(!old.exists());
    assert_eq!(
        harness.entry_files(&url),
        [blob_name(PNG_START), META_FILE.into()]
    );
    assert_eq!(fs::read(current).unwrap(), PNG_START);
}

#[tokio::test]
async fn gc_deletes_a_temp_file_left_by_an_earlier_run_with_the_same_pid() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let leftover = format!(".tmp-{}-999999999", std::process::id());
    fs::write(harness.cache.entry_dir(&url).join(&leftover), "partial").unwrap();

    let report = harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), at(1))
        .unwrap();

    assert_eq!(report.blobs, 1);
    assert_eq!(
        harness.entry_files(&url),
        [blob_name(PNG_START), META_FILE.into()]
    );
}

#[tokio::test]
async fn gc_leaves_an_entry_whose_meta_cannot_be_read_for_now() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let blob = harness.publish(&url, PNG_START).await;
    let meta = harness.cache.entry_dir(&url).join(META_FILE);
    fs::set_permissions(&meta, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read(&meta);
    let report = harness.cache.collect_garbage(&[], at(1));
    fs::set_permissions(&meta, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(readable.is_err(), "mode 000 left meta.toml readable");

    let report = report.unwrap();
    assert_eq!(report, GcReport::default());
    assert_eq!(fs::read(&blob).unwrap(), PNG_START);
    assert_eq!(harness.cache.meta(&url).await.unwrap().unwrap().url, url);
}

#[tokio::test]
async fn meta_and_its_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let url = url("https://user:secret@example.com/a.png?token=x");
    harness.publish(&url, PNG_START).await;
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

    let entry = harness.cache.entry_dir(&url);

    assert_eq!(mode(&harness.root), 0o700);
    assert_eq!(mode(&entry), 0o700);
    assert_eq!(mode(&entry.join(META_FILE)), 0o600);
    assert_eq!(mode(&entry.join(blob_name(PNG_START))), 0o600);
}

#[tokio::test]
async fn republishing_the_same_bytes_keeps_the_blob_mode() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let blob = harness.publish(&url, PNG_START).await;
    fs::set_permissions(&blob, fs::Permissions::from_mode(0o640)).unwrap();

    let again = harness.publish(&url, PNG_START).await;

    assert_eq!(again, blob);
    assert_eq!(
        fs::metadata(&again).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[tokio::test]
async fn a_symlinked_blob_path_is_replaced_with_the_new_bytes() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let blob = harness.publish(&url, PNG_START).await;
    let outside = harness.root.join("outside");
    fs::write(&outside, b"not the image").unwrap();
    fs::remove_file(&blob).unwrap();
    std::os::unix::fs::symlink(&outside, &blob).unwrap();

    let again = harness.publish(&url, PNG_START).await;

    assert!(fs::symlink_metadata(&again).unwrap().is_file());
    assert_eq!(fs::read(&again).unwrap(), PNG_START);
    assert_eq!(fs::read(&outside).unwrap(), b"not the image");
}

#[tokio::test]
async fn gc_keeps_an_entry_that_has_no_blob_yet() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let mut meta = Meta::new(url.clone());
    meta.gone = true;
    harness.cache.record(&url, &meta).await.unwrap();

    harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), at(1))
        .unwrap();

    assert_eq!(harness.cache.meta(&url).await.unwrap(), Some(meta));
}

#[tokio::test]
async fn gc_keeps_a_draft_in_progress() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let mut draft = harness.cache.draft(&url).await.unwrap();
    draft.append(PNG_START).await.unwrap();

    harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), at(1))
        .unwrap();
    let blob = harness
        .cache
        .publish(&url, draft, "png", Meta::new(url.clone()))
        .await
        .unwrap();

    assert_eq!(fs::read(blob).unwrap(), PNG_START);
}

#[tokio::test]
async fn gc_deletes_an_entry_seven_days_after_its_url_left_the_config() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let left = at(1_800_000_000);

    let first = harness.cache.collect_garbage(&[], left).unwrap();
    let at_grace = harness.cache.collect_garbage(&[], left + GC_GRACE).unwrap();
    assert_eq!(
        harness.cache.meta(&url).await.unwrap().unwrap().orphaned_at,
        Some(left)
    );
    let after = harness
        .cache
        .collect_garbage(&[], left + GC_GRACE + Duration::from_secs(1))
        .unwrap();

    assert_eq!((first.entries, at_grace.entries, after.entries), (0, 0, 1));
    assert!(!harness.cache.entry_dir(&url).exists());
}

#[tokio::test]
async fn gc_forgets_the_orphan_date_of_a_url_that_came_back() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let left = at(1_800_000_000);
    harness.cache.collect_garbage(&[], left).unwrap();

    harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), left + GC_GRACE)
        .unwrap();
    harness
        .cache
        .collect_garbage(&[], left + GC_GRACE * 2)
        .unwrap();

    assert_eq!(
        harness.cache.meta(&url).await.unwrap().unwrap().orphaned_at,
        Some(left + GC_GRACE * 2)
    );
}

#[tokio::test]
async fn gc_clears_entries_without_a_readable_meta() {
    let harness = Harness::new();
    let corrupt = url("https://example.com/corrupt.png");
    let missing = url("https://example.com/missing.png");
    for (url, meta) in [(&corrupt, Some("not = [toml")), (&missing, None)] {
        let dir = harness.cache.entry_dir(url);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(blob_name(PNG_START)), PNG_START).unwrap();
        if let Some(text) = meta {
            fs::write(dir.join(META_FILE), text).unwrap();
        }
    }

    let report = harness
        .cache
        .collect_garbage(&[corrupt.clone(), missing.clone()], at(1))
        .unwrap();

    assert_eq!(report.blobs, 2);
    assert_eq!(file_names(&harness.root), Vec::<String>::new());
}

#[tokio::test]
async fn deleting_a_blob_leaves_a_hardlinked_slot_intact() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    let old = harness.publish(&url, PNG_START).await;
    let slot = harness.root.parent().unwrap().join("slot");
    fs::hard_link(&old, &slot).unwrap();
    harness.publish(&url, b"newer").await;

    let report = harness.cache.collect_garbage(&[url], at(1)).unwrap();

    assert_eq!(report.blobs, 1);
    assert!(!old.exists());
    assert_eq!(fs::read(slot).unwrap(), PNG_START);
}

#[tokio::test]
async fn gc_keeps_a_live_temp_when_the_entry_is_expired() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let left = at(1_800_000_000);
    harness.cache.collect_garbage(&[], left).unwrap();
    let mut draft = harness.cache.draft(&url).await.unwrap();
    draft.append(b"next").await.unwrap();
    let temp = harness
        .entry_files(&url)
        .into_iter()
        .find(|name| name.starts_with(".tmp-"))
        .expect("the draft's temp file");

    harness
        .cache
        .collect_garbage(&[], left + GC_GRACE + Duration::from_secs(1))
        .unwrap();

    assert!(
        harness.entry_files(&url).contains(&temp),
        "expired removal deleted the live temp"
    );
    draft.append(b" more").await.unwrap();
    let blob = harness
        .cache
        .publish(&url, draft, "png", Meta::new(url.clone()))
        .await
        .unwrap();
    assert_eq!(fs::read(blob).unwrap(), b"next more");
}

#[tokio::test]
async fn a_failed_orphan_clear_does_not_expire_on_the_old_departure() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let left = at(1_800_000_000);
    harness.cache.collect_garbage(&[], left).unwrap();
    let dir = harness.cache.entry_dir(&url);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let cleared = harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), left + Duration::from_secs(10));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    cleared.unwrap();

    let far = left + GC_GRACE + Duration::from_secs(60);
    harness.cache.collect_garbage(&[], far).unwrap();

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_ne!(meta.orphaned_at, Some(left));
    assert!(harness.cache.entry_dir(&url).exists());
}

/// A fetch read the meta before a collection cleared its departure; its
/// write must not bring the old departure back.
#[tokio::test]
async fn a_fetch_write_keeps_the_orphan_date_that_gc_cleared() {
    let harness = Harness::new();
    let url = url("https://example.com/a.png");
    harness.publish(&url, PNG_START).await;
    let left = at(1_800_000_000);
    harness.cache.collect_garbage(&[], left).unwrap();
    let read_by_fetch = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(read_by_fetch.orphaned_at, Some(left));
    harness
        .cache
        .collect_garbage(std::slice::from_ref(&url), left + Duration::from_secs(10))
        .unwrap();

    harness.cache.record(&url, &read_by_fetch).await.unwrap();
    let recorded = harness.cache.meta(&url).await.unwrap().unwrap();
    let mut draft = harness.cache.draft(&url).await.unwrap();
    draft.append(b"newer").await.unwrap();
    harness
        .cache
        .publish(&url, draft, "png", read_by_fetch)
        .await
        .unwrap();
    let published = harness.cache.meta(&url).await.unwrap().unwrap();

    assert_eq!(recorded.orphaned_at, None);
    assert_eq!(published.orphaned_at, None);
}

#[test]
fn an_unlistable_root_is_not_an_empty_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("remote");
    fs::write(&root, b"not-a-directory").unwrap();

    assert!(Cache::new(root).load().is_err());
}
