//! Whether the screen is locked, from `org.gnome.ScreenSaver` on the
//! session bus, for `pause_when_locked`.
//!
//! [`watch`] subscribes to `ActiveChanged` and to owner changes of the
//! name first and then asks `GetActive`, so no change between the two is
//! lost. An answer outranks the `ActiveChanged` signals its shield sent
//! before it and yields to those sent after: a connection numbers what it
//! sends in rising serials, so the serials order the two. A new owner is
//! asked `GetActive` again; a name without an owner
//! counts as unlocked, since no shield hides the screen. Each answer goes
//! to the interpreter as a [`ScreenLock`]. The core reads it only when the config
//! asks for it, so the watch runs whatever the config says.
//!
//! A session without the service (not GNOME, or a shell that has not
//! started it yet) is no error: the failed `GetActive` is logged and the
//! watch keeps listening, so a service that appears later is still heard.
//! A shield that gives no answer counts as unlocked and is asked again
//! after each D-Bus reply timeout. Those retries log only at debug.

use std::mem;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt};
use gpaperd_core::event::ScreenLock;
use tokio::sync::mpsc;
use zbus::names::UniqueName;
use zbus::proxy::CacheProperties;
use zbus::{Connection, Message, proxy};

/// GNOME's screen shield, as far as gpaperd needs it.
#[proxy(
    interface = "org.gnome.ScreenSaver",
    default_service = "org.gnome.ScreenSaver",
    default_path = "/org/gnome/ScreenSaver"
)]
pub trait ScreenSaver {
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
///
/// Its sender is leaked, not dropped: a closed channel would read as a
/// watch that stopped, and the loop would log it and feed an unlock.
pub fn detached() -> mpsc::UnboundedReceiver<ScreenLock> {
    let (sender, receiver) = mpsc::unbounded_channel();
    mem::forget(sender);
    receiver
}

/// What the watch hears on the bus.
enum Heard {
    /// `ActiveChanged`: the shield went up (`true`) or down.
    Active(bool, Sent),
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
    Known(Sent),
    Unknown,
}

/// Where a message stands among those its sender sent.
struct Sent {
    sender: Option<UniqueName<'static>>,
    serial: NonZeroU32,
}

impl Sent {
    fn of(message: &Message) -> Self {
        let header = message.header();
        Self {
            sender: header.sender().map(|name| name.to_owned()),
            serial: header.primary().serial_num(),
        }
    }

    /// Whether the peer that sent `later` sent this message before it.
    fn precedes(&self, later: &Sent) -> bool {
        self.sender == later.sender && self.serial < later.serial
    }
}

/// A `GetActive` reply.
struct Answer {
    active: bool,
    sent: Sent,
}

/// Why `GetActive` gave no answer.
#[derive(Debug, thiserror::Error)]
enum Unanswered {
    /// A busy shell may still answer a later ask.
    #[error("no reply within the D-Bus reply timeout")]
    TimedOut,
    /// An error reply, such as a name without an owner.
    #[error(transparent)]
    Failed(zbus::Error),
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
    let changes = changes.filter_map(|signal| async move {
        match signal.args() {
            Ok(args) => Some(Heard::Active(args.active, Sent::of(signal.message()))),
            Err(error) => {
                tracing::warn!(reason = %error, "malformed ActiveChanged signal ignored");
                None
            }
        }
    });
    let owners = owners.map(|owner| Heard::Owner(owner.is_some()));
    let mut changes = std::pin::pin!(changes);
    let mut owners = std::pin::pin!(owners);
    // The first ask is not a new owner, and the core starts unlocked, so a
    // failure sends nothing. The streams above were subscribed first. Only
    // a timeout is asked again: an error reply here means no shield yet,
    // and the owner change that brings one asks it.
    // The last answer: the signals its shield sent before it are older.
    let mut answered = None;
    let mut retry = match ask_active(&proxy).await {
        Ok(answer) => {
            if sender.send(lock_state(answer.active)).is_err() {
                return;
            }
            answered = Some(answer.sent);
            false
        }
        Err(unanswered) => {
            log_unavailable(&unanswered);
            matches!(unanswered, Unanswered::TimedOut)
        }
    };
    loop {
        let woke = tokio::select! {
            biased;
            () = pause_before_retry(), if retry => Woke::Retry,
            Some(news) = owners.next() => Woke::Heard(news),
            Some(news) = changes.next() => Woke::Heard(news),
            else => return,
        };
        match woke {
            Woke::Retry => match ask_active(&proxy).await {
                Ok(answer) => {
                    retry = false;
                    if sender.send(lock_state(answer.active)).is_err() {
                        return;
                    }
                    answered = Some(answer.sent);
                }
                // Unlocked already stands for this owner. A signal queued
                // meanwhile is this owner's and still applies.
                Err(unanswered) => {
                    tracing::debug!(
                        reason = %unanswered,
                        "org.gnome.ScreenSaver still not answering"
                    );
                }
            },
            Woke::Heard(Heard::Active(active, sent)) => {
                if answered
                    .as_ref()
                    .is_some_and(|answer| sent.precedes(answer))
                {
                    continue;
                }
                // Every later signal is newer still. Forgetting the answer
                // also keeps a serial that wraps around from looking old.
                answered = None;
                retry = false;
                if sender.send(lock_state(active)).is_err() {
                    return;
                }
            }
            Woke::Heard(Heard::Owner(false)) => {
                retry = false;
                if sender.send(ScreenLock::Unlocked).is_err() {
                    return;
                }
                // Queued under the owner that left. Delivered later, it
                // could lock a screen that no shield is hiding.
                discard_queued(changes.as_mut());
            }
            Woke::Heard(Heard::Owner(true)) => {
                // Queued under the previous owner: not this shield's state.
                discard_queued(changes.as_mut());
                match report_owner_lock(&proxy, &sender).await {
                    OwnerReport::Stop => return,
                    OwnerReport::Known(sent) => {
                        retry = false;
                        answered = Some(sent);
                    }
                    // This owner's signals queued during the ask still
                    // apply after the Unlocked.
                    OwnerReport::Unknown => retry = true,
                }
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
    let (state, report) = match ask_active(proxy).await {
        Ok(answer) => (lock_state(answer.active), OwnerReport::Known(answer.sent)),
        Err(unanswered) => {
            log_unavailable(&unanswered);
            (ScreenLock::Unlocked, OwnerReport::Unknown)
        }
    };
    if sender.send(state).is_err() {
        return OwnerReport::Stop;
    }
    report
}

/// `GetActive`, bounded by the D-Bus reply timeout. `gio` has no
/// cancellable on this call; dropping the future is what ends the wait.
async fn ask_active(proxy: &ScreenSaverProxy<'_>) -> Result<Answer, Unanswered> {
    let call = proxy.inner().call_method("GetActive", &());
    let reply = match tokio::time::timeout(crate::dbus::REPLY_TIMEOUT, call).await {
        Ok(reply) => reply.map_err(Unanswered::Failed)?,
        Err(_elapsed) => return Err(Unanswered::TimedOut),
    };
    let active = reply.body().deserialize().map_err(Unanswered::Failed)?;
    Ok(Answer {
        active,
        sent: Sent::of(&reply),
    })
}

fn log_unavailable(unanswered: &Unanswered) {
    tracing::info!(
        reason = %unanswered,
        "org.gnome.ScreenSaver not available: pause_when_locked waits for it"
    );
}

/// Drops the `ActiveChanged` signals already queued on `stream`, so the
/// next `select` does not apply them. A pending poll leaves the stream as
/// it was.
fn discard_queued<S: Stream + ?Sized>(mut stream: Pin<&mut S>) {
    let mut context = Context::from_waker(std::task::Waker::noop());
    while let Poll::Ready(Some(_)) = stream.as_mut().poll_next(&mut context) {}
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
    use tokio::sync::mpsc::error::TryRecvError;

    use super::detached;

    #[test]
    fn a_detached_watch_stays_open() {
        let mut lock = detached();
        assert_eq!(lock.try_recv(), Err(TryRecvError::Empty));
    }
}
