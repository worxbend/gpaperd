//! The state file: `state.json`, written atomically.
//!
//! This module only moves [`Persisted`] to and from disk. What the fields
//! mean is the model's business.

use std::fs::{self, File};
use std::io::{self, Write};
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
/// # Errors
///
/// [`StateError::Corrupt`] for invalid JSON or a wrong shape, including a
/// missing version. [`StateError::UnsupportedVersion`] when the version is
/// not `STATE_VERSION`. In both cases the caller logs it and starts without
/// state; the next [`save`] replaces the file.
pub fn load(path: &Path) -> Result<Option<Persisted>, StateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
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

/// Writes the state file: temp file in the same directory, `fsync`, then
/// rename, so a crash leaves the old or the new file, never a partial one.
/// The parent directory is created when missing. Blocking I/O.
///
/// # Errors
///
/// [`StateError::Encode`] or [`StateError::Io`]. The old file is untouched.
pub fn save(path: &Path, state: &Persisted) -> Result<(), StateError> {
    let json =
        serde_json::to_vec_pretty(state).map_err(|error| StateError::Encode(error.to_string()))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // The temp name is fixed, so the next save overwrites what a crash left.
    let temp = path.with_added_extension("tmp");
    write_durably(&temp, &json)
        .and_then(|()| fs::rename(&temp, path))
        .inspect_err(|_| {
            // Best effort: the caller gets the error that made the save fail.
            let _ = fs::remove_file(&temp);
        })?;
    Ok(())
}

/// Writes `bytes` to a new file at `path` and waits until they are on disk.
fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
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
    fn failed_rename_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::create_dir(&path).unwrap();

        let saved = save(&path, &sample_state());

        assert!(matches!(saved, Err(StateError::Io(_))));
        assert_eq!(file_names(dir.path()), ["state.json"]);
    }
}
