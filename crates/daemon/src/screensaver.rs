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

use futures_util::StreamExt;
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

/// What the watch hears on the bus.
enum Heard {
    /// `ActiveChanged`: the shield went up (`true`) or down.
    Active(bool),
    /// `org.gnome.ScreenSaver` got a new owner (`true`) or lost it.
    Owner(bool),
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
            Ok(args) => Some(Heard::Active(args.active)),
            Err(error) => {
                tracing::warn!(reason = %error, "malformed ActiveChanged signal ignored");
                None
            }
        }
    });
    let owners = owners.map(|owner| Heard::Owner(owner.is_some()));
    let mut heard = std::pin::pin!(futures_util::stream::select(changes, owners));
    if let Some(active) = ask_active(&proxy).await
        && sender.send(lock_state(active)).is_err()
    {
        return;
    }
    while let Some(news) = heard.next().await {
        let active = match news {
            Heard::Active(active) => active,
            // A new shield starts in its own state and announces only
            // changes: ask it.
            Heard::Owner(true) => match ask_active(&proxy).await {
                Some(active) => active,
                None => continue,
            },
            // No shield hides the screen any more.
            Heard::Owner(false) => false,
        };
        // The event loop is gone: nobody listens.
        if sender.send(lock_state(active)).is_err() {
            return;
        }
    }
}

/// Asks the shield whether it is up. `None` when no shield answers, which
/// is logged.
async fn ask_active(proxy: &ScreenSaverProxy<'_>) -> Option<bool> {
    match proxy.get_active().await {
        Ok(active) => Some(active),
        Err(error) => {
            tracing::info!(
                reason = %error,
                "org.gnome.ScreenSaver not available: pause_when_locked waits for it"
            );
            None
        }
    }
}

fn lock_state(active: bool) -> ScreenLock {
    if active {
        ScreenLock::Locked
    } else {
        ScreenLock::Unlocked
    }
}
