//! gpaperd's tile in the wallpaper picker of GNOME Settings.
//!
//! gnome-control-center lists the files in
//! `$XDG_DATA_HOME/gnome-background-properties` among its wallpapers, in the
//! format of gnome-backgrounds' `adwaita.xml`. It parses every file there,
//! whatever its name, and again whenever one is created or changed.
//! gpaperd's file names the two slot files, so picking the tile writes both
//! slot URIs to the keys.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use gpaperd_core::config::PictureOptions;
use gpaperd_core::style::PerStyle;

/// What [`offer`] or [`withdraw`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tile file was written.
    Written,
    /// The tile file already held exactly these bytes.
    Unchanged,
    /// gpaperd's tile file was removed.
    Removed,
    /// There was no tile file.
    Absent,
    /// The file holds something gpaperd did not write: it stays.
    NotOurs,
}

/// Settings reads the file as another process: it must be readable.
const TILE_MODE: u32 = 0o644;

/// Bytes read of a file at the tile's name, at most. Each slot path is at
/// most `PATH_MAX` (4096) bytes, six times that when escaped: gpaperd's
/// tile is always shorter.
const MAX_TILE_BYTES: u64 = 64 * 1024;

/// Makes `file` offer the two `slots`, shown with `options`. Blocking I/O.
///
/// Writes `.<name>.tmp-<pid>` in `staging_dir` and renames it over `file`,
/// so Settings never reads half a tile nor a temp file: it parses every
/// file in its folder. `staging_dir` must be on the file system of `file`
/// for the rename. A temp file that a crash leaves behind stays in
/// `staging_dir`, and the next write by a process with that pid replaces
/// it. Missing directories are created. A file that already holds these
/// bytes is left alone. Not synced: every start writes the tile again when
/// its bytes differ.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidData`] when a slot path cannot be written in
/// XML: not UTF-8, or with a control character. Any I/O error; the temp
/// file is then removed and `file` is untouched.
pub fn offer(
    file: &Path,
    staging_dir: &Path,
    slots: &PerStyle<PathBuf>,
    options: PictureOptions,
) -> io::Result<Outcome> {
    let tile = render(slots, options)?;
    if read_held(file).is_ok_and(|held| held == tile.as_bytes()) {
        return Ok(Outcome::Unchanged);
    }
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::create_dir_all(staging_dir)?;
    let temp = temp_path(file, staging_dir)?;
    write_new(&temp, tile.as_bytes())
        .and_then(|()| fs::rename(&temp, file))
        .inspect_err(|_| {
            // Best effort: the caller gets the error that failed the write.
            let _ = fs::remove_file(&temp);
        })?;
    Ok(Outcome::Written)
}

/// Removes `file` when gpaperd wrote it for these `slots`, with any picture
/// options. A file with other content stays. Blocking I/O.
///
/// # Errors
///
/// Any I/O error but a missing file.
pub fn withdraw(file: &Path, slots: &PerStyle<PathBuf>) -> io::Result<Outcome> {
    let held = match read_held(file) {
        Ok(held) => held,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Absent),
        Err(error) => return Err(error),
    };
    let is_ours = PictureOptions::ALL
        .into_iter()
        .filter_map(|options| render(slots, options).ok())
        .any(|tile| tile.as_bytes() == held);
    if !is_ours {
        return Ok(Outcome::NotOurs);
    }
    fs::remove_file(file)?;
    Ok(Outcome::Removed)
}

/// What `file` holds, up to [`MAX_TILE_BYTES`]: a link to an endless file
/// cannot fill the memory, and a longer file is no tile of gpaperd's.
fn read_held(file: &Path) -> io::Result<Vec<u8>> {
    let mut held = Vec::new();
    File::open(file)?
        .take(MAX_TILE_BYTES)
        .read_to_end(&mut held)?;
    Ok(held)
}

/// The tile file's text, in the format of gnome-backgrounds' `adwaita.xml`.
fn render(slots: &PerStyle<PathBuf>, options: PictureOptions) -> io::Result<String> {
    let light = escaped_path(&slots.light)?;
    let dark = escaped_path(&slots.dark)?;
    let options = options.as_str();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE wallpapers SYSTEM \"gnome-wp-list.dtd\">\n\
         <wallpapers>\n\
         \x20 <wallpaper deleted=\"false\">\n\
         \x20   <name>gpaperd rotation</name>\n\
         \x20   <filename>{light}</filename>\n\
         \x20   <filename-dark>{dark}</filename-dark>\n\
         \x20   <options>{options}</options>\n\
         \x20   <shade_type>solid</shade_type>\n\
         \x20   <pcolor>#000000</pcolor>\n\
         \x20   <scolor>#000000</scolor>\n\
         \x20 </wallpaper>\n\
         </wallpapers>\n"
    ))
}

/// `path` as XML text. Settings reads the text back as the file name.
fn escaped_path(path: &Path) -> io::Result<String> {
    let text = path
        .to_str()
        .filter(|text| !text.chars().any(char::is_control))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("slot path cannot be written in XML: {}", path.display()),
            )
        })?;
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    Ok(escaped)
}

/// `.<name>.tmp-<pid>` in `staging_dir`, for `file`.
fn temp_path(file: &Path, staging_dir: &Path) -> io::Result<PathBuf> {
    let name = file.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no file name: {}", file.display()),
        )
    })?;
    let temp_name = format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id());
    Ok(staging_dir.join(temp_name))
}

/// Writes `bytes` to a new file at `path`, mode [`TILE_MODE`] whatever the
/// umask is. A leftover at `path` is removed first, without following a
/// symlink; `create_new` then refuses a name that reappears.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(TILE_MODE)
        .open(path)?;
    staged.write_all(bytes)?;
    staged.set_permissions(fs::Permissions::from_mode(TILE_MODE))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use gpaperd_core::config::PictureOptions;
    use gpaperd_core::style::PerStyle;

    use super::*;

    fn slots_in(dir: &Path) -> PerStyle<PathBuf> {
        PerStyle::from_fn(|style| dir.join("gpaperd/slots").join(style.as_str()))
    }

    fn tile_in(dir: &Path) -> PathBuf {
        dir.join("gnome-background-properties/gpaperd.xml")
    }

    fn staging_in(dir: &Path) -> PathBuf {
        dir.join("gpaperd")
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn offer_writes_the_tile_with_both_slots_and_the_options() {
        let slots = PerStyle {
            light: PathBuf::from("/home/u/.local/share/gpaperd/slots/light"),
            dark: PathBuf::from("/home/u/.local/share/gpaperd/slots/dark"),
        };
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());

        let outcome = offer(
            &tile,
            &staging_in(dir.path()),
            &slots,
            PictureOptions::Spanned,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Written);
        assert_eq!(
            fs::read_to_string(&tile).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE wallpapers SYSTEM \"gnome-wp-list.dtd\">\n\
             <wallpapers>\n\
             \x20 <wallpaper deleted=\"false\">\n\
             \x20   <name>gpaperd rotation</name>\n\
             \x20   <filename>/home/u/.local/share/gpaperd/slots/light</filename>\n\
             \x20   <filename-dark>/home/u/.local/share/gpaperd/slots/dark</filename-dark>\n\
             \x20   <options>spanned</options>\n\
             \x20   <shade_type>solid</shade_type>\n\
             \x20   <pcolor>#000000</pcolor>\n\
             \x20   <scolor>#000000</scolor>\n\
             \x20 </wallpaper>\n\
             </wallpapers>\n"
        );
    }

    #[test]
    fn offer_makes_the_tile_readable_by_everyone() {
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());

        offer(
            &tile,
            &staging_in(dir.path()),
            &slots_in(dir.path()),
            PictureOptions::Zoom,
        )
        .unwrap();

        let mode = fs::metadata(&tile).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn offer_escapes_markup_in_the_slot_paths() {
        let slots = PerStyle {
            light: PathBuf::from("/home/a&b/<l>"),
            dark: PathBuf::from("/home/\"it's\"/d"),
        };
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());

        offer(&tile, &staging_in(dir.path()), &slots, PictureOptions::Zoom).unwrap();

        let text = fs::read_to_string(&tile).unwrap();
        assert!(text.contains("<filename>/home/a&amp;b/&lt;l&gt;</filename>"));
        assert!(text.contains("<filename-dark>/home/&quot;it&apos;s&quot;/d</filename-dark>"));
    }

    #[test]
    fn offer_refuses_a_slot_path_that_is_not_utf8() {
        let slots = PerStyle {
            light: PathBuf::from(std::ffi::OsString::from_vec(b"/home/\xff/l".to_vec())),
            dark: PathBuf::from("/home/u/d"),
        };
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());

        let error =
            offer(&tile, &staging_in(dir.path()), &slots, PictureOptions::Zoom).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!tile.exists());
    }

    #[test]
    fn offer_refuses_a_slot_path_with_a_control_character() {
        let slots = PerStyle {
            light: PathBuf::from("/home/u\n/l"),
            dark: PathBuf::from("/home/u/d"),
        };
        let dir = tempfile::tempdir().unwrap();

        let error = offer(
            &tile_in(dir.path()),
            &staging_in(dir.path()),
            &slots,
            PictureOptions::Zoom,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn offer_leaves_a_tile_with_the_same_bytes_alone() {
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());
        let slots = slots_in(dir.path());
        offer(&tile, &staging_in(dir.path()), &slots, PictureOptions::Zoom).unwrap();
        let inode = fs::metadata(&tile).unwrap().ino();

        let outcome = offer(&tile, &staging_in(dir.path()), &slots, PictureOptions::Zoom).unwrap();

        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(fs::metadata(&tile).unwrap().ino(), inode);
    }

    #[test]
    fn offer_replaces_a_tile_with_other_options_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());
        let slots = slots_in(dir.path());
        offer(&tile, &staging_in(dir.path()), &slots, PictureOptions::Zoom).unwrap();

        let outcome = offer(
            &tile,
            &staging_in(dir.path()),
            &slots,
            PictureOptions::Centered,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Written);
        assert!(
            fs::read_to_string(&tile)
                .unwrap()
                .contains("<options>centered</options>")
        );
        assert_eq!(file_names(tile.parent().unwrap()), ["gpaperd.xml"]);
    }

    #[test]
    fn offer_stages_the_tile_outside_the_folder_that_settings_reads() {
        // Settings parses every file in its folder, temp files too.
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());
        let staging = staging_in(dir.path());
        fs::create_dir_all(&staging).unwrap();
        let leftover = staging.join(format!(".gpaperd.xml.tmp-{}", std::process::id()));
        fs::write(&leftover, "half a tile").unwrap();

        offer(&tile, &staging, &slots_in(dir.path()), PictureOptions::Zoom).unwrap();

        assert_eq!(file_names(tile.parent().unwrap()), ["gpaperd.xml"]);
        assert!(file_names(&staging).is_empty());
    }

    #[test]
    fn withdraw_removes_a_tile_that_gpaperd_wrote_with_any_options() {
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());
        let slots = slots_in(dir.path());
        offer(
            &tile,
            &staging_in(dir.path()),
            &slots,
            PictureOptions::Wallpaper,
        )
        .unwrap();

        let outcome = withdraw(&tile, &slots).unwrap();

        assert_eq!(outcome, Outcome::Removed);
        assert!(!tile.exists());
    }

    #[test]
    fn withdraw_keeps_a_file_that_gpaperd_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let tile = tile_in(dir.path());
        fs::create_dir_all(tile.parent().unwrap()).unwrap();
        fs::write(&tile, "<wallpapers/>\n").unwrap();

        let outcome = withdraw(&tile, &slots_in(dir.path())).unwrap();

        assert_eq!(outcome, Outcome::NotOurs);
        assert_eq!(fs::read_to_string(&tile).unwrap(), "<wallpapers/>\n");
    }

    #[test]
    fn withdraw_without_a_tile_does_nothing() {
        let dir = tempfile::tempdir().unwrap();

        let outcome = withdraw(&tile_in(dir.path()), &slots_in(dir.path())).unwrap();

        assert_eq!(outcome, Outcome::Absent);
    }
}
