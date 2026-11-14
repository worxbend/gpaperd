//! Scan of a style's local sources: at start-up, and again by
//! [`crate::watch`] whenever a source may have changed.
//!
//! A scan only lists and `stat`s; it opens no file. Content is checked later,
//! just before use, by [`crate::probe`]. Filters are the plan's "Sources,
//! catalog and ordering" section.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gpaperd_core::catalog::{FileId, FileStamp, LocalFile};
use gpaperd_core::config::{DirOptions, Source};
use walkdir::{DirEntry, WalkDir};

/// Deepest level scanned below a directory source. Its direct entries are
/// level 1.
const MAX_DEPTH: usize = 16;

/// Extensions of the formats gnome-shell can show, in lower case.
///
/// In-progress names (`.part`, `.crdownload`, `.tmp`, a trailing `~`) end in
/// none of these, so this list is also what keeps half-written files out.
const IMAGE_EXTENSIONS: [&str; 13] = [
    "jpg", "jpeg", "png", "webp", "jxl", "avif", "heic", "heif", "tif", "tiff", "bmp", "gif", "qoi",
];

/// What a scan found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scan {
    /// Image files in discovery order. A file reached twice (overlapping
    /// sources, symlinks) appears twice; the catalog removes duplicates.
    pub files: Vec<LocalFile>,
    /// Directories whose entries the scan listed, in discovery order: each
    /// source directory and, with `recursive`, the directories below it down
    /// to the depth limit. This is what a watch of the sources has to cover.
    /// The paths are as the walk built them, not canonical. A directory that
    /// could not be listed is here too, next to its warning.
    pub dirs: Vec<PathBuf>,
    /// Sources and entries that were skipped, for the log.
    pub warnings: Vec<ScanWarning>,
}

/// A source or entry the scan could not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanWarning {
    /// The source or entry.
    pub path: PathBuf,
    pub reason: String,
}

/// Scans the local sources of one style. Blocking I/O. A problem with one
/// source or entry becomes a [`ScanWarning`]; the scan itself never fails.
///
/// - [`Source::Url`] is skipped without a warning: the remote cache, not the
///   scan, provides its images.
/// - [`Source::Path`] is a directory or a file, decided now; a directory is
///   scanned with `DirOptions::default()`.
/// - A path that does not exist yields a warning and no files. So does a
///   `dir` that is no directory and a `file` that is no regular file.
/// - A file named directly is taken whatever its name. The filters below
///   apply to directory entries only.
/// - Directory entries: regular files, after following symlinks when
///   `follow_symlinks` is set, with an extension from the plan's list
///   (compared without case). Hidden entries are skipped unless `hidden` is
///   set. In-progress names (`.part`, `.crdownload`, `.tmp`, a trailing `~`)
///   are skipped. Subdirectories are entered only with `recursive`, at most
///   16 levels deep.
/// - An entry that cannot be read (permission denied, dangling symlink,
///   symlink loop) yields one warning and the scan goes on.
///
/// Every [`LocalFile`] has a canonical path and a stamp from `stat` of that
/// path.
pub fn scan(sources: &[Source]) -> Scan {
    let mut found = Scan::default();
    for source in sources {
        match source {
            Source::Path(path) => found.add_path(path),
            Source::Dir { path, options } => found.add_dir(path, *options),
            Source::File(path) => found.add_file(path),
            Source::Url(_) => {}
        }
    }
    found
}

impl Scan {
    fn add_path(&mut self, path: &Path) {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => self.walk(path, DirOptions::default()),
            Ok(_) => self.add_file(path),
            Err(err) => self.warn(path, err),
        }
    }

    fn add_dir(&mut self, dir: &Path, options: DirOptions) {
        match fs::metadata(dir) {
            Ok(metadata) if metadata.is_dir() => self.walk(dir, options),
            Ok(_) => self.warn(dir, "not a directory"),
            Err(err) => self.warn(dir, err),
        }
    }

    fn add_file(&mut self, path: &Path) {
        match local_file(path) {
            Ok(file) => self.files.push(file),
            Err(err) => self.warn(path, err),
        }
    }

    fn walk(&mut self, dir: &Path, options: DirOptions) {
        let max_depth = if options.recursive { MAX_DEPTH } else { 1 };
        self.dirs.push(dir.to_owned());
        let entries = WalkDir::new(dir)
            .min_depth(1)
            .max_depth(max_depth)
            .follow_links(options.follow_symlinks)
            .into_iter()
            .filter_entry(|entry| options.hidden || !is_hidden(entry));
        for entry in entries {
            match entry {
                Ok(entry) if is_image_file(&entry) => self.add_file(entry.path()),
                Ok(entry) if is_listed_dir(&entry, max_depth) => self.dirs.push(entry.into_path()),
                Ok(_) => {}
                Err(err) => self.warn(err.path().unwrap_or(dir), walk_failure(&err)),
            }
        }
    }

    fn warn(&mut self, path: &Path, reason: impl fmt::Display) {
        self.warnings.push(ScanWarning {
            path: path.to_owned(),
            reason: reason.to_string(),
        });
    }
}

fn local_file(path: &Path) -> io::Result<LocalFile> {
    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let stamp = file_stamp(&metadata)?;
    Ok(LocalFile { path, stamp })
}

/// The version of a file that a `stat` result describes.
pub(crate) fn file_stamp(metadata: &Metadata) -> io::Result<FileStamp> {
    Ok(FileStamp {
        id: file_id(metadata),
        size: metadata.len(),
        mtime: metadata.modified()?,
    })
}

/// The inode that a `stat` result describes.
pub(crate) fn file_id(metadata: &Metadata) -> FileId {
    FileId {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry.file_name().as_encoded_bytes().starts_with(b".")
}

fn is_image_file(entry: &DirEntry) -> bool {
    let has_image_extension = entry
        .path()
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    has_image_extension && entry.file_type().is_file()
}

/// Whether the walk lists the entries of this entry: a directory above the
/// depth limit. With `follow_symlinks`, a symlink to one counts.
fn is_listed_dir(entry: &DirEntry, max_depth: usize) -> bool {
    entry.file_type().is_dir() && entry.depth() < max_depth
}

/// The reason without the path, which the warning already carries.
fn walk_failure(err: &walkdir::Error) -> String {
    match err.io_error() {
        Some(io_err) => io_err.to_string(),
        // Only a symlink loop has no I/O error; walkdir's text names both ends.
        None => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use gpaperd_core::config::{Config, ParseContext};
    use tempfile::TempDir;
    use url::Url;

    use super::*;

    /// A temp dir and its canonical path, which is what scanned paths start
    /// with even when the temp root sits behind a symlink.
    fn temp_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical temp dir");
        (dir, root)
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("fixture has a parent")).expect("create dirs");
        fs::write(path, b"x").expect("write fixture");
    }

    fn dir_source(path: &Path, options: DirOptions) -> Source {
        Source::Dir {
            path: path.to_owned(),
            options,
        }
    }

    fn recursive() -> DirOptions {
        DirOptions {
            recursive: true,
            ..DirOptions::default()
        }
    }

    /// Found paths relative to `root`, sorted, because readdir order is not
    /// stable.
    fn relative_paths(root: &Path, found: &Scan) -> Vec<String> {
        let mut names: Vec<String> = found
            .files
            .iter()
            .map(|file| {
                let relative = file.path.strip_prefix(root).expect("file is under root");
                relative
                    .to_str()
                    .expect("fixture names are UTF-8")
                    .to_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn directory_scan_keeps_only_image_extensions() {
        let (_dir, root) = temp_root();
        let images = [
            "a.jpg", "b.JPEG", "c.Png", "d.webp", "e.jxl", "f.avif", "g.heic", "h.heif", "i.tif",
            "j.tiff", "k.bmp", "l.gif", "m.qoi",
        ];
        let others = [
            "notes.txt",
            "README",
            "clip.mp4",
            "logo.svg",
            "png",
            "raw.cr2",
        ];
        for name in images.iter().chain(&others) {
            touch(&root.join(name));
        }

        let found = scan(&[dir_source(&root, DirOptions::default())]);

        assert_eq!(relative_paths(&root, &found), images);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn in_progress_names_are_skipped() {
        let (_dir, root) = temp_root();
        for name in [
            "done.png",
            "a.png.part",
            "b.jpg.crdownload",
            "c.png.tmp",
            "d.png~",
        ] {
            touch(&root.join(name));
        }

        let found = scan(&[dir_source(&root, DirOptions::default())]);

        assert_eq!(relative_paths(&root, &found), ["done.png"]);
    }

    #[test]
    fn hidden_entries_are_skipped_by_default() {
        let (_dir, root) = temp_root();
        touch(&root.join("shown.png"));
        touch(&root.join(".hidden.png"));
        touch(&root.join(".cache/thumb.png"));

        let found = scan(&[dir_source(&root, recursive())]);

        assert_eq!(relative_paths(&root, &found), ["shown.png"]);
    }

    #[test]
    fn hidden_entries_are_scanned_when_asked() {
        let (_dir, root) = temp_root();
        touch(&root.join("shown.png"));
        touch(&root.join(".hidden.png"));
        touch(&root.join(".cache/thumb.png"));
        let options = DirOptions {
            hidden: true,
            ..recursive()
        };

        let found = scan(&[dir_source(&root, options)]);

        assert_eq!(
            relative_paths(&root, &found),
            [".cache/thumb.png", ".hidden.png", "shown.png"]
        );
    }

    #[test]
    fn hidden_source_dir_is_scanned_when_named() {
        let (_dir, root) = temp_root();
        touch(&root.join(".walls/a.png"));

        let found = scan(&[dir_source(&root.join(".walls"), DirOptions::default())]);

        assert_eq!(relative_paths(&root, &found), [".walls/a.png"]);
    }

    #[test]
    fn subdirectories_are_entered_only_with_recursive() {
        let (_dir, root) = temp_root();
        touch(&root.join("top.png"));
        touch(&root.join("nature/forest.png"));
        touch(&root.join("nature/sea/wave.png"));

        let flat = scan(&[dir_source(&root, DirOptions::default())]);
        let deep = scan(&[dir_source(&root, recursive())]);

        assert_eq!(relative_paths(&root, &flat), ["top.png"]);
        assert_eq!(
            relative_paths(&root, &deep),
            ["nature/forest.png", "nature/sea/wave.png", "top.png"]
        );
    }

    #[test]
    fn recursion_stops_sixteen_levels_down() {
        let (_dir, root) = temp_root();
        let level_15_dir: PathBuf = (1..=15).map(|level| format!("d{level}")).collect();
        touch(&root.join(&level_15_dir).join("level16.png"));
        touch(&root.join(&level_15_dir).join("d16/level17.png"));

        let found = scan(&[dir_source(&root, recursive())]);

        let level_16_file = level_15_dir.join("level16.png");
        assert_eq!(
            relative_paths(&root, &found),
            [level_16_file.to_str().expect("UTF-8")]
        );
    }

    #[test]
    fn listed_directories_are_the_source_and_with_recursive_those_below_it() {
        let (_dir, root) = temp_root();
        let walls = root.join("walls");
        touch(&walls.join("top.png"));
        touch(&walls.join("nature/sea/wave.png"));
        touch(&walls.join(".cache/thumb.png"));
        touch(&root.join("hero.png"));

        let flat = scan(&[dir_source(&walls, DirOptions::default())]);
        let deep = scan(&[dir_source(&walls, recursive())]);
        let file = scan(&[Source::File(root.join("hero.png"))]);

        assert_eq!(flat.dirs, deep.dirs[..1]);
        assert_eq!(
            deep.dirs,
            [
                walls.clone(),
                walls.join("nature"),
                walls.join("nature/sea")
            ]
        );
        assert_eq!(file.dirs, Vec::<PathBuf>::new());
    }

    #[test]
    fn directory_at_the_depth_limit_is_not_listed() {
        let (_dir, root) = temp_root();
        let level_15_dir: PathBuf = (1..=15).map(|level| format!("d{level}")).collect();
        touch(&root.join(&level_15_dir).join("d16/level17.png"));

        let found = scan(&[dir_source(&root, recursive())]);

        // The source and the 15 levels below it: what the scan lists.
        assert_eq!(found.dirs.len(), 16);
        assert_eq!(found.dirs.last(), Some(&root.join(level_15_dir)));
    }

    #[test]
    fn symlink_loop_ends_with_one_warning() {
        let (_dir, root) = temp_root();
        touch(&root.join("walls/a.png"));
        symlink(root.join("walls"), root.join("walls/again")).expect("create symlink");

        let found = scan(&[dir_source(&root.join("walls"), recursive())]);

        assert_eq!(relative_paths(&root, &found), ["walls/a.png"]);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert_eq!(found.warnings[0].path, root.join("walls/again"));
    }

    #[test]
    fn dangling_symlink_is_skipped_with_one_warning() {
        let (_dir, root) = temp_root();
        touch(&root.join("a.png"));
        symlink(root.join("gone.png"), root.join("dangling.png")).expect("create symlink");

        let found = scan(&[dir_source(&root, DirOptions::default())]);

        assert_eq!(relative_paths(&root, &found), ["a.png"]);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert_eq!(found.warnings[0].path, root.join("dangling.png"));
    }

    #[test]
    fn symlinks_are_followed_to_canonical_paths_by_default() {
        let (_dir, root) = temp_root();
        touch(&root.join("store/real.png"));
        touch(&root.join("store/album/b.png"));
        fs::create_dir(root.join("walls")).expect("create dir");
        symlink(root.join("store/real.png"), root.join("walls/link.png")).expect("symlink");
        symlink(root.join("store/album"), root.join("walls/album")).expect("symlink");

        let found = scan(&[dir_source(&root.join("walls"), recursive())]);

        assert_eq!(
            relative_paths(&root, &found),
            ["store/album/b.png", "store/real.png"]
        );
    }

    #[test]
    fn symlinks_are_ignored_when_follow_symlinks_is_off() {
        let (_dir, root) = temp_root();
        touch(&root.join("store/real.png"));
        touch(&root.join("store/album/b.png"));
        touch(&root.join("walls/own.png"));
        symlink(root.join("store/real.png"), root.join("walls/link.png")).expect("symlink");
        symlink(root.join("store/album"), root.join("walls/album")).expect("symlink");
        symlink(root.join("gone.png"), root.join("walls/dangling.png")).expect("symlink");
        let options = DirOptions {
            follow_symlinks: false,
            ..recursive()
        };

        let found = scan(&[dir_source(&root.join("walls"), options)]);

        assert_eq!(relative_paths(&root, &found), ["walls/own.png"]);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn missing_path_warns_and_yields_no_files() {
        let (_dir, root) = temp_root();
        let missing = root.join("not-there");
        let sources = [
            Source::Path(missing.clone()),
            dir_source(&missing, DirOptions::default()),
            Source::File(missing.clone()),
        ];

        let found = scan(&sources);

        assert!(found.files.is_empty(), "{:?}", found.files);
        let warned: Vec<&Path> = found.warnings.iter().map(|w| w.path.as_path()).collect();
        assert_eq!(warned, [&missing, &missing, &missing]);
    }

    #[test]
    fn plain_path_to_a_directory_is_scanned_with_default_options() {
        let (_dir, root) = temp_root();
        touch(&root.join("top.png"));
        touch(&root.join(".hidden.png"));
        touch(&root.join("sub/deep.png"));

        let found = scan(&[Source::Path(root.clone())]);

        assert_eq!(relative_paths(&root, &found), ["top.png"]);
    }

    #[test]
    fn plain_path_to_a_file_is_taken_whatever_its_name() {
        let (_dir, root) = temp_root();
        touch(&root.join(".wallpaper.tmp"));

        let found = scan(&[Source::Path(root.join(".wallpaper.tmp"))]);

        assert_eq!(relative_paths(&root, &found), [".wallpaper.tmp"]);
    }

    #[test]
    fn file_source_that_is_a_directory_warns() {
        let (_dir, root) = temp_root();

        let found = scan(&[Source::File(root.clone())]);

        assert!(found.files.is_empty(), "{:?}", found.files);
        let expected = ScanWarning {
            path: root,
            reason: "not a regular file".to_owned(),
        };
        assert_eq!(found.warnings, [expected]);
    }

    #[test]
    fn dir_source_that_is_a_file_warns() {
        let (_dir, root) = temp_root();
        touch(&root.join("a.png"));

        let found = scan(&[dir_source(&root.join("a.png"), DirOptions::default())]);

        assert!(found.files.is_empty(), "{:?}", found.files);
        let expected = ScanWarning {
            path: root.join("a.png"),
            reason: "not a directory".to_owned(),
        };
        assert_eq!(found.warnings, [expected]);
    }

    #[test]
    fn unreadable_subdirectory_is_skipped_with_one_warning() {
        let (_dir, root) = temp_root();
        touch(&root.join("a.png"));
        let locked = root.join("locked");
        fs::create_dir(&locked).expect("create dir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod fixture");
        if fs::read_dir(&locked).is_ok() {
            // Root ignores directory modes, so the case cannot be staged.
            return;
        }

        let found = scan(&[dir_source(&root, recursive())]);
        // Lets the temp dir delete itself.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod fixture");

        assert_eq!(relative_paths(&root, &found), ["a.png"]);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        assert_eq!(found.warnings[0].path, locked);
        assert!(
            found.warnings[0].reason.contains("ermission denied"),
            "{}",
            found.warnings[0].reason
        );
    }

    #[test]
    fn same_file_reached_twice_is_listed_twice_with_one_identity() {
        let (_dir, root) = temp_root();
        touch(&root.join("walls/a.png"));
        fs::hard_link(root.join("walls/a.png"), root.join("walls/twin.png")).expect("hard link");
        let sources = [
            dir_source(&root.join("walls"), DirOptions::default()),
            Source::File(root.join("walls/a.png")),
        ];

        let found = scan(&sources);

        assert_eq!(
            relative_paths(&root, &found),
            ["walls/a.png", "walls/a.png", "walls/twin.png"]
        );
        let first_id = found.files[0].stamp.id;
        assert!(found.files.iter().all(|file| file.stamp.id == first_id));
    }

    #[test]
    fn stamp_matches_stat_of_the_file() {
        let (_dir, root) = temp_root();
        let image = root.join("a.png");
        fs::write(&image, b"twelve bytes").expect("write fixture");
        let metadata = fs::metadata(&image).expect("stat fixture");

        let found = scan(&[Source::File(image.clone())]);

        let expected = LocalFile {
            path: image,
            stamp: FileStamp {
                id: FileId {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                },
                size: 12,
                mtime: metadata.modified().expect("mtime"),
            },
        };
        assert_eq!(found.files, [expected]);
    }

    #[test]
    fn remote_sources_are_skipped_without_a_warning() {
        let (_dir, root) = temp_root();
        touch(&root.join("a.png"));
        let sources = [
            Source::Url(Url::parse("https://example.com/walls/dawn.jpg").expect("test URL parses")),
            Source::File(root.join("a.png")),
        ];

        let found = scan(&sources);

        assert_eq!(relative_paths(&root, &found), ["a.png"]);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }

    #[test]
    fn relative_source_resolves_through_a_symlinked_config_dir() {
        // The stow layout: ~/.config/gpaperd is a symlink into a dotfiles
        // repository, and the images sit next to the real config file.
        let (_dir, root) = temp_root();
        let repo_dir = root.join("dotfiles/gpaperd");
        touch(&repo_dir.join("config.toml"));
        touch(&repo_dir.join("walls/a.png"));
        fs::create_dir_all(root.join("home/.config")).expect("create dirs");
        symlink(&repo_dir, root.join("home/.config/gpaperd")).expect("create symlink");
        let resolved_config = root
            .join("home/.config/gpaperd/config.toml")
            .canonicalize()
            .expect("resolve config file");
        let env = |_: &str| -> Option<OsString> { None };
        let ctx = ParseContext {
            config_dir: resolved_config.parent().expect("config file has a parent"),
            home_dir: &root.join("home"),
            env: &env,
        };

        let config =
            Config::parse("[light]\nsources = [\"walls\"]\n", &ctx).expect("config parses");
        let found = scan(&config.styles.light.sources);

        assert_eq!(
            relative_paths(&root, &found),
            ["dotfiles/gpaperd/walls/a.png"]
        );
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
    }
}
