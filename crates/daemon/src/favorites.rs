//! Copies for `gpaperctl favorite`: the image on screen into the user's
//! `favorites_dir`.
//!
//! A copy never replaces or follows anything: each one is a new file made
//! with `O_CREAT | O_EXCL`, and a taken name gets a numeric suffix. No
//! source file is changed. A copy that fails halfway is removed; it is
//! this function's own file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use gpaperd_core::event::Favorite;

/// Names tried per image, the plain one included, before the copy gives up.
const MAX_NAME_TRIES: u32 = 1_000;

/// Copies each image into `dir`, creating `dir` and its parents when
/// missing. Blocking I/O.
///
/// # Errors
///
/// The first I/O error. Copies written before it stay. A name that is not
/// one plain path component is `InvalidInput`, before anything is written.
pub fn save(dir: &Path, images: &[Favorite]) -> io::Result<()> {
    if let Some(bad) = images.iter().find(|image| !is_plain_name(&image.name)) {
        let message = format!("{:?} is no plain file name", bad.name);
        return Err(io::Error::new(ErrorKind::InvalidInput, message));
    }
    fs::create_dir_all(dir)?;
    for image in images {
        copy_new(&image.source, dir, &image.name)?;
    }
    Ok(())
}

/// Whether `name` is one normal path component: no `/`, not empty, `.` or
/// `..`.
fn is_plain_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) && !name.contains('/')
}

/// Copies `source` into a new file in `dir`, under `name` or the first free
/// suffixed form of it.
fn copy_new(source: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    let mut input = File::open(source)?;
    let (path, mut output) = create_unique(dir, name)?;
    let copied = io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    if let Err(error) = copied {
        drop(output);
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// Creates the first of `name`, `stem-1.ext`, `stem-2.ext`, … that does not
/// exist in `dir`.
fn create_unique(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    for attempt in 0..MAX_NAME_TRIES {
        let path = dir.join(suffixed(name, attempt));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let message = format!("{MAX_NAME_TRIES} names like {name:?} are taken");
    Err(io::Error::new(ErrorKind::AlreadyExists, message))
}

/// `name` for attempt 0, else `stem-N.ext`, or `name-N` without an
/// extension.
fn suffixed(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.to_owned();
    }
    let path = Path::new(name);
    let stem = path.file_stem().map(|stem| stem.to_string_lossy());
    match (stem, path.extension()) {
        (Some(stem), Some(extension)) => {
            format!("{stem}-{attempt}.{}", extension.to_string_lossy())
        }
        _ => format!("{name}-{attempt}"),
    }
}
