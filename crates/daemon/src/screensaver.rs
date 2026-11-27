//! Whether the screen is locked, from `org.gnome.ScreenSaver` on the
//! session bus, for `pause_when_locked`.
//!
//! [`watch`] subscribes to `ActiveChanged` and to owner changes of the
//! name first and then asks `GetActive`, so no change between the two is
//! lost. A new owner is asked `GetActive` again; a name without an owner
//! counts as unlocked, since no shield hides the screen. Each answer goes
//! to the interpreter as a [`ScreenLock`]. The core reads it only when the config
//! asks for it, so the watch runs whatever the config says.
//!
//! A session without the service (not GNOME, or a shell that has not
//! started it yet) is no error: the failed `GetActive` is logged once and
//! the watch keeps listening, so a service that appears later is still
//! heard.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt};
use gpaperd_core::event::ScreenLock;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;
use zbus::{Connection, proxy};

/// GNOME's screen shield, as far as gpaperd needs it.
#[proxy(
    interface = "org.gnome.ScreenSaver",
    default_service = "org.gnome.ScreenSaver",
    default_path = "/org/gnome/ScreenSaver"
)]
pub trait ScreenSaver {
    /// Whether the shield is up now.
    fn get_active(&self) -> zbus::Result<bool>;

    /// The shield went up (`true`) or down (`false`).
    #[zbus(signal)]
    fn active_changed(&self, active: bool) -> zbus::Result<()>;
}

/// Watches the screen lock on `connection` in a task of its own, and sends
/// every state it learns to the returned receiver: the state at start,
/// then each change. The task ends when the receiver is dropped or the
/// connection closes.
///
/// Must run inside a tokio runtime.
pub fn watch(connection: Connection) -> mpsc::UnboundedReceiver<ScreenLock> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(follow(connection, sender));
    receiver
}

/// A receiver that never yields: no session bus, so nothing to watch.
pub fn detached() -> mpsc::UnboundedReceiver<ScreenLock> {
    mpsc::unbounded_channel().1
}

/// Keeps an `ActiveChanged` only when it was sampled for the owner
/// generation the watch is still on. A signal polled under the previous
/// owner is dropped.
pub(crate) fn keep_signal(stamp: u64, generation: u64) -> bool {
    stamp == generation
}

/// What the watch hears on the bus.
enum Heard {
    /// `ActiveChanged`, stamped with the owner generation at the poll.
    Active { active: bool, stamp: u64 },
    /// `org.gnome.ScreenSaver` got a new owner (`true`) or lost it.
    Owner(bool),
}

enum Woke {
    Retry,
    Heard(Heard),
}

/// The new owner's `GetActive` was answered, was not, or the listener is gone.
enum OwnerReport {
    Stop,
    Known,
    Unknown,
}

async fn follow(connection: Connection, sender: mpsc::UnboundedSender<ScreenLock>) {
    let proxy = match ScreenSaverProxy::builder(&connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await
    {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::warn!(reason = %error, "screen lock not watched");
            return;
        }
    };
    let streams = futures_util::try_join!(
        proxy.receive_active_changed(),
        proxy.inner().receive_owner_changed()
    );
    let (changes, owners) = match streams {
        Ok(streams) => streams,
        Err(error) => {
            tracing::warn!(reason = %error, "screen lock not watched");
            return;
        }
    };
    let generation = Arc::new(AtomicU64::new(0));
    let stamped = Arc::clone(&generation);
    let changes = changes.filter_map(move |signal| {
        let stamp = stamped.load(Ordering::Acquire);
        async move {
            match signal.args() {
                Ok(args) => Some(Heard::Active {
                    active: args.active,
                    stamp,
                }),
                Err(error) => {
                    tracing::warn!(reason = %error, "malformed ActiveChanged signal ignored");
                    None
                }
            }
        }
    });
    let owners = owners.map(|owner| Heard::Owner(owner.is_some()));
    let mut changes = std::pin::pin!(changes);
    let mut owners = std::pin::pin!(owners);
    // The first ask is not a new owner. A failure stays quiet: the watch
    // keeps listening, and the streams above were subscribed first.
    if let Some(active) = ask_active(&proxy).await
        && sender.send(lock_state(active)).is_err()
    {
        return;
    }
    let mut retry = false;
    loop {
        let woke = tokio::select! {
            biased;
            () = pause_before_retry(), if retry => Woke::Retry,
            Some(news) = owners.next() => Woke::Heard(news),
            Some(news) = changes.next() => Woke::Heard(news),
            else => return,
        };
        match woke {
            Woke::Retry => {
                match report_owner_lock(&proxy, &sender).await {
                    OwnerReport::Stop => return,
                    OwnerReport::Known => retry = false,
                    OwnerReport::Unknown => retry = true,
                }
                // The timer won the select, so a signal queued during the
                // wait is still here. It must not replace the ask.
                discard_queued(changes.as_mut());
            }
            Woke::Heard(Heard::Active { active, stamp }) => {
                if !keep_signal(stamp, generation.load(Ordering::Acquire)) {
                    continue;
                }
                retry = false;
                if sender.send(lock_state(active)).is_err() {
                    return;
                }
            }
            Woke::Heard(Heard::Owner(false)) => {
                retry = false;
                generation.fetch_add(1, Ordering::Release);
                if sender.send(ScreenLock::Unlocked).is_err() {
                    return;
                }
                // A signal still queued under the owner that left would be
                // stamped with this generation on the next poll and could
                // lock a screen that no shield is hiding.
                discard_queued(changes.as_mut());
            }
            Woke::Heard(Heard::Owner(true)) => {
                generation.fetch_add(1, Ordering::Release);
                match report_owner_lock(&proxy, &sender).await {
                    OwnerReport::Stop => return,
                    OwnerReport::Known => retry = false,
                    OwnerReport::Unknown => retry = true,
                }
                // Queued under the previous owner, or while this ask ran.
                // Delivered on the next poll, it replaces `GetActive`.
                discard_queued(changes.as_mut());
            }
        }
    }
}

async fn pause_before_retry() {
    tokio::time::sleep(crate::dbus::REPLY_TIMEOUT).await;
}

/// Asks the new shield. Unknown does not keep the previous lock bit.
async fn report_owner_lock(
    proxy: &ScreenSaverProxy<'_>,
    sender: &mpsc::UnboundedSender<ScreenLock>,
) -> OwnerReport {
    let (state, known) = match ask_active(proxy).await {
        Some(active) => (lock_state(active), true),
        None => (ScreenLock::Unlocked, false),
    };
    if sender.send(state).is_err() {
        return OwnerReport::Stop;
    }
    if known {
        OwnerReport::Known
    } else {
        OwnerReport::Unknown
    }
}

/// `GetActive`, bounded by the D-Bus reply timeout. `gio` has no
/// cancellable on this call; dropping the future is what ends the wait.
/// `None` is logged. The first failure stays quiet at the caller.
async fn ask_active(proxy: &ScreenSaverProxy<'_>) -> Option<bool> {
    match tokio::time::timeout(crate::dbus::REPLY_TIMEOUT, proxy.get_active()).await {
        Ok(Ok(active)) => Some(active),
        Ok(Err(error)) => {
            tracing::info!(
                reason = %error,
                "org.gnome.ScreenSaver not available: pause_when_locked waits for it"
            );
            None
        }
        Err(_elapsed) => {
            tracing::info!(
                "org.gnome.ScreenSaver not available: pause_when_locked waits for it"
            );
            None
        }
    }
}

/// Drops `ActiveChanged` values already queued. A later poll is a later
/// signal. The stamp is taken at poll time, so leaving them for the next
/// `select` would treat a previous owner's bool as current.
fn discard_queued<S: Stream + ?Sized>(stream: Pin<&mut S>) {
    let _ = take_ready(stream);
}

/// Signals already queued on `stream`. A pending poll leaves them there.
fn take_ready<S: Stream + ?Sized>(mut stream: Pin<&mut S>) -> Vec<S::Item> {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(&waker);
    let mut ready = Vec::new();
    while let Poll::Ready(Some(item)) = stream.as_mut().poll_next(&mut context) {
        ready.push(item);
    }
    ready
}

fn lock_state(active: bool) -> ScreenLock {
    if active {
        ScreenLock::Locked
    } else {
        ScreenLock::Unlocked
    }
}

#[cfg(test)]
mod tests {
    use super::keep_signal;

    #[test]
    fn a_signal_from_the_previous_owner_generation_is_dropped() {
        assert!(keep_signal(0, 0));
        assert!(!keep_signal(0, 1));
    }
}
