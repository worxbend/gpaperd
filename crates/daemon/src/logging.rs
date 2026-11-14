//! Logging setup: the journal when systemd connected stderr to it, else
//! formatted lines on stderr.
//!
//! Events carry structured fields: `style`, `item`, `method` (hardlink,
//! reflink, copy), `reason` and `path`. In the journal they are the fields
//! `STYLE=`, `ITEM=` and so on, so `journalctl --user -u gpaperd STYLE=dark`
//! filters on them.

use std::env;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Installs the global logger. Call it once, before the first log line.
///
/// # Panics
///
/// When a global logger is already installed.
pub fn init() {
    let level = LevelFilter::INFO;
    match journal_layer() {
        Some(journal) => tracing_subscriber::registry()
            .with(level)
            .with(journal)
            .init(),
        None => tracing_subscriber::registry()
            .with(level)
            .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
            .init(),
    }
}

/// A layer that writes to the journal, when stderr goes there anyway.
fn journal_layer() -> Option<tracing_journald::Layer> {
    if !stderr_is_journal_stream() {
        return None;
    }
    // Without a prefix the fields are `STYLE=`, `ITEM=`, …, not `F_STYLE=`.
    // No journal socket after all: stderr still reaches the journal.
    tracing_journald::layer()
        .map(|journal| journal.with_field_prefix(None))
        .ok()
}

/// Whether stderr is the stream that systemd connected to the journal.
///
/// `JOURNAL_STREAM` alone does not say so: every child process inherits it,
/// so a shell in a terminal that runs as a systemd service has it too. The
/// variable names the stream's device and inode, to compare with stderr
/// (systemd.exec(5)).
fn stderr_is_journal_stream() -> bool {
    let journal_stream = env::var("JOURNAL_STREAM")
        .ok()
        .as_deref()
        .and_then(parse_journal_stream);
    journal_stream.is_some() && journal_stream == stderr_stream().ok()
}

/// The device and inode numbers of the file that stderr is.
fn stderr_stream() -> io::Result<(u64, u64)> {
    let stderr = File::from(io::stderr().as_fd().try_clone_to_owned()?);
    let metadata = stderr.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// The device and inode numbers in a `JOURNAL_STREAM` value, `"<dev>:<ino>"`.
fn parse_journal_stream(value: &str) -> Option<(u64, u64)> {
    let (device, inode) = value.split_once(':')?;
    Some((device.parse().ok()?, inode.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_stream_holds_a_device_and_an_inode() {
        assert_eq!(parse_journal_stream("26:10943"), Some((26, 10943)));
    }

    #[test]
    fn journal_stream_without_two_numbers_names_no_stream() {
        for malformed in ["", "26", "26:", ":10943", "26:ten", "26:1:2"] {
            assert_eq!(parse_journal_stream(malformed), None, "{malformed:?}");
        }
    }
}
