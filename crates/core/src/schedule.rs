//! Wall-clock deadline maths. Pure: `now` is always an argument.
//!
//! Rules are the plan's "Scheduling and time" section. The interval counts
//! the time a style is on screen.

use std::time::{Duration, SystemTime};

/// One style's rotation clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleClock {
    /// Start of the running interval. `None` until the first publish.
    pub last_change: Option<SystemTime>,
    /// On-screen time left. `Some` only while the style is hidden and its
    /// clock is stopped.
    pub remaining: Option<Duration>,
}

impl StyleClock {
    /// When the next rotation is due: `last_change + interval`.
    ///
    /// `None` when the clock is stopped (`remaining` is set) or has never
    /// started.
    pub fn deadline(&self, interval: Duration) -> Option<SystemTime> {
        if self.remaining.is_some() {
            return None;
        }
        self.last_change?.checked_add(interval)
    }

    /// Starts a fresh interval at `now`: the slot just changed. A stopped
    /// clock runs again.
    pub fn record_change(&mut self, now: SystemTime) {
        self.last_change = Some(now);
        self.remaining = None;
    }

    /// Stops the clock because the style left the screen: stores the time
    /// left until the deadline, zero when overdue.
    ///
    /// A clock that is already stopped, or that never started, stays as it
    /// is.
    pub fn hide(&mut self, now: SystemTime, interval: Duration) {
        if let Some(deadline) = self.deadline(interval) {
            let time_left = deadline.duration_since(now).unwrap_or(Duration::ZERO);
            self.remaining = Some(time_left);
        }
    }

    /// Restarts a stopped clock because the style came on screen, so that the
    /// deadline is `now + remaining`. A style switch never forces an immediate
    /// change.
    ///
    /// The time left is capped at `interval`, which a config change may have
    /// shortened while the clock was stopped. A running clock stays as it is.
    pub fn show(&mut self, now: SystemTime, interval: Duration) {
        let Some(time_left) = self.remaining.take() else {
            return;
        };
        let time_spent = interval.saturating_sub(time_left);
        self.last_change = Some(now.checked_sub(time_spent).unwrap_or(now));
    }

    /// Applies the clock-went-back rule: when `now < last_change`, sets
    /// `last_change = now`, a fresh interval.
    pub fn absorb_clock_step(&mut self, now: SystemTime) {
        if self
            .last_change
            .is_some_and(|last_change| now < last_change)
        {
            self.last_change = Some(now);
        }
    }
}
