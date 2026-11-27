//! Client side of gpaperd's D-Bus control interface: the proxy that
//! `gpaperctl` calls through. A library so that the daemon's D-Bus tests can
//! drive the server with the very client the CLI uses.
//!
//! The JSON that `Status()` returns is
//! [`gpaperd_core::control::StatusReport`]; parse it with `serde_json`.

#![forbid(unsafe_code)]

use std::io;
use std::time::Duration;

use zbus::proxy;

/// How long a client waits for the answer to a call. A little longer than
/// the daemon's own 30 s reply timeout, whose `Failed` answer comes first
/// whenever the daemon still runs its event loop. This one covers a daemon
/// that is stopped or stuck: a call to it would otherwise wait for ever.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(35);

/// Whether `error` is a call that got no answer within the connection's
/// method timeout.
#[must_use]
pub fn is_timeout(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(error) if error.kind() == io::ErrorKind::TimedOut)
}

/// The daemon's control interface, as the plan's "Control CLI and D-Bus API"
/// table lists it. Names match [`gpaperd_core::control::BUS_NAME`],
/// [`OBJECT_PATH`](gpaperd_core::control::OBJECT_PATH) and
/// [`INTERFACE`](gpaperd_core::control::INTERFACE).
///
/// `style` is `light`, `dark`, `visible` or `both`; anything else is an
/// `org.freedesktop.DBus.Error.InvalidArgs` error.
#[proxy(
    interface = "io.github.worxbend.Gpaperd1",
    default_service = "io.github.worxbend.Gpaperd1",
    default_path = "/io/github/worxbend/Gpaperd1"
)]
pub trait Gpaperd1 {
    /// Rotate now.
    fn next(&self, style: &str) -> zbus::Result<()>;
    /// Step back through the style's history.
    fn previous(&self, style: &str) -> zbus::Result<()>;
    /// Show the image `id` now: an absolute, canonical path or an `http` or
    /// `https` URL of the style's sources. Outside them, or neither, it is
    /// an `org.freedesktop.DBus.Error.InvalidArgs` error.
    fn set(&self, style: &str, id: &str) -> zbus::Result<()>;
    /// Stop the style's timer (a user pause).
    fn pause(&self, style: &str) -> zbus::Result<()>;
    /// End the user pause.
    fn resume(&self, style: &str) -> zbus::Result<()>;
    /// Re-read the config. `(true, "")` on success, else `(false, error)`
    /// with line and column.
    fn reload(&self) -> zbus::Result<(bool, String)>;
    /// Fetch every listed URL now.
    fn refresh_remote(&self) -> zbus::Result<()>;
    /// Write back the user's original keys and pause both styles.
    fn restore(&self) -> zbus::Result<()>;
    /// The `StatusReport` JSON document.
    fn status(&self) -> zbus::Result<String>;

    /// `"light"` or `"dark"`: the style on screen.
    #[zbus(property)]
    fn style(&self) -> zbus::Result<String>;
    /// The paused styles.
    #[zbus(property)]
    fn paused(&self) -> zbus::Result<Vec<String>>;
    /// The light slot's item id; `""` when none.
    #[zbus(property)]
    fn current_light(&self) -> zbus::Result<String>;
    /// The dark slot's item id; `""` when none.
    #[zbus(property)]
    fn current_dark(&self) -> zbus::Result<String>;
    /// Next rotation, seconds since the Unix epoch; `0` when none.
    #[zbus(property)]
    fn next_change(&self) -> zbus::Result<u64>;

    /// A new image is on screen in `style`: after every successful publish,
    /// and in uri mode after the key write that shows it.
    #[zbus(signal)]
    fn changed(&self, style: &str, id: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use gpaperd_core::control::{BUS_NAME, INTERFACE, OBJECT_PATH};
    use zbus::proxy::Defaults;

    use super::Gpaperd1Proxy;

    #[test]
    fn proxy_defaults_match_the_shared_names() {
        let interface = Gpaperd1Proxy::INTERFACE.as_ref().map(|name| name.as_str());
        let destination = Gpaperd1Proxy::DESTINATION
            .as_ref()
            .map(|name| name.as_str());
        let path = Gpaperd1Proxy::PATH.as_ref().map(|path| path.as_str());
        assert_eq!(interface, Some(INTERFACE));
        assert_eq!(destination, Some(BUS_NAME));
        assert_eq!(path, Some(OBJECT_PATH));
    }
}
