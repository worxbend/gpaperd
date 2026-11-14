//! Content-addressed cache of remote images.
//!
//! Layout under [`Paths::remote_cache_dir`](crate::paths::Paths):
//!
//! ```text
//! <root>/<blake3(url)>/meta.toml                 the entry's Meta
//! <root>/<blake3(url)>/<blake3(content)>.<ext>   one blob per version
//! <root>/<blake3(url)>/.tmp-<pid>-<n>            a write in progress
//! ```
//!
//! A new version gets a new blob name, so a blob that a slot is hardlinked to
//! is never rewritten. Storing it deletes the entry's blobs older than the
//! version it replaces. Every file is written to a temp file in its own
//! directory, `fsync`ed and renamed, so a crash leaves no partial blob or
//! `meta.toml`; the next garbage collection deletes the temp files it left.
//!
//! `meta.toml` holds the URL as written, credentials and query included, so
//! the directories are `0700` and `meta.toml` is `0600`.
//!
//! The methods that a fetch calls ([`Cache::meta`], [`Cache::record`],
//! [`Cache::draft`], [`Cache::publish`]) are async. The sweeps over the whole
//! cache ([`Cache::load`], [`Cache::collect_garbage`]) are blocking: run them
//! on the blocking pool.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::catalog::RemoteBlob;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::fetch::redacted;

/// The name of each entry's metadata file.
pub const META_FILE: &str = "meta.toml";

/// How long an entry stays after its URL left the config.
pub const GC_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Every temp file name starts with this, then `<pid>-<n>`.
const TEMP_PREFIX: &str = ".tmp-";

/// Makes temp file names unique within the process, so two writes into one
/// entry never share a temp file.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// The temp files that this process has open now. Garbage collection skips
/// them and deletes every other temp file, a leftover of an earlier run
/// that had the same PID included.
static LIVE_TEMPS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// The remote cache rooted at one directory.
///
/// Shared by the fetches in flight and the garbage collector. A
/// [`Cache::publish`] excludes every other write and the collector, so no
/// clean-up deletes a blob whose `meta.toml` is still to be written.
#[derive(Debug)]
pub struct Cache {
    root: PathBuf,
    /// Read-locked by each write into an entry, write-locked by the garbage
    /// collector and by the last step of a publish. Held on the blocking
    /// pool only.
    sweep: Arc<RwLock<()>>,
}

/// The content of an entry's `meta.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// The URL as written in the config. Two URLs with the same hash are
    /// told apart by it.
    pub url: Url,
    /// The `ETag` of the cached version, sent back as `If-None-Match`.
    pub etag: Option<String>,
    /// The `Last-Modified` of the cached version, sent back as
    /// `If-Modified-Since`.
    pub last_modified: Option<String>,
    /// The blob's file name in the entry directory. `None` until a first
    /// version is stored.
    pub blob: Option<String>,
    /// The image MIME type that the blob's magic bytes gave.
    pub mime: Option<String>,
    /// When the server last confirmed or replaced the cached version.
    pub fetched_at: Option<SystemTime>,
    /// Why the last fetch failed. `None` after one that worked.
    pub last_error: Option<String>,
    /// The server answered `404` or `410`. The fetcher warns only when this
    /// turns true.
    pub gone: bool,
    /// When a garbage collection first found the URL in no config. Cleared
    /// when the URL is back. Not in the plan's field list: it makes "left
    /// the config more than seven days ago" survive a restart.
    pub orphaned_at: Option<SystemTime>,
}

impl Meta {
    /// The metadata of a URL that has no stored version yet.
    pub fn new(url: Url) -> Meta {
        Meta {
            url,
            etag: None,
            last_modified: None,
            blob: None,
            mime: None,
            fetched_at: None,
            last_error: None,
            gone: false,
            orphaned_at: None,
        }
    }
}

/// A new version of a blob on its way in: a temp file in the entry
/// directory, with a running hash and size count.
///
/// Dropping a draft that was not published deletes its temp file, which is
/// what a rejected body and `ENOSPC` need.
#[derive(Debug)]
pub struct BlobDraft {
    // Declared before `file`, so the name goes before the handle closes.
    temp: TempFile,
    file: tokio::fs::File,
    hasher: blake3::Hasher,
    len: u64,
}

/// What one garbage collection deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Entries whose URL was in no config for longer than [`GC_GRACE`].
    pub entries: usize,
    /// Blobs that no `meta.toml` named, and stray temp files.
    pub blobs: usize,
}

impl Cache {
    /// A cache rooted at `root`, which need not exist yet. Touches nothing.
    pub fn new(root: PathBuf) -> Cache {
        Cache {
            root,
            sweep: Arc::default(),
        }
    }

    /// `<root>/<blake3(url)>`: the entry directory of `url`.
    pub fn entry_dir(&self, url: &Url) -> PathBuf {
        self.root.join(url_hash(url))
    }

    /// Blocking. Every entry whose `meta.toml` reads and whose blob is a
    /// regular file. A missing root is an empty cache. An entry that cannot
    /// be read is logged and left out; nothing is deleted here.
    pub fn load(&self) -> Vec<RemoteBlob> {
        let dirs = match entry_dirs(&self.root) {
            Ok(dirs) => dirs,
            Err(error) => {
                tracing::warn!(reason = %error, "remote cache not read: starting without it");
                return Vec::new();
            }
        };
        let mut blobs: Vec<RemoteBlob> = dirs.iter().filter_map(|dir| loaded_blob(dir)).collect();
        blobs.sort();
        blobs
    }

    /// The `meta.toml` of `url`. `None` when there is no entry, or its
    /// `meta.toml` names another URL (a hash collision).
    ///
    /// # Errors
    ///
    /// A `meta.toml` that cannot be read or parsed.
    pub async fn meta(&self, url: &Url) -> io::Result<Option<Meta>> {
        let path = self.entry_dir(url).join(META_FILE);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = decode(&text)?;
        Ok((meta.url == *url).then_some(meta))
    }

    /// Writes `meta` as `url`'s `meta.toml`: temp file, `fsync`, rename.
    /// Creates the entry directory when needed. For a `304`, a `404`, or a
    /// failure, where no blob changes.
    ///
    /// # Errors
    ///
    /// Any I/O error. The old `meta.toml` stays as it was.
    pub async fn record(&self, url: &Url, meta: &Meta) -> io::Result<()> {
        let dir = self.entry_dir(url);
        let text = encode(meta)?;
        let sweep = Arc::clone(&self.sweep);
        // On the blocking pool, so that a cancelled fetch still finishes or
        // cleans up the write.
        blocking(move || {
            let _sweep = sweep.read().unwrap_or_else(PoisonError::into_inner);
            create_entry_dir(&dir)?;
            write_durably(&dir, META_FILE, text.as_bytes())
        })
        .await
    }

    /// Opens a temp file for a new version of `url`'s blob in its entry
    /// directory, which is created when needed.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub async fn draft(&self, url: &Url) -> io::Result<BlobDraft> {
        let dir = self.entry_dir(url);
        let sweep = Arc::clone(&self.sweep);
        // One hop under the sweep lock: a garbage collection between the
        // two would delete the new, still empty directory.
        let (temp, file) = blocking(move || {
            let _sweep = sweep.read().unwrap_or_else(PoisonError::into_inner);
            create_entry_dir(&dir)?;
            let temp = TempFile::new_in(&dir);
            let file = File::create_new(&temp.path)?;
            Ok((temp, file))
        })
        .await?;
        Ok(BlobDraft {
            temp,
            file: tokio::fs::File::from_std(file),
            hasher: blake3::Hasher::new(),
            len: 0,
        })
    }

    /// Makes `draft` the current version of `url`: `fsync`s it, renames it
    /// to `<blake3(content)>.<extension>`, then writes `meta` naming that
    /// blob as [`Cache::record`] does. Returns the blob's absolute path.
    ///
    /// A blob of that name holds the same bytes already, so it is kept and
    /// the draft is dropped. The blob that the old `meta.toml` named stays:
    /// a probe, a publish or the background key may still read it. Older
    /// blobs of the entry are deleted.
    ///
    /// # Errors
    ///
    /// Any I/O error, `ENOSPC` included. The temp file is gone and the
    /// earlier blob and `meta.toml` stay as they were.
    pub async fn publish(
        &self,
        url: &Url,
        draft: BlobDraft,
        extension: &str,
        mut meta: Meta,
    ) -> io::Result<PathBuf> {
        let BlobDraft {
            temp,
            mut file,
            hasher,
            ..
        } = draft;
        // tokio reports a failed write at the next operation: flush to see it.
        file.flush().await?;
        let file = file.into_std().await;
        let name = format!("{}.{extension}", hasher.finalize().to_hex());
        meta.blob = Some(name.clone());
        let text = encode(&meta)?;
        let dir = self.entry_dir(url);
        let sweep = Arc::clone(&self.sweep);
        blocking(move || {
            file.sync_all()?;
            // Exclusive: a publish of the same URL running alongside could
            // otherwise lose its new blob to this one's clean-up.
            let _sweep = sweep.write().unwrap_or_else(PoisonError::into_inner);
            install_blob(&dir, temp, &name, text.as_bytes())
        })
        .await
    }

    /// Blocking. Deletes the entry of every URL that is not in `keep` and
    /// whose `orphaned_at` lies more than [`GC_GRACE`] before `now`, and
    /// every blob or stray temp file in an entry that its `meta.toml` does
    /// not name. Sets `orphaned_at` to `now` on an entry that just left the
    /// config, and clears it on one that is back.
    ///
    /// An entry whose `meta.toml` is missing, corrupt or in the wrong
    /// directory names no blob: its files go, and so does the directory once
    /// it is empty. One whose `meta.toml` cannot be read for another reason
    /// is left alone. Temp files of this process's writes in progress stay.
    ///
    /// A slot hardlinked to a deleted blob keeps its data.
    ///
    /// # Errors
    ///
    /// An I/O error that stopped the walk. A single file that cannot be
    /// deleted is logged and skipped.
    pub fn collect_garbage(&self, keep: &[Url], now: SystemTime) -> io::Result<GcReport> {
        let _sweep = self.sweep.write().unwrap_or_else(PoisonError::into_inner);
        let mut report = GcReport::default();
        for dir in entry_dirs(&self.root)? {
            collect_entry(&dir, keep, now, &mut report);
        }
        Ok(report)
    }
}

impl BlobDraft {
    /// Appends `chunk` to the temp file, the hash and the size count.
    ///
    /// # Errors
    ///
    /// Any I/O error, `ENOSPC` included.
    pub async fn append(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk).await?;
        self.hasher.update(chunk);
        self.len += chunk.len() as u64;
        Ok(())
    }

    /// Bytes appended so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A temp file name that is deleted when dropped. After a rename the delete
/// finds nothing, which is fine.
#[derive(Debug)]
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new_in(dir: &Path) -> TempFile {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{TEMP_PREFIX}{}-{n}", process::id()));
        live_temps().insert(path.clone());
        TempFile { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort: the file is gone after a rename, and a leftover is
        // deleted by the next garbage collection.
        let _ = fs::remove_file(&self.path);
        live_temps().remove(&self.path);
    }
}

fn live_temps() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    LIVE_TEMPS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The serde shape of `meta.toml`. Times are RFC 3339, so the file reads well.
#[derive(Debug, Serialize, Deserialize)]
struct MetaFile {
    url: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(default)]
    blob: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    fetched_at: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    gone: bool,
    #[serde(default)]
    orphaned_at: Option<String>,
}

fn encode(meta: &Meta) -> io::Result<String> {
    let file = MetaFile {
        url: meta.url.to_string(),
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
        blob: meta.blob.clone(),
        mime: meta.mime.clone(),
        fetched_at: meta.fetched_at.map(format_time),
        last_error: meta.last_error.clone(),
        gone: meta.gone,
        orphaned_at: meta.orphaned_at.map(format_time),
    };
    toml::to_string(&file).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Parses a `meta.toml`. The file lives in the user's cache, so everything
/// that later becomes a path or a request is checked here.
fn decode(text: &str) -> io::Result<Meta> {
    let file: MetaFile = toml::from_str(text).map_err(invalid)?;
    let url = Url::parse(&file.url).map_err(invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "{} is not an http or https URL",
            url.scheme()
        )));
    }
    if let Some(blob) = &file.blob
        && !is_blob_name(blob)
    {
        return Err(invalid(format!("{blob:?} is not a blob name")));
    }
    Ok(Meta {
        url,
        etag: file.etag,
        last_modified: file.last_modified,
        blob: file.blob,
        mime: file.mime,
        fetched_at: file.fetched_at.as_deref().map(parse_time).transpose()?,
        last_error: file.last_error,
        gone: file.gone,
        orphaned_at: file.orphaned_at.as_deref().map(parse_time).transpose()?,
    })
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// A plain file name in the entry directory: not a path, not hidden (temp
/// files are), not the metadata file.
fn is_blob_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\0']) && name != META_FILE
}

/// Times before the epoch only come from a broken clock; they are written as
/// the epoch.
fn format_time(time: SystemTime) -> String {
    humantime::format_rfc3339_nanos(time.max(UNIX_EPOCH)).to_string()
}

fn parse_time(text: &str) -> io::Result<SystemTime> {
    humantime::parse_rfc3339(text).map_err(invalid)
}

fn url_hash(url: &Url) -> String {
    blake3::hash(url.as_str().as_bytes()).to_hex().to_string()
}

/// The subdirectories of `root`. A missing root has none.
fn entry_dirs(root: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// The `meta.toml` in `dir`, if it is there and belongs there: its URL
/// hashes to the directory's name.
fn entry_meta(dir: &Path) -> io::Result<Option<Meta>> {
    let text = match fs::read_to_string(dir.join(META_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let meta = decode(&text)?;
    if dir.file_name() != Some(url_hash(&meta.url).as_ref()) {
        return Err(invalid("its URL belongs in another directory"));
    }
    Ok(Some(meta))
}

/// The blob of the entry in `dir`, for [`Cache::load`]. Logs why there is
/// none when that is unexpected.
fn loaded_blob(dir: &Path) -> Option<RemoteBlob> {
    let meta = match entry_meta(dir) {
        Ok(meta) => meta?,
        Err(error) => {
            tracing::warn!(entry = %dir.display(), reason = %error, "remote cache entry skipped");
            return None;
        }
    };
    let blob = dir.join(meta.blob.as_deref()?);
    // Not followed: a blob is always a regular file that the cache wrote.
    match fs::symlink_metadata(&blob) {
        Ok(stat) if stat.is_file() => Some(RemoteBlob {
            url: meta.url,
            blob,
        }),
        _ => {
            tracing::warn!(
                url = %redacted(&meta.url),
                "cached image is missing: it is fetched again"
            );
            None
        }
    }
}

/// Creates `dir` and its missing parents, readable by the user only.
fn create_entry_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Writes `bytes` to `dir/name`, readable by the user only: temp file in
/// `dir`, `fsync`, rename.
fn write_durably(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = TempFile::new_in(dir);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp.path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp.path, dir.join(name))
}

/// Gives the draft in `temp` its blob name `name` in `dir`, then writes the
/// `meta.toml` in `meta_text`. A new blob is deleted again when the
/// `meta.toml` cannot be written. Then every blob but this one and the one
/// the old `meta.toml` named is deleted, so a URL whose content changes on
/// each fetch does not fill the disk before the next garbage collection.
fn install_blob(dir: &Path, temp: TempFile, name: &str, meta_text: &[u8]) -> io::Result<PathBuf> {
    // When it cannot be read, nothing is deleted: garbage collection tells.
    let previous = entry_meta(dir).map(|meta| meta.and_then(|meta| meta.blob));
    let blob = dir.join(name);
    // Same name, same bytes: the blob that is there stays, so a slot or a
    // probe reading it sees no change.
    let is_new = !blob.try_exists()?;
    if is_new {
        fs::rename(&temp.path, &blob)?;
    }
    drop(temp);
    write_durably(dir, META_FILE, meta_text).inspect_err(|_| {
        if is_new {
            // Best effort: an unnamed blob is garbage collected anyway.
            let _ = fs::remove_file(&blob);
        }
    })?;
    if let Ok(previous) = previous {
        let named: Vec<&str> = [Some(META_FILE), Some(name), previous.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        delete_unnamed(dir, &named);
    }
    Ok(blob)
}

/// One entry's share of [`Cache::collect_garbage`].
fn collect_entry(dir: &Path, keep: &[Url], now: SystemTime, report: &mut GcReport) {
    let meta = match entry_meta(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            tracing::warn!(entry = %dir.display(), reason = %error, "remote cache entry is corrupt: deleting its files");
            None
        }
        Err(error) => {
            tracing::warn!(entry = %dir.display(), reason = %error, "remote cache entry not read: left for the next collection");
            return;
        }
    };
    let Some(mut meta) = meta else {
        report.blobs += delete_unnamed(dir, &[]);
        // Fails while a write of this process is in progress: it stays.
        let _ = fs::remove_dir(dir);
        return;
    };
    if is_expired(&meta, keep, now) {
        match fs::remove_dir_all(dir) {
            Ok(()) => report.entries += 1,
            Err(error) => {
                tracing::warn!(url = %redacted(&meta.url), reason = %error, "old remote cache entry not deleted")
            }
        }
        return;
    }
    update_orphaned_at(dir, &mut meta, keep, now);
    let named: Vec<&str> = [Some(META_FILE), meta.blob.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    report.blobs += delete_unnamed(dir, &named);
}

fn is_expired(meta: &Meta, keep: &[Url], now: SystemTime) -> bool {
    !keep.contains(&meta.url)
        && meta
            .orphaned_at
            .and_then(|since| now.duration_since(since).ok())
            .is_some_and(|age| age > GC_GRACE)
}

/// Starts the grace period of a URL that left the config, or ends it for
/// one that is back, and writes the change.
fn update_orphaned_at(dir: &Path, meta: &mut Meta, keep: &[Url], now: SystemTime) {
    let orphaned_at = if keep.contains(&meta.url) {
        None
    } else {
        Some(meta.orphaned_at.unwrap_or(now))
    };
    if orphaned_at == meta.orphaned_at {
        return;
    }
    meta.orphaned_at = orphaned_at;
    let written = encode(meta).and_then(|text| write_durably(dir, META_FILE, text.as_bytes()));
    if let Err(error) = written {
        tracing::warn!(url = %redacted(&meta.url), reason = %error, "remote cache entry not updated");
    }
}

/// Deletes every file in `dir` but those in `named` and this process's temp
/// files in use. Returns how many files went, a `meta.toml` not counted.
fn delete_unnamed(dir: &Path, named: &[&str]) -> usize {
    let files = match fs::read_dir(dir) {
        Ok(files) => files,
        Err(error) => {
            tracing::warn!(entry = %dir.display(), reason = %error, "remote cache entry not swept");
            return 0;
        }
    };
    let mut deleted = 0;
    for file in files.flatten() {
        let name = file.file_name();
        let name = name.to_string_lossy();
        if named.contains(&name.as_ref()) || live_temps().contains(&file.path()) {
            continue;
        }
        match fs::remove_file(file.path()) {
            Ok(()) if name == META_FILE => {}
            Ok(()) => deleted += 1,
            Err(error) => {
                tracing::warn!(file = %file.path().display(), reason = %error, "remote cache file not deleted")
            }
        }
    }
    deleted
}

/// Runs `work` on the blocking pool. A panic there is re-raised here.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    match tokio::task::spawn_blocking(work).await {
        Ok(outcome) => outcome,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn full_meta() -> Meta {
        Meta {
            url: url("https://example.com/a b.jpg?x=1"),
            etag: Some("\"v1\"".into()),
            last_modified: Some("Sun, 06 Nov 1994 08:49:37 GMT".into()),
            blob: Some("abc.jpg".into()),
            mime: Some("image/jpeg".into()),
            fetched_at: Some(UNIX_EPOCH + Duration::new(1_800_000_000, 123)),
            last_error: Some("HTTP 503".into()),
            gone: true,
            orphaned_at: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }

    #[test]
    fn meta_round_trips_through_toml() {
        assert_eq!(decode(&encode(&full_meta()).unwrap()).unwrap(), full_meta());
    }

    #[test]
    fn new_meta_round_trips_through_toml() {
        let meta = Meta::new(url("http://example.com/x.png"));

        assert_eq!(decode(&encode(&meta).unwrap()).unwrap(), meta);
    }

    #[test]
    fn meta_with_a_path_for_a_blob_is_rejected() {
        for blob in ["../../etc/passwd", "a/b.jpg", ".tmp-1-2", "meta.toml", ""] {
            let text = format!("url = \"https://example.com/a.jpg\"\nblob = {blob:?}\n");

            assert!(decode(&text).is_err(), "{blob:?} accepted");
        }
    }

    #[test]
    fn meta_with_a_file_url_is_rejected() {
        assert!(decode("url = \"file:///etc/passwd\"\n").is_err());
    }

    #[test]
    fn time_before_the_epoch_is_written_as_the_epoch() {
        assert_eq!(
            format_time(UNIX_EPOCH - Duration::from_secs(5)),
            "1970-01-01T00:00:00.000000000Z"
        );
    }

    #[test]
    fn entry_dir_is_the_blake3_of_the_url() {
        let cache = Cache::new("/cache".into());
        let target = url("https://example.com/a.jpg");

        let expected = blake3::hash(b"https://example.com/a.jpg").to_hex();
        assert_eq!(
            cache.entry_dir(&target),
            Path::new("/cache").join(expected.as_str())
        );
    }

    /// `ENOSPC` mid-write: `/dev/full` stands in for the temp file's
    /// handle, so every write fails as on a full disk.
    #[tokio::test]
    async fn a_full_disk_mid_write_deletes_the_temp_file_and_keeps_the_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("remote"));
        let target = url("https://example.com/a.png");
        let mut draft = cache.draft(&target).await.unwrap();
        draft.append(b"old").await.unwrap();
        let old = cache
            .publish(&target, draft, "png", Meta::new(target.clone()))
            .await
            .unwrap();
        let entry = cache.entry_dir(&target);
        let meta_before = fs::read(entry.join(META_FILE)).unwrap();
        let mut draft = cache.draft(&target).await.unwrap();
        let temp = draft.temp.path.clone();
        let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
        draft.file = tokio::fs::File::from_std(full);

        let outcome = async {
            draft.append(&[0; 64 * 1024]).await?;
            cache
                .publish(&target, draft, "png", Meta::new(target.clone()))
                .await
        }
        .await;

        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert!(!temp.exists(), "{temp:?}");
        assert_eq!(fs::read(&old).unwrap(), b"old");
        assert_eq!(fs::read(entry.join(META_FILE)).unwrap(), meta_before);
        let mut names: Vec<_> = fs::read_dir(&entry)
            .unwrap()
            .map(|file| file.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [old.file_name().unwrap(), META_FILE.as_ref()]);
    }

    #[test]
    fn expiry_needs_more_than_the_grace_period() {
        let since = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut meta = Meta::new(url("https://example.com/a.jpg"));
        meta.orphaned_at = Some(since);

        assert!(!is_expired(&meta, &[], since + GC_GRACE));
        assert!(is_expired(
            &meta,
            &[],
            since + GC_GRACE + Duration::from_secs(1)
        ));
        assert!(!is_expired(
            &meta,
            &[meta.url.clone()],
            since + GC_GRACE * 2
        ));
        assert!(!is_expired(&meta, &[], since - Duration::from_secs(1)));
    }
}
