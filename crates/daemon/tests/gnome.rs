//! The GSettings adapter against compiled test schemas and the keyfile
//! backend.
//!
//! GSettings reads `GSETTINGS_BACKEND`, `GSETTINGS_SCHEMA_DIR` and the XDG
//! directories once per process. So every scenario runs in its own child
//! process of this test binary, with all of them pointed into one temp
//! directory. No scenario can reach the user's dconf.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gpaperd_core::config::PictureOptions;
use gpaperd_core::event::{BackgroundKeys, Connectivity, GnomeEvent, GnomeSnapshot};
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_daemon::gnome::{GnomeError, GnomePort, GnomeThread, KeyWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::timeout;

/// Set in a child process: the temp directory that holds its GSettings world.
const SANDBOX_VAR: &str = "GPAPERD_TEST_GNOME_SANDBOX";
/// The child creates this file when its scenario ran to the end.
const SCENARIO_RAN: &str = "scenario-ran";
/// Where the keyfile backend stores settings, below `XDG_CONFIG_HOME`.
const SETTINGS_DIR: &str = "config/glib-2.0/settings";

const LIGHT_SLOT: &str = "file:///gpaperd-test/slots/light";
const DARK_SLOT: &str = "file:///gpaperd-test/slots/dark";

const FULL_CONNECTIVITY: GnomeEvent = GnomeEvent::ConnectivityChanged(Connectivity::Full);

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const QUIET_PERIOD: Duration = Duration::from_millis(300);

/// The GSettings world of one scenario.
struct Session {
    backend: &'static str,
    /// A directory below `tests/schemas`. `None`: no schema is installed.
    schemas: Option<&'static str>,
    /// `GIO_USE_NETWORK_MONITOR`. `None` lets GLib pick by priority, which
    /// depends on this machine.
    network_monitor: Option<&'static str>,
}

/// GLib's base monitor reports full connectivity and never changes.
const BASE_MONITOR: Option<&str> = Some("base");

const GNOME: Session = Session {
    backend: "keyfile",
    schemas: Some("gnome"),
    network_monitor: BASE_MONITOR,
};
const GNOME_41: Session = Session {
    backend: "keyfile",
    schemas: Some("gnome-41"),
    network_monitor: BASE_MONITOR,
};
const INCOMPLETE_SCHEMA: Session = Session {
    backend: "keyfile",
    schemas: Some("incomplete"),
    network_monitor: BASE_MONITOR,
};
const FEWER_OPTIONS: Session = Session {
    backend: "keyfile",
    schemas: Some("fewer-options"),
    network_monitor: BASE_MONITOR,
};
const NOT_GNOME: Session = Session {
    backend: "keyfile",
    schemas: None,
    network_monitor: BASE_MONITOR,
};
const MEMORY_BACKEND: Session = Session {
    backend: "memory",
    schemas: Some("gnome"),
    network_monitor: BASE_MONITOR,
};
const GNOME_WITH_DEFAULT_MONITOR: Session = Session {
    network_monitor: None,
    ..GNOME
};

/// Defines a `#[test]` that runs an async scenario in its own process.
macro_rules! isolated_test {
    ($name:ident, $session:expr, $scenario:expr $(,)?) => {
        #[test]
        fn $name() {
            in_child_process(stringify!($name), &$session, $scenario);
        }
    };
}

isolated_test!(
    spawn_succeeds_on_gnome_and_reports_the_schema_defaults,
    GNOME,
    async |_sandbox| {
        let (gnome, _events) = spawn_gnome();

        let snapshot = gnome.snapshot().await;

        assert_eq!(
            snapshot,
            Ok(GnomeSnapshot {
                visible: Style::Light,
                keys: BackgroundKeys {
                    picture_uri: "file:///schema-default/light.png".to_owned(),
                    picture_uri_dark: Some("file:///schema-default/dark.png".to_owned()),
                    picture_options: "zoom".to_owned(),
                },
                locked: PerStyle::default(),
            })
        );
    }
);

isolated_test!(
    first_snapshot_holds_the_users_keys_and_spawn_writes_nothing,
    GNOME,
    async |sandbox| {
        let users_settings = "[org/gnome/desktop/background]\n\
            picture-uri='file:///user/light.png'\n\
            picture-uri-dark='file:///user/dark.png'\n\
            picture-options='none'\n\
            \n\
            [org/gnome/desktop/interface]\n\
            color-scheme='prefer-dark'\n";
        sandbox.write_settings(users_settings);
        let (gnome, _events) = spawn_gnome();

        let snapshot = gnome.snapshot().await;

        assert_eq!(
            snapshot,
            Ok(GnomeSnapshot {
                visible: Style::Dark,
                keys: BackgroundKeys {
                    picture_uri: "file:///user/light.png".to_owned(),
                    picture_uri_dark: Some("file:///user/dark.png".to_owned()),
                    picture_options: "none".to_owned(),
                },
                locked: PerStyle::default(),
            })
        );
        assert_eq!(sandbox.read_settings(), users_settings);
    }
);

isolated_test!(
    set_keys_is_on_disk_when_it_returns,
    GNOME,
    async |sandbox| {
        let (gnome, _events) = spawn_gnome();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        let on_disk = sandbox.read_settings();
        assert!(has_line(&on_disk, &format!("picture-uri='{LIGHT_SLOT}'")));
        assert!(has_line(
            &on_disk,
            &format!("picture-uri-dark='{DARK_SLOT}'")
        ));
    }
);

isolated_test!(
    set_keys_raises_one_event_that_holds_both_uris,
    GNOME,
    async |_sandbox| {
        let (gnome, mut events) = spawn_gnome();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        assert_eq!(
            next_event(&mut events).await,
            GnomeEvent::KeysChanged(BackgroundKeys {
                picture_uri: LIGHT_SLOT.to_owned(),
                picture_uri_dark: Some(DARK_SLOT.to_owned()),
                picture_options: "zoom".to_owned(),
            })
        );
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    set_keys_with_values_already_set_raises_no_event,
    GNOME,
    async |_sandbox| {
        let (gnome, mut events) = spawn_gnome();
        gnome.set_keys(slot_keys(None)).await.expect("set the keys");
        next_event(&mut events).await;

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    set_keys_with_values_already_set_does_not_rewrite_the_keyfile,
    GNOME,
    async |sandbox| {
        let (gnome, _events) = spawn_gnome();
        gnome.set_keys(slot_keys(None)).await.expect("set the keys");
        let inode_before = sandbox.keyfile_inode();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        // The keyfile backend writes a new file on every commit.
        assert_eq!(sandbox.keyfile_inode(), inode_before);
    }
);

isolated_test!(
    set_keys_turns_picture_options_none_into_zoom,
    GNOME,
    async |sandbox| {
        sandbox.write_settings("[org/gnome/desktop/background]\npicture-options='none'\n");
        let (gnome, _events) = spawn_gnome();

        gnome
            .set_keys(slot_keys(Some(PictureOptions::Zoom)))
            .await
            .expect("set the keys");

        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(keys.picture_options, "zoom");
    }
);

isolated_test!(
    restore_keys_writes_the_users_values_as_they_are,
    GNOME,
    async |_sandbox| {
        let (gnome, _events) = spawn_gnome();
        gnome
            .set_keys(slot_keys(Some(PictureOptions::Zoom)))
            .await
            .expect("set the keys");
        let users_keys = BackgroundKeys {
            picture_uri: "file:///user/light.png".to_owned(),
            picture_uri_dark: Some("file:///user/dark.png".to_owned()),
            // A nick that the daemon itself never writes.
            picture_options: "none".to_owned(),
        };

        gnome
            .restore_keys(users_keys.clone())
            .await
            .expect("restore the keys");

        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(keys, users_keys);
    }
);

isolated_test!(
    restore_keys_without_a_dark_value_leaves_the_dark_key_alone,
    GNOME,
    async |_sandbox| {
        let (gnome, _events) = spawn_gnome();
        gnome.set_keys(slot_keys(None)).await.expect("set the keys");
        let light_only = BackgroundKeys {
            picture_uri: "file:///user/light.png".to_owned(),
            picture_uri_dark: None,
            picture_options: "centered".to_owned(),
        };

        gnome
            .restore_keys(light_only)
            .await
            .expect("restore the keys");

        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(keys.picture_uri, "file:///user/light.png");
        assert_eq!(keys.picture_uri_dark.as_deref(), Some(DARK_SLOT));
        assert_eq!(keys.picture_options, "centered");
    }
);

isolated_test!(
    set_keys_without_picture_options_leaves_them_alone,
    GNOME,
    async |sandbox| {
        sandbox.write_settings("[org/gnome/desktop/background]\npicture-options='centered'\n");
        let (gnome, _events) = spawn_gnome();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(keys.picture_options, "centered");
    }
);

isolated_test!(
    set_keys_for_one_style_leaves_the_other_key_alone,
    GNOME,
    async |sandbox| {
        sandbox.write_settings(
            "[org/gnome/desktop/background]\n\
             picture-uri='file:///user/light.png'\n\
             picture-uri-dark='file:///user/dark.png'\n",
        );
        let (gnome, _events) = spawn_gnome();
        let light_key_only = KeyWrite {
            uris: PerStyle {
                light: Some(LIGHT_SLOT.to_owned()),
                dark: None,
            },
            picture_options: None,
        };

        gnome
            .set_keys(light_key_only)
            .await
            .expect("set the light key");

        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(keys.picture_uri, LIGHT_SLOT);
        assert_eq!(
            keys.picture_uri_dark.as_deref(),
            Some("file:///user/dark.png")
        );
    }
);

isolated_test!(
    external_change_of_two_keys_raises_one_event,
    GNOME,
    async |sandbox| {
        let (_gnome, mut events) = spawn_gnome();

        sandbox.write_settings(
            "[org/gnome/desktop/background]\n\
             picture-uri='file:///user/light.png'\n\
             picture-uri-dark='file:///user/dark.png'\n",
        );

        assert_eq!(
            next_event(&mut events).await,
            GnomeEvent::KeysChanged(BackgroundKeys {
                picture_uri: "file:///user/light.png".to_owned(),
                picture_uri_dark: Some("file:///user/dark.png".to_owned()),
                picture_options: "zoom".to_owned(),
            })
        );
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    external_change_after_an_own_write_is_still_reported,
    GNOME,
    async |sandbox| {
        let (gnome, mut events) = spawn_gnome();
        gnome.set_keys(slot_keys(None)).await.expect("set the keys");
        next_event(&mut events).await;

        sandbox.write_settings(&format!(
            "[org/gnome/desktop/background]\n\
             picture-uri='file:///user/light.png'\n\
             picture-uri-dark='{DARK_SLOT}'\n"
        ));

        assert_eq!(
            next_event(&mut events).await,
            GnomeEvent::KeysChanged(BackgroundKeys {
                picture_uri: "file:///user/light.png".to_owned(),
                picture_uri_dark: Some(DARK_SLOT.to_owned()),
                picture_options: "zoom".to_owned(),
            })
        );
    }
);

isolated_test!(prefer_dark_raises_a_style_event, GNOME, async |sandbox| {
    let (_gnome, mut events) = spawn_gnome();

    sandbox.write_settings("[org/gnome/desktop/interface]\ncolor-scheme='prefer-dark'\n");

    assert_eq!(
        next_event(&mut events).await,
        GnomeEvent::StyleChanged(Style::Dark)
    );
    assert_quiet(&mut events).await;
});

isolated_test!(
    prefer_light_keeps_the_light_style_and_raises_no_style_event,
    GNOME,
    async |sandbox| {
        let (_gnome, mut events) = spawn_gnome();

        // The key change in the same commit proves the adapter saw it.
        sandbox.write_settings(
            "[org/gnome/desktop/background]\n\
             picture-uri='file:///user/light.png'\n\
             \n\
             [org/gnome/desktop/interface]\n\
             color-scheme='prefer-light'\n",
        );

        assert!(matches!(
            next_event(&mut events).await,
            GnomeEvent::KeysChanged(_)
        ));
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    session_without_the_background_schema_is_rejected,
    NOT_GNOME,
    async |_sandbox| {
        assert_eq!(
            spawn_error(),
            GnomeError::SchemaMissing {
                schema: "org.gnome.desktop.background".to_owned(),
            }
        );
    }
);

isolated_test!(
    background_schema_without_picture_options_is_rejected,
    INCOMPLETE_SCHEMA,
    async |_sandbox| {
        assert_eq!(
            spawn_error(),
            GnomeError::SchemaMissing {
                schema: "org.gnome.desktop.background with key picture-options".to_owned(),
            }
        );
    }
);

isolated_test!(
    memory_backend_is_rejected,
    MEMORY_BACKEND,
    async |_sandbox| {
        assert_eq!(
            spawn_error(),
            GnomeError::VolatileBackend {
                backend: "GMemorySettingsBackend".to_owned(),
            }
        );
    }
);

isolated_test!(
    gnome_41_has_no_dark_key_and_is_always_light,
    GNOME_41,
    async |_sandbox| {
        let (gnome, _events) = spawn_gnome();

        let snapshot = gnome.snapshot().await;

        assert_eq!(
            snapshot,
            Ok(GnomeSnapshot {
                visible: Style::Light,
                keys: BackgroundKeys {
                    picture_uri: "file:///schema-default/light.png".to_owned(),
                    picture_uri_dark: None,
                    picture_options: "zoom".to_owned(),
                },
                locked: PerStyle::default(),
            })
        );
    }
);

isolated_test!(
    gnome_41_gets_only_the_light_key,
    GNOME_41,
    async |sandbox| {
        let (gnome, _events) = spawn_gnome();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        let on_disk = sandbox.read_settings();
        assert!(has_line(&on_disk, &format!("picture-uri='{LIGHT_SLOT}'")));
        assert!(!on_disk.contains("picture-uri-dark"));
    }
);

isolated_test!(
    locked_keys_that_differ_are_reported_and_left_alone,
    GNOME,
    async |sandbox| {
        let users_settings = "[org/gnome/desktop/background]\n\
            picture-uri='file:///admin/light.png'\n\
            picture-uri-dark='file:///admin/dark.png'\n";
        sandbox.write_settings(users_settings);
        sandbox.lock_settings();
        let (gnome, _events) = spawn_gnome();

        let written = gnome.set_keys(slot_keys(None)).await;

        assert_eq!(
            written,
            Err(GnomeError::Write {
                reason: "locked keys: picture-uri, picture-uri-dark".to_owned(),
                unstored: vec!["picture-uri".to_owned(), "picture-uri-dark".to_owned()],
            })
        );
        assert_eq!(sandbox.read_settings(), users_settings);
    }
);

isolated_test!(a_snapshot_names_the_locked_keys, GNOME, async |sandbox| {
    sandbox.write_settings("[org/gnome/desktop/background]\n");
    sandbox.lock_settings();
    let (gnome, _events) = spawn_gnome();

    let snapshot = gnome.snapshot().await.expect("a snapshot");

    let both = PerStyle {
        light: true,
        dark: true,
    };
    assert_eq!(snapshot.locked, both);
});

isolated_test!(
    a_lock_added_at_runtime_is_reported_once,
    GNOME,
    async |sandbox| {
        sandbox.write_settings("[org/gnome/desktop/background]\n");
        let (_gnome, mut events) = spawn_gnome();

        sandbox.lock_settings();

        let both = PerStyle {
            light: true,
            dark: true,
        };
        assert_eq!(
            next_event(&mut events).await,
            GnomeEvent::LocksChanged(both)
        );
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    a_lock_lifted_at_runtime_is_reported_once,
    GNOME,
    async |sandbox| {
        sandbox.write_settings("[org/gnome/desktop/background]\n");
        sandbox.lock_settings();
        let (_gnome, mut events) = spawn_gnome();

        sandbox.unlock_settings();

        assert_eq!(
            next_event(&mut events).await,
            GnomeEvent::LocksChanged(PerStyle::default())
        );
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    locked_keys_that_already_point_at_the_slots_are_accepted,
    GNOME,
    async |sandbox| {
        sandbox.write_settings(&format!(
            "[org/gnome/desktop/background]\n\
             picture-uri='{LIGHT_SLOT}'\n\
             picture-uri-dark='{DARK_SLOT}'\n"
        ));
        sandbox.lock_settings();
        let (gnome, _events) = spawn_gnome();

        let written = gnome.set_keys(slot_keys(None)).await;

        assert_eq!(written, Ok(()));
    }
);

isolated_test!(
    value_the_schema_refuses_fails_the_write_and_changes_no_key,
    FEWER_OPTIONS,
    async |_sandbox| {
        let (gnome, _events) = spawn_gnome();

        let written = gnome
            .set_keys(slot_keys(Some(PictureOptions::Spanned)))
            .await;

        assert!(
            matches!(&written, Err(GnomeError::Write { reason, .. }) if reason.starts_with("picture-options: ")),
            "{written:?}"
        );
        let keys = gnome.snapshot().await.expect("snapshot").keys;
        assert_eq!(
            keys,
            BackgroundKeys {
                picture_uri: "file:///schema-default/light.png".to_owned(),
                picture_uri_dark: Some("file:///schema-default/dark.png".to_owned()),
                picture_options: "zoom".to_owned(),
            }
        );
    }
);

isolated_test!(
    dropping_the_handle_stops_the_thread,
    GNOME,
    async |_sandbox| {
        let (gnome, mut events) = spawn_gnome();

        drop(gnome);

        // The thread owns the only sender, so the channel ends with it.
        let end = timeout(EVENT_TIMEOUT, events.recv()).await;
        assert_eq!(end, Ok(None));
    }
);

isolated_test!(
    spawn_queues_the_connectivity_at_start_once,
    GNOME,
    async |_sandbox| {
        let (_gnome, mut events) = spawn_gnome_with_events();

        assert_eq!(events.try_recv(), Ok(FULL_CONNECTIVITY));
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    connectivity_at_start_comes_before_key_changes,
    GNOME,
    async |_sandbox| {
        let (gnome, mut events) = spawn_gnome_with_events();

        gnome.set_keys(slot_keys(None)).await.expect("set the keys");

        assert_eq!(next_event(&mut events).await, FULL_CONNECTIVITY);
        assert!(matches!(
            next_event(&mut events).await,
            GnomeEvent::KeysChanged(_)
        ));
        assert_quiet(&mut events).await;
    }
);

isolated_test!(
    rejected_session_reports_no_connectivity,
    NOT_GNOME,
    async |_sandbox| {
        let (sender, mut events) = mpsc::unbounded_channel();

        GnomeThread::spawn(sender).expect_err("the session must be rejected");

        // Only the thread held the sender, and it is gone.
        assert_eq!(events.recv().await, None);
    }
);

// GLib picks the backend this machine offers (netlink, NetworkManager or
// systemd), so the level is whatever this machine has. The point is that a
// real backend starts and stops with the thread.
isolated_test!(
    default_monitor_reports_once_and_stops_with_the_thread,
    GNOME_WITH_DEFAULT_MONITOR,
    async |_sandbox| {
        let (gnome, mut events) = spawn_gnome_with_events();
        assert!(matches!(
            events.try_recv(),
            Ok(GnomeEvent::ConnectivityChanged(_))
        ));

        drop(gnome);

        // Some backends settle their level after start; such an update may
        // still arrive before the channel ends.
        let end = timeout(EVENT_TIMEOUT, async {
            while let Some(event) = events.recv().await {
                assert!(
                    matches!(event, GnomeEvent::ConnectivityChanged(_)),
                    "{event:?}"
                );
            }
        })
        .await;
        assert_eq!(end, Ok(()));
    }
);

/// The temp directory of the running scenario.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn keyfile(&self) -> PathBuf {
        self.root.join(SETTINGS_DIR).join("keyfile")
    }

    /// Replaces the keyfile in one rename, the way a settings tool commits.
    fn write_settings(&self, keyfile_text: &str) {
        let keyfile = self.keyfile();
        fs::create_dir_all(self.root.join(SETTINGS_DIR)).expect("create the settings dir");
        let staged = self.root.join("keyfile.staged");
        fs::write(&staged, keyfile_text).expect("stage the keyfile");
        fs::rename(&staged, &keyfile).expect("replace the keyfile");
    }

    fn read_settings(&self) -> String {
        fs::read_to_string(self.keyfile()).expect("read the keyfile")
    }

    fn keyfile_inode(&self) -> u64 {
        fs::metadata(self.keyfile())
            .expect("stat the keyfile")
            .ino()
    }

    /// Makes every key read-only, as an administrator's lock does. The
    /// keyfile backend takes a read-only settings directory as "locked".
    /// This needs a non-root user: root can write to such a directory.
    fn lock_settings(&self) {
        set_mode(&self.root.join(SETTINGS_DIR), 0o500);
    }

    /// Lifts the lock of [`Sandbox::lock_settings`].
    fn unlock_settings(&self) {
        set_mode(&self.root.join(SETTINGS_DIR), 0o700);
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("change permissions");
}

/// Runs `scenario` in a child process whose GSettings world is `session`.
///
/// The parent call starts this same test again as a child and checks that it
/// passed. The child call, told apart by `SANDBOX_VAR`, runs the scenario.
fn in_child_process(test_name: &str, session: &Session, scenario: impl AsyncFnOnce(Sandbox)) {
    match std::env::var_os(SANDBOX_VAR) {
        Some(root) => run_scenario(PathBuf::from(root), scenario),
        None => expect_child_to_pass(test_name, session),
    }
}

fn run_scenario(root: PathBuf, scenario: impl AsyncFnOnce(Sandbox)) {
    // Never run a scenario against real settings: the sandbox variable alone
    // is not proof that the parent prepared this process.
    let backend = std::env::var("GSETTINGS_BACKEND").unwrap_or_default();
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    assert!(
        matches!(backend.as_str(), "keyfile" | "memory")
            && config_home == Some(root.join("config")),
        "refusing to run outside the sandbox"
    );

    let ran_marker = root.join(SCENARIO_RAN);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build a tokio runtime");
    runtime.block_on(scenario(Sandbox { root }));
    fs::write(ran_marker, "").expect("mark the scenario as run");
}

fn expect_child_to_pass(test_name: &str, session: &Session) {
    let root = tempfile::tempdir().expect("create a temp dir");
    let mut child = Command::new(std::env::current_exe().expect("path of the test binary"));
    child.args([test_name, "--exact", "--nocapture"]);
    enter_sandbox(&mut child, root.path(), session);
    let output = child.output().expect("run the child test process");
    let settings_dir = root.path().join(SETTINGS_DIR);
    if settings_dir.exists() {
        // A locked directory would keep the temp dir from being deleted.
        set_mode(&settings_dir, 0o700);
    }

    let child_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "scenario failed:\n{child_log}");
    assert!(
        root.path().join(SCENARIO_RAN).exists(),
        "the child process ran no scenario:\n{child_log}"
    );
}

/// Points every directory and backend choice GSettings reads into `root`.
fn enter_sandbox(child: &mut Command, root: &Path, session: &Session) {
    let directories = [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        // Hides the schemas installed on this machine.
        ("XDG_DATA_DIRS", "system-data"),
    ];
    for (variable, name) in directories {
        let directory = root.join(name);
        fs::create_dir(&directory).expect("create a sandbox directory");
        child.env(variable, directory);
    }
    child
        .env(SANDBOX_VAR, root)
        .env("GSETTINGS_BACKEND", session.backend)
        // If GLib ever fell back to dconf, it must not find the user's
        // dconf service.
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", root.join("no-bus").display()),
        );
    match session.schemas {
        Some(fixture) => child.env("GSETTINGS_SCHEMA_DIR", compile_schemas(fixture, root)),
        None => child.env_remove("GSETTINGS_SCHEMA_DIR"),
    };
    match session.network_monitor {
        Some(backend) => child.env("GIO_USE_NETWORK_MONITOR", backend),
        None => child.env_remove("GIO_USE_NETWORK_MONITOR"),
    };
}

fn compile_schemas(fixture: &str, root: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/schemas")
        .join(fixture);
    let compiled = root.join("schemas");
    fs::create_dir(&compiled).expect("create the schema directory");
    let status = Command::new("glib-compile-schemas")
        .arg("--strict")
        .arg("--targetdir")
        .arg(&compiled)
        .arg(&source)
        .status()
        .expect("run glib-compile-schemas, which ships with GLib");
    assert!(status.success(), "glib-compile-schemas rejected {fixture}");
    compiled
}

/// Starts the GLib thread and takes the connectivity event it sends at
/// start, so a scenario sees only the events its own actions raise.
fn spawn_gnome() -> (GnomeThread, UnboundedReceiver<GnomeEvent>) {
    let (gnome, mut events) = spawn_gnome_with_events();
    assert_eq!(events.try_recv(), Ok(FULL_CONNECTIVITY));
    (gnome, events)
}

fn spawn_gnome_with_events() -> (GnomeThread, UnboundedReceiver<GnomeEvent>) {
    let (sender, events) = mpsc::unbounded_channel();
    let gnome = GnomeThread::spawn(sender).expect("start the GNOME thread");
    (gnome, events)
}

fn spawn_error() -> GnomeError {
    let (sender, _events) = mpsc::unbounded_channel();
    GnomeThread::spawn(sender).expect_err("the session must be rejected")
}

fn slot_keys(picture_options: Option<PictureOptions>) -> KeyWrite {
    KeyWrite {
        uris: PerStyle {
            light: Some(LIGHT_SLOT.to_owned()),
            dark: Some(DARK_SLOT.to_owned()),
        },
        picture_options,
    }
}

fn has_line(text: &str, wanted: &str) -> bool {
    text.lines().any(|line| line == wanted)
}

async fn next_event(events: &mut UnboundedReceiver<GnomeEvent>) -> GnomeEvent {
    timeout(EVENT_TIMEOUT, events.recv())
        .await
        .expect("an event within the timeout")
        .expect("the GNOME thread to be alive")
}

async fn assert_quiet(events: &mut UnboundedReceiver<GnomeEvent>) {
    let extra = timeout(QUIET_PERIOD, events.recv()).await;
    assert!(extra.is_err(), "unexpected event: {extra:?}");
}
