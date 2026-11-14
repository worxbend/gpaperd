//! Per-style image catalog: item identity, file stamps and validation state.
//!
//! The scanner (daemon `scan.rs`) and the watcher (daemon `watch.rs`) produce
//! plain [`LocalFile`] and [`FsChange`] values, and the remote cache (daemon
//! `cache.rs`, `fetch.rs`) plain [`RemoteBlob`] values. Only the catalog turns
//! them into [`Item`]s, so identity and de-duplication rules live in one
//! place.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use url::Url;

/// Stable identity of a catalog item: the canonical path of a local file, or
/// the URL of a remote image.
///
/// The same file has the same id in both styles, and so has the same URL. A
/// local id starts with `/` and a remote one with `http`, so the two never
/// meet.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    /// Wraps an id string as is.
    pub fn new(id: impl Into<String>) -> ItemId {
        ItemId(id.into())
    }

    /// The id of a local file. `path` must be canonical. A non-UTF-8 path is
    /// converted lossily; I/O uses [`LocalFile::path`], not the id. The one
    /// exception is the item that the state file names for a slot: before
    /// the first scan of a run, nothing else says where its file is.
    pub fn from_path(path: &Path) -> ItemId {
        ItemId(path.to_string_lossy().into_owned())
    }

    /// The id of a remote image: its URL as written in the config.
    ///
    /// The id holds the whole URL, credentials and query included. Never log
    /// a remote id as it is: log the daemon's `fetch::redacted` form.
    pub fn from_url(url: &Url) -> ItemId {
        ItemId(url.as_str().to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A file's inode identity: `st_dev` and `st_ino`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// What `stat` said about a file. Two equal stamps mean unchanged content,
/// so a stamp names one version of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileStamp {
    pub id: FileId,
    /// Size in bytes.
    pub size: u64,
    pub mtime: SystemTime,
}

/// One image file found by a scan.
///
/// This is the seam between the scanner and the catalog: the scanner fills
/// the fields from `canonicalize` and `stat` and applies no other rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// Absolute and canonical: every symlink resolved.
    pub path: PathBuf,
    /// From `stat` of `path`, so it describes the symlink target.
    pub stamp: FileStamp,
}

/// A remote image as the cache holds it: the seam between the daemon's cache
/// and the catalog.
///
/// The blob's name is the hash of its content, so a blob path names one
/// version of the image, as a [`FileStamp`] does for a local file. A new
/// version comes with a new path; a blob is never rewritten in place.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteBlob {
    /// `http` or `https`, as written in the config.
    pub url: Url,
    /// Absolute: `<cache>/<blake3(url)>/<blake3(content)>.<ext>`.
    pub blob: PathBuf,
}

/// One change to a style's local files, seen after the last full scan.
///
/// This is the seam between the watcher and the catalog. The watcher reports
/// what is on disk per path, over all sources of the style: a path that two
/// sources reach is reported once, and is [`FsChange::Removed`] only when no
/// source reaches it any more. The catalog applies the `(dev, ino)` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsChange {
    /// An image file is at this path with this stamp. Either the path is new,
    /// or the file at a known path changed: edited in place (same inode,
    /// other size or mtime) or replaced by an atomic save (other inode).
    Found(LocalFile),
    /// No image file is at this path any more: deleted, renamed away, or its
    /// source directory was removed or unmounted. Holds the path as an
    /// earlier [`LocalFile::path`] gave it.
    Removed(PathBuf),
}

/// Why an item cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadReason {
    #[error("file is missing")]
    Missing,
    #[error("not a regular file")]
    NotRegularFile,
    /// Holds the OS error text.
    #[error("cannot read file: {0}")]
    Unreadable(String),
    #[error("content is not an image")]
    NotAnImage,
    /// The decode failed and the file was still changing: it is being copied
    /// in. The only reason that is probed again without a change to the
    /// file: see `PROBE_RETRY_DELAY` in [`crate::step`].
    #[error("file is incomplete")]
    Truncated,
    /// glycin cannot decode it: the content is corrupt, or no loader for its
    /// format is installed. Holds glycin's error text.
    #[error("cannot decode image: {0}")]
    Undecodable(String),
    /// Width times height is more than `max_pixels`.
    #[error("{width}x{height} pixels is more than max_pixels allows")]
    TooLarge { width: u32, height: u32 },
}

/// Validation state of an item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Check {
    /// Not probed yet.
    #[default]
    Unchecked,
    /// The last probe passed.
    Usable,
    /// The last probe failed. The item is skipped when candidates are picked,
    /// until its file changes.
    Bad(BadReason),
}

/// Where an item's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A file in one of the style's local sources. The id is its path.
    Local(LocalFile),
    /// A remote URL of the style's sources, downloaded into the cache. The id
    /// is its URL.
    Remote(RemoteBlob),
}

impl Origin {
    /// The file to probe and publish: the local file, or the cached blob.
    pub fn path(&self) -> &Path {
        match self {
            Origin::Local(file) => &file.path,
            Origin::Remote(remote) => &remote.blob,
        }
    }
}

/// A catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: ItemId,
    pub origin: Origin,
    pub check: Check,
}

impl Item {
    /// Whether the item may be picked: every item that is not [`Check::Bad`].
    pub fn is_candidate(&self) -> bool {
        !matches!(self.check, Check::Bad(_))
    }

    /// The version of the item's bytes that the catalog holds.
    pub fn version(&self) -> Version {
        match &self.origin {
            Origin::Local(file) => Version::File(file.stamp),
            Origin::Remote(remote) => Version::Blob(remote.clone()),
        }
    }
}

/// One version of an item's bytes, which a check is about: a local file's
/// stamp, or a remote image's blob, whose name is the hash of its content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Version {
    File(FileStamp),
    Blob(RemoteBlob),
}

/// What a change to a [`Catalog`] did to its items. Both lists are in id
/// order. An item that left the catalog is in neither: nothing follows from
/// that until the item's turn comes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogDelta {
    /// Items under an id that the catalog did not hold before.
    pub added: Vec<ItemId>,
    /// Items whose file has another stamp than before, edited in place or
    /// replaced by another file, and remote items with another blob. Their
    /// check was dropped.
    pub modified: Vec<ItemId>,
}

/// One style's images, keyed by id.
///
/// Invariant: no two items share a [`FileId`], so symlinks, hardlinks and
/// overlapping folders count once. Of the paths to one file, the smallest id
/// is the item, whatever order the files were found in: a catalog that
/// followed changes equals one built from a scan of the same files.
///
/// A check belongs to a version of a file, which is its [`FileStamp`]. It is
/// kept for as long as the catalog holds an item with that stamp, under
/// whatever path, and is dropped when the stamp changes.
///
/// Remote items are one per URL, set apart from the local files by
/// [`Catalog::set_remote`]: a scan or a file change leaves them alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    items: BTreeMap<ItemId, Item>,
    /// Files that are no items, because an item with a smaller id is the same
    /// file. One of them takes over when that item's path goes away.
    aliases: BTreeMap<ItemId, LocalFile>,
}

impl Catalog {
    /// Builds a catalog from scan output, with every item [`Check::Unchecked`].
    ///
    /// The id is [`ItemId::from_path`] of the file's path. When several files
    /// share a [`FileId`], the one with the smallest id is kept, so the result
    /// does not depend on input order.
    pub fn from_local_files(files: Vec<LocalFile>) -> Catalog {
        let mut catalog = Catalog::default();
        catalog.replace(files);
        catalog
    }

    /// Makes the catalog hold exactly the files of a new scan. Duplicates
    /// are allowed, as in [`Catalog::from_local_files`].
    pub fn replace(&mut self, mut files: Vec<LocalFile>) -> CatalogDelta {
        // A path that was scanned twice may have two stamps. The sort makes
        // the same one win whatever order the scanner found them in.
        files.sort_by(|a, b| (&a.path, a.stamp).cmp(&(&b.path, b.stamp)));
        let mut by_id = BTreeMap::new();
        for file in files {
            by_id.entry(ItemId::from_path(&file.path)).or_insert(file);
        }
        self.rebuild(by_id)
    }

    /// Applies what the watcher saw, in order. A path that is already known
    /// with the same stamp, and a removed path that is not known, change
    /// nothing.
    pub fn apply(&mut self, changes: Vec<FsChange>) -> CatalogDelta {
        let mut files = self.aliases.clone();
        files.extend(self.local_files());
        for change in changes {
            match change {
                FsChange::Found(file) => files.insert(ItemId::from_path(&file.path), file),
                FsChange::Removed(path) => files.remove(&ItemId::from_path(&path)),
            };
        }
        self.rebuild(files)
    }

    /// The local file of every item, by id.
    fn local_files(&self) -> impl Iterator<Item = (ItemId, LocalFile)> {
        self.items.values().filter_map(|item| match &item.origin {
            Origin::Local(file) => Some((item.id.clone(), file.clone())),
            Origin::Remote(_) => None,
        })
    }

    /// Derives the items anew from every known file, by id.
    ///
    /// shortcut: this is O(files) per call, which is one batch of changes;
    /// make it incremental when a style holds more than about 100k files.
    fn rebuild(&mut self, files: BTreeMap<ItemId, LocalFile>) -> CatalogDelta {
        let before = mem::take(&mut self.items);
        let checks: BTreeMap<FileStamp, &Check> = before
            .values()
            .filter_map(|item| Some((local_stamp(item)?, &item.check)))
            .collect();
        self.aliases.clear();
        let mut seen_files = BTreeSet::new();
        let mut delta = CatalogDelta::default();
        // In id order, so the smallest id of a file is the one that is kept.
        for (id, file) in files {
            if !seen_files.insert(file.stamp.id) {
                self.aliases.insert(id, file);
                continue;
            }
            match before.get(&id) {
                None => delta.added.push(id.clone()),
                Some(known) if local_stamp(known) != Some(file.stamp) => {
                    delta.modified.push(id.clone());
                }
                Some(_) => {}
            }
            let check = checks
                .get(&file.stamp)
                .map_or(Check::Unchecked, |&check| check.clone());
            let item = Item {
                id: id.clone(),
                origin: Origin::Local(file),
                check,
            };
            self.items.insert(id, item);
        }
        // A remote item is no file: local changes leave it as it is.
        let remotes = before
            .into_iter()
            .filter(|(_, item)| matches!(item.origin, Origin::Remote(_)));
        self.items.extend(remotes);
        delta
    }

    /// Makes the remote items exactly one per URL of `remotes`, each with
    /// its blob. A URL that is listed twice counts once. A check is kept for
    /// as long as its URL keeps the same blob; a URL that is left out leaves
    /// the catalog. Local items stay as they are.
    pub fn set_remote(&mut self, remotes: impl IntoIterator<Item = RemoteBlob>) -> CatalogDelta {
        let wanted: BTreeMap<ItemId, RemoteBlob> = remotes
            .into_iter()
            .map(|remote| (ItemId::from_url(&remote.url), remote))
            .collect();
        self.items
            .retain(|id, item| matches!(item.origin, Origin::Local(_)) || wanted.contains_key(id));
        let mut delta = CatalogDelta::default();
        for (id, remote) in wanted {
            let origin = Origin::Remote(remote);
            match self.items.get(&id) {
                Some(known) if known.origin == origin => continue,
                Some(_) => delta.modified.push(id.clone()),
                None => delta.added.push(id.clone()),
            }
            let item = Item {
                id: id.clone(),
                origin,
                check: Check::Unchecked,
            };
            self.items.insert(id, item);
        }
        delta
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: &ItemId) -> Option<&Item> {
        self.items.get(id)
    }

    /// All items in id order.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.values()
    }

    /// The items that may be picked, in id order.
    pub fn candidates(&self) -> impl Iterator<Item = &Item> {
        self.items().filter(|item| item.is_candidate())
    }

    /// Whether `id` is in this catalog and may be picked.
    pub fn is_candidate(&self, id: &ItemId) -> bool {
        self.get(id).is_some_and(Item::is_candidate)
    }

    /// Records a probe result. An id that is not in this catalog is ignored.
    pub fn set_check(&mut self, id: &ItemId, check: Check) {
        if let Some(item) = self.items.get_mut(id) {
            item.check = check;
        }
    }
}

/// The stamp of a local item. `None` for a remote one.
fn local_stamp(item: &Item) -> Option<FileStamp> {
    match &item.origin {
        Origin::Local(file) => Some(file.stamp),
        Origin::Remote(_) => None,
    }
}
