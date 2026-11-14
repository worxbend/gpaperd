//! Slot files: the two fixed files the background keys point at.
//!
//! A rotation replaces a slot with one `rename(2)`, so gnome-shell never sees
//! a partial file and its own file monitor triggers the reload. See the
//! plan's "Applying wallpapers" section.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use gpaperd_core::catalog::FileId;
use gpaperd_core::config::LinkPolicy;
use gpaperd_core::event::{Method, SlotWrite};
use rustix::fs::{AtFlags, CWD};
use rustix::io::Errno;

use crate::scan::file_id;

/// Atomically replaces `slot` with the content of `src`. Blocking I/O.
///
/// Stages `.<slot name>.tmp-<pid>` next to the slot, then renames it over the
/// slot. The slot's directory is created when missing.
///
/// Staging with [`LinkPolicy::PreferLink`] tries a hardlink, then a reflink
/// (`FICLONE`), then a copy. With [`LinkPolicy::AlwaysCopy`] it copies. A
/// reflink or a copy is synced to disk before the rename, and gets the
/// permission bits of `src`, less the umask.
///
/// The slot's inode always changes: when `src` and `slot` already share an
/// inode, the hardlink is skipped, because a rename between two links of one
/// inode does nothing and the shell would not reload.
///
/// # Errors
///
/// On any failure the temp file is removed and the old slot is untouched.
pub fn publish(src: &Path, slot: &Path, link: LinkPolicy) -> io::Result<SlotWrite> {
    let temp = temp_path(slot)?;
    if let Some(dir) = temp.parent() {
        fs::create_dir_all(dir)?;
    }
    remove_if_present(&temp)?; // left by a crash of a process with this pid
    stage(src, &temp, slot, link)
        .and_then(|method| install(&temp, slot, method))
        .inspect_err(|_| {
            // Best effort: the caller gets the error that made the publish
            // fail, and the start-up sweep deletes a file that stays behind.
            let _ = fs::remove_file(&temp);
        })
}

/// Atomically replaces `slot` with a copy of its own content: the same bytes
/// on a new inode. The shell then drops the texture it holds for the slot
/// and loads the file again. Blocking I/O.
///
/// # Errors
///
/// As [`publish`]. A slot that is not there is [`io::ErrorKind::NotFound`].
pub fn republish(slot: &Path) -> io::Result<SlotWrite> {
    // A file shares its own inode, so this never stages a hardlink.
    publish(slot, slot, LinkPolicy::PreferLink)
}

/// `.<slot name>.tmp-<pid>` next to the slot.
fn temp_path(slot: &Path) -> io::Result<PathBuf> {
    let name = slot.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("slot path has no file name: {}", slot.display()),
        )
    })?;
    let temp_name = format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id());
    Ok(slot.with_file_name(temp_name))
}

/// Puts the content of `src` into a new file at `temp`.
fn stage(src: &Path, temp: &Path, slot: &Path, link: LinkPolicy) -> io::Result<Method> {
    let hard_link_allowed = link == LinkPolicy::PreferLink && !shares_inode(src, slot)?;
    if hard_link_allowed && try_hard_link(src, temp)? {
        return Ok(Method::Hardlink);
    }
    stage_new_inode(src, temp, link)
}

/// Whether `src` already is the file in the slot. A rename between two links
/// of one inode does nothing, so such a source must be staged as a new inode.
fn shares_inode(src: &Path, slot: &Path) -> io::Result<bool> {
    let source = file_id(&fs::metadata(src)?);
    Ok(slot_id(slot)? == Some(source))
}

/// Links `temp` to the file that `src` names. `Ok(false)` when the file
/// system or the kernel's link protection refuses a link that a copy can
/// replace.
fn try_hard_link(src: &Path, temp: &Path) -> io::Result<bool> {
    // SYMLINK_FOLLOW links the file a symlink points at, not the symlink.
    match rustix::fs::linkat(CWD, src, CWD, temp, AtFlags::SYMLINK_FOLLOW) {
        Ok(()) => Ok(true),
        Err(Errno::XDEV | Errno::PERM | Errno::MLINK) => Ok(false),
        Err(errno) => Err(errno.into()),
    }
}

/// The read, write and execute bits of a file mode, without the file type
/// and the set-id bits.
const PERMISSION_BITS: u32 = 0o777;

/// Reflinks or copies `src` into a new file at `temp`.
fn stage_new_inode(src: &Path, temp: &Path, link: LinkPolicy) -> io::Result<Method> {
    let mut source = File::open(src)?;
    // A hard link has its source's mode. A copy with the default mode could
    // be readable by more users than a private source is.
    let source_mode = source.metadata()?.mode() & PERMISSION_BITS;
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(source_mode)
        .open(temp)?;
    // Whatever makes FICLONE fail, the copy is the fallback, and the copy
    // reports a real I/O error itself.
    let reflinked =
        link == LinkPolicy::PreferLink && rustix::fs::ioctl_ficlone(&staged, &source).is_ok();
    let method = if reflinked {
        Method::Reflink
    } else {
        io::copy(&mut source, &mut staged)?;
        Method::Copy
    };
    // On disk before the rename, so a power cut cannot leave an empty slot.
    staged.sync_all()?;
    Ok(method)
}

/// Renames the staged file onto the slot.
fn install(temp: &Path, slot: &Path, method: Method) -> io::Result<SlotWrite> {
    let staged = file_id(&fs::metadata(temp)?);
    fs::rename(temp, slot)?;
    Ok(SlotWrite {
        method,
        slot: staged,
    })
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// The inode of a slot file, or `None` when the file does not exist.
/// Blocking I/O.
///
/// # Errors
///
/// Any `stat` failure other than "not found".
pub fn slot_id(slot: &Path) -> io::Result<Option<FileId>> {
    match fs::metadata(slot) {
        Ok(stat) => Ok(Some(file_id(&stat))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The `file://` URI of a path, percent-encoded (`#`, `%`, spaces, …). This
/// is the value written to the background keys.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when `path` is not absolute.
pub fn file_uri(path: &Path) -> io::Result<String> {
    url::Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute path: {}", path.display()),
            )
        })
}

/// Deletes the temp files (`.*.tmp-*`) that a crashed publish left in
/// `slots_dir`. A missing directory has nothing to sweep. Blocking I/O.
///
/// # Errors
///
/// The first failure to list the directory or to delete a temp file.
pub fn sweep_temp_files(slots_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(slots_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if is_temp_name(&entry.file_name()) {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

/// Whether a file name has the shape `.*.tmp-*` that [`temp_path`] makes.
fn is_temp_name(name: &OsStr) -> bool {
    name.to_string_lossy()
        .strip_prefix('.')
        .is_some_and(|rest| rest.contains(".tmp-"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    use gpaperd_core::event::Method;

    use super::*;

    fn write_source(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn inode(path: &Path) -> FileId {
        slot_id(path).unwrap().expect("file exists")
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn crash_leftover(slot: &Path) -> PathBuf {
        slot.with_file_name(format!(".light.tmp-{}", std::process::id()))
    }

    #[test]
    fn publish_replaces_the_slot_content_and_reports_its_inode() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        let night = write_source(dir.path(), "night.jpg", "night");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        let written = publish(&night, &slot, LinkPolicy::PreferLink).unwrap();

        assert_eq!(fs::read_to_string(&slot).unwrap(), "night");
        assert_eq!(written.slot, inode(&slot));
    }

    #[test]
    fn publish_creates_the_slots_directory() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("share/gpaperd/slots/light");
        let day = write_source(dir.path(), "day.jpg", "day");

        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn prefer_link_hardlinks_a_source_on_the_same_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");

        let written = publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        assert_eq!(written.method, Method::Hardlink);
        assert_eq!(inode(&slot), inode(&day));
    }

    #[test]
    fn always_copy_gives_the_slot_its_own_inode() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");

        let written = publish(&day, &slot, LinkPolicy::AlwaysCopy).unwrap();

        assert_eq!(written.method, Method::Copy);
        assert_ne!(inode(&slot), inode(&day));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn copied_slot_is_no_more_readable_than_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let private = write_source(dir.path(), "private.jpg", "private");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();

        publish(&private, &slot, LinkPolicy::AlwaysCopy).unwrap();

        assert_eq!(fs::metadata(&slot).unwrap().mode() & PERMISSION_BITS, 0o600);
    }

    #[test]
    fn read_only_source_is_copied_over_a_read_only_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        let night = write_source(dir.path(), "night.jpg", "night");
        for source in [&day, &night] {
            fs::set_permissions(source, fs::Permissions::from_mode(0o444)).unwrap();
        }
        publish(&day, &slot, LinkPolicy::AlwaysCopy).unwrap();

        publish(&night, &slot, LinkPolicy::AlwaysCopy).unwrap();

        assert_eq!(fs::read_to_string(&slot).unwrap(), "night");
    }

    #[test]
    fn source_on_another_filesystem_is_copied() {
        let slots = tempfile::tempdir().unwrap();
        let Ok(pics) = tempfile::tempdir_in("/dev/shm") else {
            eprintln!("skipped: /dev/shm is not writable");
            return;
        };
        if fs::metadata(slots.path()).unwrap().dev() == fs::metadata(pics.path()).unwrap().dev() {
            eprintln!("skipped: the temp directory and /dev/shm are one file system");
            return;
        }
        let slot = slots.path().join("light");
        let day = write_source(pics.path(), "day.jpg", "day");

        let written = publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        assert_eq!(written.method, Method::Copy);
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn hard_link_refused_with_eperm_falls_through_to_the_copy() {
        // The kernel refuses to link a directory with EPERM, as
        // fs.protected_hardlinks does for another user's file. The error
        // must then come from the copy, not from the link.
        let dir = tempfile::tempdir().unwrap();
        let album = dir.path().join("album");
        fs::create_dir(&album).unwrap();

        let failed = publish(&album, &dir.path().join("light"), LinkPolicy::PreferLink);

        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::IsADirectory);
    }

    #[test]
    fn slot_keeps_its_data_after_the_source_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        fs::remove_file(&day).unwrap();

        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn symlinked_source_publishes_the_file_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        let alias = dir.path().join("alias.jpg");
        symlink(&day, &alias).unwrap();

        publish(&alias, &slot, LinkPolicy::PreferLink).unwrap();

        assert!(fs::symlink_metadata(&slot).unwrap().is_file());
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn publish_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        let day = write_source(dir.path(), "day.jpg", "day");

        publish(&day, &slots_dir.join("light"), LinkPolicy::PreferLink).unwrap();
        publish(&day, &slots_dir.join("light"), LinkPolicy::AlwaysCopy).unwrap();

        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn publish_replaces_a_temp_file_left_by_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        fs::create_dir(&slots_dir).unwrap();
        let slot = slots_dir.join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        fs::write(crash_leftover(&slot), "half a file").unwrap();

        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn republishing_the_slots_own_source_stages_a_new_inode() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();
        assert_eq!(inode(&slot), inode(&day));

        let written = publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        assert_ne!(written.method, Method::Hardlink);
        assert_ne!(inode(&slot), inode(&day));
        assert_eq!(written.slot, inode(&slot));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
    }

    #[test]
    fn republish_keeps_the_content_and_gives_the_slot_a_new_inode() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        let slot = slots_dir.join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::AlwaysCopy).unwrap();
        let before = inode(&slot);

        let written = republish(&slot).unwrap();

        assert_ne!(written.slot, before);
        assert_eq!(written.slot, inode(&slot));
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn republish_of_a_hardlinked_slot_leaves_the_source_alone() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slots/light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();
        let source_before = inode(&day);

        republish(&slot).unwrap();

        assert_ne!(inode(&slot), source_before);
        assert_eq!(inode(&day), source_before);
        assert_eq!(fs::read_to_string(&day).unwrap(), "day");
    }

    #[test]
    fn republish_of_a_missing_slot_fails_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        fs::create_dir(&slots_dir).unwrap();

        let failed = republish(&slots_dir.join("light"));

        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(file_names(&slots_dir).is_empty());
    }

    #[test]
    fn publish_of_a_missing_source_fails_and_keeps_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        let slot = slots_dir.join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();

        let failed = publish(&dir.path().join("gone.jpg"), &slot, LinkPolicy::PreferLink);

        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn publish_that_fails_while_staging_keeps_the_slot_and_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        let slot = slots_dir.join("light");
        let day = write_source(dir.path(), "day.jpg", "day");
        publish(&day, &slot, LinkPolicy::PreferLink).unwrap();
        let not_a_file = dir.path().join("album");
        fs::create_dir(&not_a_file).unwrap();

        let failed = publish(&not_a_file, &slot, LinkPolicy::PreferLink);

        assert!(failed.is_err());
        assert_eq!(fs::read_to_string(&slot).unwrap(), "day");
        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn publish_that_fails_at_the_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let slots_dir = dir.path().join("slots");
        let slot = slots_dir.join("light");
        fs::create_dir_all(&slot).unwrap();
        let day = write_source(dir.path(), "day.jpg", "day");

        let failed = publish(&day, &slot, LinkPolicy::PreferLink);

        assert!(failed.is_err());
        assert_eq!(file_names(&slots_dir), ["light"]);
    }

    #[test]
    fn publish_rejects_a_slot_path_without_a_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let day = write_source(dir.path(), "day.jpg", "day");

        let failed = publish(&day, Path::new("/"), LinkPolicy::PreferLink);

        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn slot_id_of_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(slot_id(&dir.path().join("light")).unwrap(), None);
    }

    #[test]
    fn slot_id_is_the_files_device_and_inode() {
        let dir = tempfile::tempdir().unwrap();
        let day = write_source(dir.path(), "day.jpg", "day");
        let stat = fs::metadata(&day).unwrap();

        let id = slot_id(&day).unwrap();

        assert_eq!(
            id,
            Some(FileId {
                dev: stat.dev(),
                ino: stat.ino()
            })
        );
    }

    #[test]
    fn sweep_deletes_leftover_temp_files_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "light",
            "dark",
            ".light.tmp-4242",
            ".dark.tmp-1",
            ".hidden",
            "notes.tmp-7",
        ] {
            fs::write(dir.path().join(name), "x").unwrap();
        }

        sweep_temp_files(dir.path()).unwrap();

        assert_eq!(
            file_names(dir.path()),
            [".hidden", "dark", "light", "notes.tmp-7"]
        );
    }

    #[test]
    fn sweep_of_a_missing_directory_is_ok() {
        let dir = tempfile::tempdir().unwrap();

        sweep_temp_files(&dir.path().join("slots")).unwrap();
    }

    #[test]
    fn file_uri_of_a_plain_path_adds_the_scheme() {
        let uri = file_uri(Path::new("/home/u/.local/share/gpaperd/slots/light")).unwrap();

        assert_eq!(uri, "file:///home/u/.local/share/gpaperd/slots/light");
    }

    #[test]
    fn file_uri_percent_encodes_hash_percent_and_space() {
        let uri = file_uri(Path::new("/pics/50% off #1/a b.jpg")).unwrap();

        assert_eq!(uri, "file:///pics/50%25%20off%20%231/a%20b.jpg");
    }

    #[test]
    fn file_uri_rejects_a_relative_path() {
        let failed = file_uri(Path::new("slots/light"));

        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
