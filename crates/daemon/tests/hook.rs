//! `on_change`: the command starts off the event loop, with its variables,
//! and what goes wrong with it is logged.
//!
//! Each command here is a short `sh` script that ends on its own within a
//! few seconds, so that no test leaves a process behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpaperd_core::catalog::ItemId;
use gpaperd_core::config::HookCommand;
use gpaperd_core::event::{HookEvent, HookRun};
use gpaperd_core::style::Style;
use gpaperd_daemon::hook::{Hooks, OVERLAP_WARNING_PERIOD};
use tempfile::TempDir;
use tracing::subscriber::DefaultGuard;

/// How often a test looks again for what a command did.
const LOOK_PERIOD: Duration = Duration::from_millis(10);
/// Looks before a test gives up: ten seconds.
const MAX_LOOKS: u32 = 1_000;
/// Bytes of log that one test keeps.
const MAX_LOG_BYTES: usize = 1 << 20;

/// What the hooks log on the test's thread.
#[derive(Clone, Default)]
struct LogBook {
    lines: Arc<Mutex<Vec<u8>>>,
}

impl LogBook {
    fn collect_this_threads_log(&self) -> DefaultGuard {
        let book = self.clone();
        let collector = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || book.clone())
            .finish();
        tracing::subscriber::set_default(collector)
    }

    fn text(&self) -> String {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&lines).into_owned()
    }

    fn logged(&self, text: &str) -> bool {
        self.text().contains(text)
    }

    fn times_logged(&self, text: &str) -> usize {
        self.text().matches(text).count()
    }
}

impl io::Write for LogBook {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() < MAX_LOG_BYTES {
            lines.extend_from_slice(line);
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs `test` on a runtime like the daemon's: one thread, with the I/O
/// driver that reaps the children.
fn on_runtime(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a tokio runtime")
        .block_on(test);
}

/// Waits until `holds` says yes, for ten seconds at most.
async fn eventually(what: &str, holds: impl Fn() -> bool) {
    for _ in 0..MAX_LOOKS {
        if holds() {
            return;
        }
        tokio::time::sleep(LOOK_PERIOD).await;
    }
    panic!("timed out waiting for {what}");
}

fn sh(script: &str, args: &[&Path]) -> HookCommand {
    let mut all = vec!["-c".to_owned(), script.to_owned()];
    all.extend(args.iter().map(|arg| arg.display().to_string()));
    HookCommand {
        program: PathBuf::from("/bin/sh"),
        args: all,
    }
}

fn hook_run(command: HookCommand, event: HookEvent) -> HookRun {
    HookRun {
        command,
        event,
        style: Style::Dark,
        item: ItemId::new("/walls/night sky.png"),
        path: Some(PathBuf::from("/walls/night sky.png")),
        visible: false,
    }
}

/// A script that writes its `GPAPERD_` variables, sorted, to the file in
/// `$0`, all at once.
const SAVE_VARIABLES: &str = r#"env | grep '^GPAPERD_' | sort > "$0.part" && mv "$0.part" "$0""#;

/// A script that runs until the file in `$0` exists, or for five seconds.
const WAIT_FOR_RELEASE: &str =
    r#"i=0; while [ ! -e "$0" ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done"#;

#[test]
fn the_command_gets_the_image_in_its_variables() {
    on_runtime(async {
        let dir = TempDir::new().expect("create a temp dir");
        let saved = dir.path().join("variables");
        let mut hooks = Hooks::default();

        let run = hook_run(sh(SAVE_VARIABLES, &[&saved]), HookEvent::Changed);
        hooks.run(run, Instant::now());

        eventually("the variables", || saved.exists()).await;
        let variables = fs::read_to_string(&saved).expect("read the variables");
        assert_eq!(
            variables,
            "GPAPERD_EVENT=changed\n\
             GPAPERD_ID=/walls/night sky.png\n\
             GPAPERD_PATH=/walls/night sky.png\n\
             GPAPERD_STYLE=dark\n\
             GPAPERD_VISIBLE=0\n"
        );
    });
}

#[test]
fn a_run_without_a_path_has_no_gpaperd_path() {
    on_runtime(async {
        let dir = TempDir::new().expect("create a temp dir");
        let saved = dir.path().join("variables");
        let mut hooks = Hooks::default();
        let run = HookRun {
            path: None,
            visible: true,
            ..hook_run(sh(SAVE_VARIABLES, &[&saved]), HookEvent::Switched)
        };

        hooks.run(run, Instant::now());

        eventually("the variables", || saved.exists()).await;
        let variables = fs::read_to_string(&saved).expect("read the variables");
        assert_eq!(
            variables,
            "GPAPERD_EVENT=switched\n\
             GPAPERD_ID=/walls/night sky.png\n\
             GPAPERD_STYLE=dark\n\
             GPAPERD_VISIBLE=1\n"
        );
    });
}

#[test]
fn a_program_that_does_not_start_is_logged_with_its_reason() {
    let log = LogBook::default();
    let _route = log.collect_this_threads_log();
    on_runtime(async {
        let mut hooks = Hooks::default();
        let command = HookCommand {
            program: PathBuf::from("/nonexistent/wallpaper-hook"),
            args: Vec::new(),
        };

        hooks.run(hook_run(command, HookEvent::Changed), Instant::now());

        let text = log.text();
        assert!(text.contains("on_change did not start"), "{text}");
        assert!(text.contains("/nonexistent/wallpaper-hook"), "{text}");
        assert!(text.contains("No such file or directory"), "{text}");
    });
}

#[test]
fn a_failing_command_is_logged_with_its_exit_status_and_not_its_variables() {
    let log = LogBook::default();
    let _route = log.collect_this_threads_log();
    on_runtime(async {
        let mut hooks = Hooks::default();

        hooks.run(
            hook_run(sh("exit 3", &[]), HookEvent::Changed),
            Instant::now(),
        );

        eventually("the failure in the log", || log.logged("on_change failed")).await;
        let text = log.text();
        assert!(text.contains("exit status: 3"), "{text}");
        assert!(!text.contains("night sky"), "{text}");
    });
}

#[test]
fn overlapping_runs_of_one_event_kind_warn_at_most_once_a_minute() {
    let log = LogBook::default();
    let _route = log.collect_this_threads_log();
    on_runtime(async {
        let dir = TempDir::new().expect("create a temp dir");
        let release = dir.path().join("release");
        let waits = || sh(WAIT_FOR_RELEASE, &[&release]);
        let mut hooks = Hooks::default();
        let start = Instant::now();
        let overlapping = "on_change starts while earlier runs still run";

        hooks.run(hook_run(waits(), HookEvent::Changed), start);
        hooks.run(hook_run(waits(), HookEvent::Switched), start);
        assert_eq!(log.times_logged(overlapping), 0, "{}", log.text());

        hooks.run(hook_run(waits(), HookEvent::Changed), start);
        hooks.run(
            hook_run(waits(), HookEvent::Changed),
            start + OVERLAP_WARNING_PERIOD / 2,
        );
        assert_eq!(log.times_logged(overlapping), 1, "{}", log.text());

        hooks.run(
            hook_run(waits(), HookEvent::Changed),
            start + OVERLAP_WARNING_PERIOD,
        );
        assert_eq!(log.times_logged(overlapping), 2, "{}", log.text());

        fs::write(&release, b"").expect("release the runs");
    });
}

#[test]
fn a_run_that_ended_does_not_count_as_overlapping() {
    let log = LogBook::default();
    let _route = log.collect_this_threads_log();
    on_runtime(async {
        let mut hooks = Hooks::default();
        hooks.run(
            hook_run(sh("true", &[]), HookEvent::Changed),
            Instant::now(),
        );
        eventually("the end of the first run", || log.logged("on_change ended")).await;

        hooks.run(
            hook_run(sh("true", &[]), HookEvent::Changed),
            Instant::now(),
        );

        let text = log.text();
        assert!(!text.contains("earlier runs still run"), "{text}");
    });
}
