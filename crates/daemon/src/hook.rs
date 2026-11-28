//! `on_change`: starts the user's command when the image on screen changes.
//!
//! The command runs with no shell. Its stdin is `/dev/null`; its stdout and
//! stderr are the daemon's, which is the journal under systemd. The event
//! loop never waits for it: a task of its own reaps each child, and logs a
//! non-zero exit. The daemon does not kill a run that outlives it.

use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gpaperd_core::event::{HookEvent, HookRun};
use tokio::process::{Child, Command};

/// The shortest time between two warnings about runs of one event kind
/// that overlap.
pub const OVERLAP_WARNING_PERIOD: Duration = Duration::from_secs(60);

/// Starts the runs of `on_change` and keeps count of those still running.
#[derive(Default)]
pub struct Hooks {
    changed: Overlap,
    switched: Overlap,
}

/// The runs of one event kind that are still running.
#[derive(Default)]
struct Overlap {
    running: Arc<AtomicUsize>,
    last_warning: Option<Instant>,
}

impl Overlap {
    /// Warns that a run starts while an earlier one is still running, at
    /// most once per [`OVERLAP_WARNING_PERIOD`].
    fn warn_when_busy(&mut self, event: HookEvent, now: Instant) {
        let running = self.running.load(Ordering::Relaxed);
        if running == 0 {
            return;
        }
        let is_quiet = self
            .last_warning
            .is_none_or(|last| now.saturating_duration_since(last) >= OVERLAP_WARNING_PERIOD);
        if is_quiet {
            self.last_warning = Some(now);
            tracing::warn!(
                event = event.as_str(),
                running,
                "on_change starts while earlier runs still run"
            );
        }
    }
}

/// Counts a run until its child is reaped, or until its reaper is dropped
/// with the runtime.
struct RunningCount(Arc<AtomicUsize>);

impl RunningCount {
    fn start(running: &Arc<AtomicUsize>) -> RunningCount {
        running.fetch_add(1, Ordering::Relaxed);
        RunningCount(Arc::clone(running))
    }
}

impl Drop for RunningCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Hooks {
    /// Starts `run` and returns at once. A run whose earlier one of the
    /// same event kind still runs starts all the same. `now` dates the
    /// warning about that.
    ///
    /// A failure to start is logged, and so is a non-zero exit, with the
    /// program and the status. The environment is never logged.
    ///
    /// Must run inside a tokio runtime with its I/O driver enabled: the
    /// reaper waits for `SIGCHLD`.
    pub fn run(&mut self, run: HookRun, now: Instant) {
        let overlap = match run.event {
            HookEvent::Changed => &mut self.changed,
            HookEvent::Switched => &mut self.switched,
        };
        overlap.warn_when_busy(run.event, now);
        let program = run.command.program.display().to_string();
        let event = run.event.as_str();
        let child = match command(&run).spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%program, event, reason = %error, "on_change did not start");
                return;
            }
        };
        let count = RunningCount::start(&overlap.running);
        tokio::spawn(reap(child, count, program, event));
    }
}

/// Waits for `child` to exit and logs a failure. `_count` counts the run
/// as running until then.
async fn reap(mut child: Child, _count: RunningCount, program: String, event: &'static str) {
    match child.wait().await {
        Ok(status) if status.success() => tracing::debug!(%program, event, "on_change ended"),
        Ok(status) => tracing::warn!(%program, event, %status, "on_change failed"),
        Err(error) => tracing::warn!(%program, event, reason = %error, "on_change not reaped"),
    }
}

/// The command for `run`: the daemon's environment plus the `GPAPERD_`
/// variables. A `GPAPERD_PATH` that the daemon inherited never reaches a
/// run that has no path.
fn command(run: &HookRun) -> Command {
    let mut command = Command::new(&run.command.program);
    command
        .args(&run.command.args)
        .envs(environment(run))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    if run.path.is_none() {
        command.env_remove("GPAPERD_PATH");
    }
    command
}

fn environment(run: &HookRun) -> Vec<(&'static str, OsString)> {
    let visible = if run.visible { "1" } else { "0" };
    let mut variables = vec![
        ("GPAPERD_EVENT", OsString::from(run.event.as_str())),
        ("GPAPERD_STYLE", OsString::from(run.style.as_str())),
        ("GPAPERD_ID", OsString::from(run.item.as_str())),
        ("GPAPERD_VISIBLE", OsString::from(visible)),
    ];
    if let Some(path) = &run.path {
        variables.push(("GPAPERD_PATH", path.clone().into_os_string()));
    }
    variables
}
