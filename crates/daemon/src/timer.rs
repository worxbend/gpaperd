//! Wall-clock timer on a `timerfd`.
//!
//! `tokio::time` counts `CLOCK_MONOTONIC`, which stops during suspend. An
//! absolute `CLOCK_REALTIME` deadline that passes during suspend fires right
//! after resume. See the plan's "Scheduling and time" section.

use std::io;
use std::os::fd::OwnedFd;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::event::Fired;
use rustix::io::Errno;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

/// One `timerfd` on `CLOCK_REALTIME`, armed for one absolute deadline.
#[derive(Debug)]
pub struct WallTimer {
    fd: AsyncFd<OwnedFd>,
    /// A clock step that the kernel reported to [`WallTimer::arm`], kept
    /// until [`WallTimer::wait`] returns it.
    clock_step: Notify,
}

impl WallTimer {
    /// Creates a disarmed timer.
    ///
    /// # Panics
    ///
    /// When called outside a tokio runtime.
    pub fn new() -> io::Result<WallTimer> {
        let fd = timerfd_create(
            TimerfdClockId::Realtime,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        // SAFETY: `fd` is an open timerfd. The `AsyncFd` owns it from here
        // on, so nothing can close or replace it before the `AsyncFd` is
        // dropped.
        let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }?;
        Ok(WallTimer {
            fd,
            clock_step: Notify::new(),
        })
    }

    /// Arms the timer for `deadline`, replacing any earlier one. `None`
    /// disarms it.
    ///
    /// Uses `TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET`. A deadline in the
    /// past fires at once. A deadline at or before the Unix epoch is clamped
    /// to 1 ns after it, because a zero value would disarm the timer.
    ///
    /// A clock step that the kernel reports here instead of to a waiting
    /// `read` is not an error: the timer is armed, and the next
    /// [`WallTimer::wait`] returns [`Fired::ClockChanged`].
    pub fn arm(&self, deadline: Option<SystemTime>) -> io::Result<()> {
        let (flags, expiry) = match deadline {
            Some(deadline) => (
                TimerfdTimerFlags::ABSTIME | TimerfdTimerFlags::CANCEL_ON_SET,
                expiry_of(deadline),
            ),
            // Without CANCEL_ON_SET a clock step does not wake a disarmed
            // timer. A zero expiry disarms.
            None => (TimerfdTimerFlags::empty(), Timespec::default()),
        };
        let setting = Itimerspec {
            it_interval: Timespec::default(),
            it_value: expiry,
        };
        match timerfd_settime(self.fd.get_ref(), flags, &setting) {
            Ok(_) => Ok(()),
            // timerfd_create(2), NOTES: the clock was stepped since the last
            // arm. The kernel says so once, here, and arms the timer anyway.
            Err(Errno::CANCELED) => {
                self.clock_step.notify_one();
                Ok(())
            }
            Err(errno) => Err(errno.into()),
        }
    }

    /// Waits until the timer fires. Pends forever while the timer is
    /// disarmed, unless an earlier [`WallTimer::arm`] saw a clock step that
    /// was not returned yet. Cancel-safe: dropping the future loses nothing.
    ///
    /// Returns [`Fired::Deadline`] when the deadline passed, and
    /// [`Fired::ClockChanged`] when the wall clock was stepped (`read` fails
    /// with `ECANCELED`); the caller then arms the timer again.
    pub async fn wait(&self) -> io::Result<Fired> {
        tokio::select! {
            biased;
            () = self.clock_step.notified() => Ok(Fired::ClockChanged),
            fired = self.read_timerfd() => fired,
        }
    }

    /// Waits for the next expiry or clock step that the timerfd reports.
    async fn read_timerfd(&self) -> io::Result<Fired> {
        loop {
            let mut guard = self.fd.readable().await?;
            let mut expirations = [0u8; 8];
            match rustix::io::read(self.fd.get_ref(), &mut expirations) {
                Ok(_) => return Ok(Fired::Deadline),
                Err(Errno::CANCELED) => return Ok(Fired::ClockChanged),
                // Readiness left by an expiry that a later `arm` replaced.
                Err(Errno::AGAIN) => guard.clear_ready(),
                Err(errno) => return Err(errno.into()),
            }
        }
    }
}

/// The timerfd expiry of a deadline. A zero expiry would disarm the timer, so
/// a deadline at or before the Unix epoch becomes 1 ns after it.
fn expiry_of(deadline: SystemTime) -> Timespec {
    let since_epoch = deadline
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .max(Duration::from_nanos(1));
    Timespec {
        tv_sec: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: since_epoch.subsec_nanos().into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use tokio::time::timeout;

    use super::*;

    /// Longer than any scheduling delay a loaded test machine adds.
    const PATIENCE: Duration = Duration::from_secs(5);
    /// Long enough to tell "still waiting" from "fired late".
    const QUIET: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn fires_at_an_absolute_deadline_and_not_before() {
        let timer = WallTimer::new().unwrap();
        let deadline = SystemTime::now() + Duration::from_millis(150);

        timer.arm(Some(deadline)).unwrap();
        let fired = timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert_eq!(fired, Fired::Deadline);
        assert!(SystemTime::now() >= deadline);
    }

    #[tokio::test]
    async fn overdue_deadline_fires_at_once() {
        let timer = WallTimer::new().unwrap();

        timer
            .arm(Some(SystemTime::now() - Duration::from_secs(3600)))
            .unwrap();
        let fired = timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert_eq!(fired, Fired::Deadline);
    }

    #[tokio::test]
    async fn deadline_at_the_epoch_fires_instead_of_disarming() {
        let timer = WallTimer::new().unwrap();

        timer.arm(Some(UNIX_EPOCH)).unwrap();
        let fired = timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert_eq!(fired, Fired::Deadline);
    }

    #[tokio::test]
    async fn deadline_before_the_epoch_fires_at_once() {
        let timer = WallTimer::new().unwrap();

        timer
            .arm(Some(UNIX_EPOCH - Duration::from_secs(1)))
            .unwrap();
        let fired = timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert_eq!(fired, Fired::Deadline);
    }

    #[tokio::test]
    async fn a_deadline_a_week_ahead_is_armed_and_waits() {
        let timer = WallTimer::new().unwrap();
        let a_week = Duration::from_secs(7 * 24 * 60 * 60);

        timer.arm(Some(SystemTime::now() + a_week)).unwrap();

        assert!(timeout(QUIET, timer.wait()).await.is_err());
    }

    #[tokio::test]
    async fn new_timer_is_disarmed() {
        let timer = WallTimer::new().unwrap();

        assert!(timeout(QUIET, timer.wait()).await.is_err());
    }

    #[tokio::test]
    async fn arming_with_none_disarms_a_pending_deadline() {
        let timer = WallTimer::new().unwrap();
        timer
            .arm(Some(SystemTime::now() + Duration::from_millis(100)))
            .unwrap();

        timer.arm(None).unwrap();

        assert!(timeout(QUIET, timer.wait()).await.is_err());
    }

    #[tokio::test]
    async fn rearming_drops_an_expiry_that_nobody_waited_for() {
        let timer = WallTimer::new().unwrap();
        timer
            .arm(Some(SystemTime::now() - Duration::from_secs(1)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        timer
            .arm(Some(SystemTime::now() + Duration::from_secs(3600)))
            .unwrap();

        assert!(timeout(QUIET, timer.wait()).await.is_err());
    }

    #[tokio::test]
    async fn fires_once_per_armed_deadline() {
        let timer = WallTimer::new().unwrap();
        timer
            .arm(Some(SystemTime::now() - Duration::from_secs(1)))
            .unwrap();
        timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert!(timeout(QUIET, timer.wait()).await.is_err());
    }

    #[tokio::test]
    async fn fires_again_after_being_armed_again() {
        let timer = WallTimer::new().unwrap();
        timer
            .arm(Some(SystemTime::now() - Duration::from_secs(1)))
            .unwrap();
        timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        timer
            .arm(Some(SystemTime::now() + Duration::from_millis(50)))
            .unwrap();
        let fired = timeout(PATIENCE, timer.wait()).await.unwrap().unwrap();

        assert_eq!(fired, Fired::Deadline);
    }
}
