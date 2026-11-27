//! `gpaperctl favorite`'s copies: into a temp directory, never over a file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use gpaperd_core::event::Favorite;
use gpaperd_daemon::favorites;
use tempfile::TempDir;

fn favorite(source: &Path, name: &str) -> Favorite {
    Favorite {
        source: source.to_owned(),
        name: name.to_owned(),
    }
}

/// A temp directory with `day.jpg` holding `day` in `pictures/`.
fn pictures() -> (TempDir, PathBuf) {
    let root = TempDir::new().expect("temp dir");
    let pictures = root.path().join("pictures");
    fs::create_dir(&pictures).expect("pictures dir");
    let day = pictures.join("day.jpg");
    fs::write(&day, b"day").expect("write day.jpg");
    (root, day)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_favorite_is_copied_under_its_name_into_a_new_directory() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites/nested");

    favorites::save(&favorites_dir, &[favorite(&day, "day.jpg")]).expect("saved");

    assert_eq!(fs::read(favorites_dir.join("day.jpg")).unwrap(), b"day");
    assert_eq!(fs::read(&day).unwrap(), b"day", "the source changed");
}

#[test]
fn a_taken_name_gets_a_numeric_suffix_and_the_old_file_stays() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites");
    fs::create_dir(&favorites_dir).unwrap();
    fs::write(favorites_dir.join("day.jpg"), b"older").unwrap();
    fs::write(favorites_dir.join("day-1.jpg"), b"old").unwrap();

    favorites::save(&favorites_dir, &[favorite(&day, "day.jpg")]).expect("saved");

    assert_eq!(names(&favorites_dir), ["day-1.jpg", "day-2.jpg", "day.jpg"]);
    assert_eq!(fs::read(favorites_dir.join("day.jpg")).unwrap(), b"older");
    assert_eq!(fs::read(favorites_dir.join("day-1.jpg")).unwrap(), b"old");
    assert_eq!(fs::read(favorites_dir.join("day-2.jpg")).unwrap(), b"day");
}

#[test]
fn a_name_without_an_extension_gets_its_suffix_at_the_end() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites");
    fs::create_dir(&favorites_dir).unwrap();
    fs::write(favorites_dir.join("2000"), b"old").unwrap();

    favorites::save(&favorites_dir, &[favorite(&day, "2000")]).expect("saved");

    assert_eq!(names(&favorites_dir), ["2000", "2000-1"]);
}

#[test]
fn a_dangling_symlink_under_the_name_is_not_followed() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites");
    fs::create_dir(&favorites_dir).unwrap();
    let target = root.path().join("elsewhere.jpg");
    std::os::unix::fs::symlink(&target, favorites_dir.join("day.jpg")).unwrap();

    favorites::save(&favorites_dir, &[favorite(&day, "day.jpg")]).expect("saved");

    assert!(!target.exists(), "the copy followed the symlink");
    assert_eq!(fs::read(favorites_dir.join("day-1.jpg")).unwrap(), b"day");
}

#[test]
fn two_images_with_one_name_both_land() {
    let (root, day) = pictures();
    let other = root.path().join("day.jpg");
    fs::write(&other, b"other day").unwrap();
    let favorites_dir = root.path().join("favorites");

    let images = [favorite(&day, "day.jpg"), favorite(&other, "day.jpg")];
    favorites::save(&favorites_dir, &images).expect("saved");

    assert_eq!(fs::read(favorites_dir.join("day.jpg")).unwrap(), b"day");
    assert_eq!(
        fs::read(favorites_dir.join("day-1.jpg")).unwrap(),
        b"other day"
    );
}

#[test]
fn a_missing_source_fails_and_leaves_no_file() {
    let (root, _) = pictures();
    let favorites_dir = root.path().join("favorites");
    let gone = root.path().join("gone.jpg");

    let error = favorites::save(&favorites_dir, &[favorite(&gone, "gone.jpg")])
        .expect_err("nothing to copy");

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        names(&favorites_dir).is_empty(),
        "{:?}",
        names(&favorites_dir)
    );
}

#[test]
fn an_unwritable_directory_fails_with_the_io_error() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites");
    fs::create_dir(&favorites_dir).unwrap();
    fs::set_permissions(&favorites_dir, fs::Permissions::from_mode(0o500)).unwrap();
    // Root ignores the mode; the test means nothing then.
    if fs::write(favorites_dir.join("probe"), b"").is_ok() {
        eprintln!("skipped: the directory is writable despite its mode");
        return;
    }

    let error =
        favorites::save(&favorites_dir, &[favorite(&day, "day.jpg")]).expect_err("read-only");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn a_name_that_is_no_single_component_is_refused() {
    let (root, day) = pictures();
    let favorites_dir = root.path().join("favorites");

    for name in ["", ".", "..", "a/b.jpg", "dir/", "/etc/passwd"] {
        let error = favorites::save(&favorites_dir, &[favorite(&day, name)])
            .expect_err("no plain file name");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
    }
    assert!(!root.path().join("b.jpg").exists());
}
