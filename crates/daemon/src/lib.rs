//! gpaperd's imperative shell: adapters around the pure core, and the
//! interpreter that executes the core's effects.

pub mod cache;
pub mod config_watch;
pub mod dbus;
pub mod favorites;
pub mod fetch;
pub mod gnome;
pub mod interp;
pub mod logging;
pub mod paths;
pub mod probe;
pub mod scan;
pub mod screensaver;
pub mod slots;
pub mod state;
pub mod timer;
pub mod watch;
