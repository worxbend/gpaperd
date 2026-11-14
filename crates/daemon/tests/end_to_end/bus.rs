//! A private session bus for the control scenarios: one `dbus-daemon` per
//! scenario, killed when it is dropped. No scenario reaches the user's bus.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use gpaperd_ctl::Gpaperd1Proxy;
use zbus::proxy::CacheProperties;

pub struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    pub fn start() -> PrivateBus {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork", "--nopidfile"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start dbus-daemon");
        let stdout = child.stdout.take().expect("a piped stdout");
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .expect("dbus-daemon prints its address");
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        PrivateBus { child, address }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .expect("connect to the private bus")
    }

    /// The client that `gpaperctl` uses. Properties are read from the
    /// daemon on each call, never from a cache.
    pub async fn client(&self) -> Gpaperd1Proxy<'static> {
        Gpaperd1Proxy::builder(&self.connect().await)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("build the proxy")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
