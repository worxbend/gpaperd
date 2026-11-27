//! The state file: `state.json`, written atomically.
//!
//! This module only moves [`Persisted`] to and from disk. What the fields
//! mean is the model's business.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use gpaperd_core::model::{Persisted, STATE_VERSION};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state file I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Holds the decoder's message.
    #[error("state file is corrupt: {0}")]
    Corrupt(String),
    #[error("state file has unsupported version {found}")]
    UnsupportedVersion { found: u64 },
    /// Holds the encoder's message, for example for a time before the epoch.
    #[error("state cannot be encoded: {0}")]
    Encode(String),
}

/// Reads the state file. `Ok(None)` when it does not exist. Blocking I/O.
///
/// An existing file is left mode `0600` and its directory mode `0700` before
/// the bytes are treated as state. A missing file does not create the directory.
///
/// # Errors
///
/// [`StateError::Io`] when an existing file cannot be read or its mode cannot
/// be tightened. The file stays where it is. [`StateError::Corrupt`] for
/// invalid JSON or a wrong shape, including a missing version.
/// [`StateError::UnsupportedVersion`] when the version is not `STATE_VERSION`.
/// The caller logs those two and starts without state; the next [`save`]
/// replaces the file. It does not replace a file that failed with I/O.
pub fn load(path: &Path) -> Result<Option<Persisted>, StateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Remote URLs live in this file. Tighten the modes before a parse error
    // can return and leave a world-readable copy behind.
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        ensure_private_dir(dir)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(corrupt)?;
    // The version is checked before the shape, so that a file from a newer
    // gpaperd is reported as unsupported and not as corrupt.
    let found = json
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| StateError::Corrupt("no version number".to_owned()))?;
    if found != u64::from(STATE_VERSION) {
        return Err(StateError::UnsupportedVersion { found });
    }
    serde_json::from_value(json).map(Some).map_err(corrupt)
}

fn corrupt(error: serde_json::Error) -> StateError {
    StateError::Corrupt(error.to_string())
}

/// Writes the state file: temp file in the same directory, `fsync`, rename,
/// then `fsync` of the parent directory. A crash leaves the old or the new
/// file, never a partial one. The parent directory is created when missing.
/// Blocking I/O.
///
/// # Errors
///
/// [`StateError::Encode`] or [`StateError::Io`]. A failure before the rename
/// leaves the old file. A parent sync failure happens after the rename, so
/// the new file is already in place.
pub fn save(path: &Path, state: &Persisted) -> Result<(), StateError> {
    let json =
        serde_json::to_vec_pretty(state).map_err(|error| StateError::Encode(error.to_string()))?;
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        // The file holds remote URLs, credentials included. The directory
        // is the user's alone, including one a previous run left at 0755.
        ensure_private_dir(dir)?;
    }
    // The temp name is fixed. `write_durably` replaces a crash leftover
    // without following a symlink planted at that name.
    let temp = path.with_added_extension("tmp");
    write_durably(&temp, &json).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    // The directory entry from the rename is durable only after this sync.
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Creates `dir` when it is missing, then makes it readable by the user only.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Writes `bytes` to a new file at `path` and waits until they are on disk.
/// The file is readable by the user only, whatever the process umask is.
///
/// `create` plus `truncate` follows a symlink. The temp path is fixed, so a
/// symlink there would let this write land outside the state directory.
/// Removing the name drops a symlink without following it. `create_new`
/// (`O_EXCL`) then refuses to open a name that reappears.
fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    use gpaperd_core::catalog::{FileId, ItemId};
    use gpaperd_core::control::History;
    use gpaperd_core::event::BackgroundKeys;
    use gpaperd_core::model::{STATE_VERSION, StylePersisted};
    use gpaperd_core::style::PerStyle;

    use super::*;

    fn sample_state() -> Persisted {
        Persisted {
            version: STATE_VERSION,
            styles: PerStyle {
                light: StylePersisted {
                    current: Some(ItemId::new("/pics/a.jpg")),
                    slot: Some(FileId { dev: 42, ino: 7 }),
                    last_change: Some(UNIX_EPOCH + Duration::new(1_800_000_000, 123)),
                    remaining: Some(Duration::from_secs(90)),
                    bag: vec![ItemId::new("/pics/b.png"), ItemId::new("/pics/c d#.webp")],
                    shown: Some(vec![ItemId::new("/pics/a.jpg")]),
                    paused: true,
                    user_paused: true,
                    history: History::from(vec![ItemId::new("/pics/z.jpg")]),
                    // So that a field the model adds later needs no edit here.
                    ..StylePersisted::default()
                },
                dark: StylePersisted::default(),
            },
            original_keys: Some(BackgroundKeys {
                picture_uri: "file:///usr/share/backgrounds/day.jpg".into(),
                picture_uri_dark: None,
                picture_options: "zoom".into(),
            }),
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

    #[test]
    fn saved_state_loads_back_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        save(&path, &sample_state()).unwrap();

        assert_eq!(load(&path).unwrap(), Some(sample_state()));
    }

    #[test]
    fn save_creates_the_parent_directory_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state/gpaperd");

        save(&state_dir.join("state.json"), &sample_state()).unwrap();

        assert_eq!(file_names(&state_dir), ["state.json"]);
    }

    #[test]
    fn save_keeps_the_directory_and_the_file_private() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("gpaperd");
        fs::create_dir(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = state_dir.join("state.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save(&path, &sample_state()).unwrap();

        let mode = |path: &Path| MetadataExt::mode(&fs::metadata(path).unwrap()) & 0o777;
        assert_eq!(mode(&state_dir), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn load_of_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(load(&dir.path().join("state.json")).unwrap(), None);
    }

    #[test]
    fn a_state_file_from_before_m2_loads_with_the_new_fields_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let m1_style = r#"{ "current": "/pics/a.jpg", "slot": { "dev": 42, "ino": 7 },
            "last_change": null, "remaining": null, "bag": ["/pics/b.png"] }"#;
        let m1_file = format!(
            r#"{{ "version": 1, "styles": {{ "light": {m1_style}, "dark": {m1_style} }},
                "original_keys": null }}"#
        );
        fs::write(&path, m1_file).unwrap();

        let loaded = load(&path).unwrap().expect("a state file");

        let expected = StylePersisted {
            current: Some(ItemId::new("/pics/a.jpg")),
            slot: Some(FileId { dev: 42, ino: 7 }),
            bag: vec![ItemId::new("/pics/b.png")],
            ..StylePersisted::default()
        };
        assert_eq!(loaded.styles.light, expected);
    }

    #[test]
    fn a_state_file_from_m3_loads_with_no_user_pause_and_no_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let m3_style = r#"{ "current": "/pics/a.jpg", "slot": null, "last_change": null,
            "remaining": null, "bag": [], "shown": ["/pics/a.jpg"],
            "dirty_while_hidden": true, "paused": true }"#;
        let m3_file = format!(
            r#"{{ "version": 1, "styles": {{ "light": {m3_style}, "dark": {m3_style} }},
                "original_keys": null }}"#
        );
        fs::write(&path, m3_file).unwrap();

        let loaded = load(&path).unwrap().expect("a state file");

        let expected = StylePersisted {
            current: Some(ItemId::new("/pics/a.jpg")),
            shown: Some(vec![ItemId::new("/pics/a.jpg")]),
            dirty_while_hidden: true,
            paused: true,
            ..StylePersisted::default()
        };
        assert_eq!(loaded.styles.dark, expected);
        assert!(!loaded.styles.dark.user_paused);
        assert!(loaded.styles.dark.history.is_empty());
    }

    #[test]
    fn load_reports_invalid_json_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{ \"version\": 1, \"sty").unwrap();

        assert!(matches!(load(&path), Err(StateError::Corrupt(_))));
    }

    #[test]
    fn load_reports_a_wrong_shape_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(
            &path,
            r#"{ "version": 1, "styles": "oops", "original_keys": null }"#,
        )
        .unwrap();

        assert!(matches!(load(&path), Err(StateError::Corrupt(_))));
    }

    #[test]
    fn load_reports_a_missing_version_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{ "styles": {}, "original_keys": null }"#).unwrap();

        assert!(matches!(load(&path), Err(StateError::Corrupt(_))));
    }

    #[test]
    fn load_reports_an_unknown_version_whatever_the_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{ "version": 2, "layout": "from the future" }"#).unwrap();

        assert!(matches!(
            load(&path),
            Err(StateError::UnsupportedVersion { found: 2 })
        ));
    }

    #[test]
    fn save_replaces_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "not json").unwrap();

        save(&path, &sample_state()).unwrap();

        assert_eq!(load(&path).unwrap(), Some(sample_state()));
    }

    #[test]
    fn failed_encode_keeps_the_old_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save(&path, &sample_state()).unwrap();
        let mut before_epoch = sample_state();
        before_epoch.styles.dark.last_change = Some(UNIX_EPOCH - Duration::from_secs(1));

        let saved = save(&path, &before_epoch);

        assert!(matches!(saved, Err(StateError::Encode(_))));
        assert_eq!(load(&path).unwrap(), Some(sample_state()));
        assert_eq!(file_names(dir.path()), ["state.json"]);
    }

    #[test]
    fn load_of_an_existing_file_makes_the_file_and_directory_private() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("gpaperd");
        fs::create_dir(&state_dir).unwrap();
        let path = state_dir.join("state.json");
        save(&path, &sample_state()).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(load(&path).unwrap(), Some(sample_state()));

        let mode = |path: &Path| MetadataExt::mode(&fs::metadata(path).unwrap()) & 0o777;
        assert_eq!(mode(&state_dir), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn load_of_a_missing_file_does_not_create_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/state.json");

        assert_eq!(load(&path).unwrap(), None);
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn load_of_a_directory_is_io_and_leaves_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("marker"), b"stay").unwrap();

        assert!(matches!(load(&path), Err(StateError::Io(_))));
        assert_eq!(fs::read(path.join("marker")).unwrap(), b"stay");
    }

    #[test]
    fn load_of_corrupt_bytes_leaves_them_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"not json").unwrap();

        assert!(matches!(load(&path), Err(StateError::Corrupt(_))));
        assert_eq!(fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn save_does_not_follow_a_symlink_at_the_temp_path() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("secret");
        fs::write(&outside, b"keep-me").unwrap();
        let path = dir.path().join("state.json");
        symlink(&outside, path.with_added_extension("tmp")).unwrap();

        save(&path, &sample_state()).unwrap();

        assert_eq!(fs::read(&outside).unwrap(), b"keep-me");
        assert!(!path.with_added_extension("tmp").exists());
        assert_eq!(load(&path).unwrap(), Some(sample_state()));
    }

    #[test]
    fn save_replaces_a_leftover_temp_and_keeps_the_file_private() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let temp = path.with_added_extension("tmp");
        fs::write(&temp, b"partial").unwrap();
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o644)).unwrap();

        save(&path, &sample_state()).unwrap();

        let mode = MetadataExt::mode(&fs::metadata(&path).unwrap()) & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!temp.exists());
    }

    #[test]
    fn failed_rename_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::create_dir(&path).unwrap();

        let saved = save(&path, &sample_state());

        assert!(matches!(saved, Err(StateError::Io(_))));
        assert_eq!(file_names(dir.path()), ["state.json"]);
    }
}
