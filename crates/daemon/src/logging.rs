//! Logging setup: the journal when systemd connected stderr to it, else
//! formatted lines on stderr.
//!
//! Events carry structured fields: `style`, `item`, `method` (hardlink,
//! reflink, copy), `reason` and `path`. In the journal they are the fields
//! `STYLE=`, `ITEM=` and so on, so `journalctl --user -u gpaperd STYLE=dark`
//! filters on them.

use std::env;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use tracing::field::{Field, Visit};
use tracing::{Event, Level};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
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

/// The datagram socket journald listens on.
///
/// `tracing-journald` sends here and does not export the path. Its
/// constructor uses a blocking socket, so a full buffer never returns and
/// the stderr fallback does not run.
const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// A layer that writes to the journal, when stderr goes there anyway.
///
/// Fields are unprefixed (`STYLE=`, not `F_STYLE=`). A send that would
/// wait is not a journal we can use: stderr still reaches the journal,
/// and a later event uses this same nonblocking socket.
fn journal_layer() -> Option<JournalLayer> {
    if !stderr_is_journal_stream() {
        return None;
    }
    JournalLayer::open(Path::new(JOURNAL_SOCKET))
}

/// Native journal datagrams on a nonblocking socket.
struct JournalLayer {
    socket: UnixDatagram,
    path: PathBuf,
    syslog_identifier: String,
}

impl JournalLayer {
    /// `None` when the socket is missing or a send would wait.
    ///
    /// The probe payload is empty. journald discards that, which is also
    /// how a listener is distinguished from a path nobody has bound.
    fn open(path: &Path) -> Option<Self> {
        let socket = UnixDatagram::unbound().ok()?;
        socket.set_nonblocking(true).ok()?;
        socket.send_to(&[], path).ok()?;
        Some(Self {
            socket,
            path: path.to_path_buf(),
            syslog_identifier: syslog_identifier(),
        })
    }

    fn send_to_journal(&self, payload: &[u8]) -> io::Result<usize> {
        self.socket.send_to(payload, &self.path)
    }

    fn write_event(&self, buf: &mut Vec<u8>, event: &Event<'_>) {
        let meta = event.metadata();
        put_field(buf, "PRIORITY", &[priority(meta.level())]);
        put_field(buf, "TARGET", meta.target().as_bytes());
        if let Some(file) = meta.file() {
            put_field(buf, "CODE_FILE", file.as_bytes());
        }
        if let Some(line) = meta.line() {
            put_field(buf, "CODE_LINE", line.to_string().as_bytes());
        }
        put_field(buf, "SYSLOG_IDENTIFIER", self.syslog_identifier.as_bytes());
        event.record(&mut FieldVisitor { buf });
    }
}

impl<S> Layer<S> for JournalLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut buf = Vec::with_capacity(256);
        self.write_event(&mut buf, event);
        // `WouldBlock` and `EMSGSIZE` drop this event. Waiting here would
        // stall the caller, which is the failure this socket is meant to avoid.
        let _ = self.send_to_journal(&buf);
    }
}

/// File name of this process, as journald's `SYSLOG_IDENTIFIER`.
fn syslog_identifier() -> String {
    env::args_os()
        .next()
        .as_ref()
        .and_then(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// journald priority byte for a tracing level.
///
/// `INFO` is notice (`5`), the mapping these events already use. `TRACE`
/// and any later level are debug (`7`).
fn priority(level: &Level) -> u8 {
    if *level == Level::ERROR {
        b'3'
    } else if *level == Level::WARN {
        b'4'
    } else if *level == Level::INFO {
        b'5'
    } else if *level == Level::DEBUG {
        b'6'
    } else {
        b'7'
    }
}

/// One journal native field: sanitized name, then a little-endian length.
fn put_field(buf: &mut Vec<u8>, name: &str, value: &[u8]) {
    sanitize_field_name(name, buf);
    buf.push(b'\n');
    buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
    buf.extend_from_slice(value);
    buf.push(b'\n');
}

/// journald drops a field whose name is not an uppercase identifier.
fn sanitize_field_name(name: &str, buf: &mut Vec<u8>) {
    buf.extend(
        name.bytes()
            .map(|byte| if byte == b'.' { b'_' } else { byte })
            .skip_while(|byte| *byte == b'_')
            .filter(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
            .map(|byte| byte.to_ascii_uppercase()),
    );
}

struct FieldVisitor<'a> {
    buf: &'a mut Vec<u8>,
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        put_field(self.buf, field.name(), value.as_bytes());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        put_field(self.buf, field.name(), rendered.as_bytes());
    }
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
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::{Path, PathBuf};

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

    #[test]
    fn a_full_journal_socket_is_not_opened() {
        let (_dir, _peer, path) = bound_socket();
        let _senders = fill(&path);
        assert!(
            JournalLayer::open(&path).is_none(),
            "a full journal socket must not be opened; stderr is the fallback"
        );
    }

    #[test]
    fn a_later_datagram_does_not_wait_on_a_full_socket() {
        let (_dir, _peer, path) = bound_socket();
        let journal = JournalLayer::open(&path).expect("an open socket accepts one datagram");
        let _senders = fill(&path);
        let error = journal
            .send_to_journal(&[0])
            .expect_err("a full socket must refuse another datagram");
        assert_eq!(
            error.kind(),
            io::ErrorKind::WouldBlock,
            "a later send on a full journal socket must not wait"
        );
    }

    #[test]
    fn a_journal_field_value_is_length_encoded() {
        let mut buf = Vec::new();
        put_field(&mut buf, "STYLE", b"dark");
        let mut expected = Vec::from(&b"STYLE\n"[..]);
        expected.extend_from_slice(&4u64.to_le_bytes());
        expected.extend_from_slice(b"dark\n");
        assert_eq!(buf, expected);
    }

    #[test]
    fn a_journal_field_name_is_journald_safe() {
        let mut buf = Vec::new();
        put_field(&mut buf, "style", b"dark");
        assert!(buf.starts_with(b"STYLE\n"), "{buf:?}");
        buf.clear();
        put_field(&mut buf, "code.file", b"x");
        assert!(buf.starts_with(b"CODE_FILE\n"), "{buf:?}");
        buf.clear();
        put_field(&mut buf, "_hidden", b"x");
        assert!(buf.starts_with(b"HIDDEN\n"), "{buf:?}");
    }

    #[test]
    fn journal_priority_matches_the_tracing_level() {
        assert_eq!(priority(&Level::ERROR), b'3');
        assert_eq!(priority(&Level::WARN), b'4');
        assert_eq!(priority(&Level::INFO), b'5');
        assert_eq!(priority(&Level::DEBUG), b'6');
        assert_eq!(priority(&Level::TRACE), b'7');
    }

    /// The peer stays open so the path still names a socket.
    fn bound_socket() -> (tempfile::TempDir, UnixDatagram, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("socket");
        let peer = UnixDatagram::bind(&path).expect("a bound datagram");
        (dir, peer, path)
    }

    /// Fills the receiver queue, not only one sender's send buffer.
    ///
    /// One sender stops at its own send buffer, which can hold fewer
    /// datagrams than the receiver queue (`max_dgram_qlen`). So each round
    /// uses a new sender, and a new sender whose first datagram would wait
    /// proves the queue is full. Keep the senders alive through the asserts.
    fn fill(path: &Path) -> Vec<UnixDatagram> {
        let mut senders = Vec::new();
        loop {
            let sender = UnixDatagram::unbound().expect("a sender");
            sender
                .set_nonblocking(true)
                .expect("filling must not block");
            if !send_until_blocked(&sender, path) {
                return senders;
            }
            senders.push(sender);
        }
    }

    /// Whether any datagram went out before a send would wait.
    fn send_until_blocked(sender: &UnixDatagram, path: &Path) -> bool {
        let mut sent_any = false;
        loop {
            match sender.send_to(&[0], path) {
                Ok(_) => sent_any = true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return sent_any,
                Err(error) => panic!("filling the socket failed: {error}"),
            }
        }
    }
}
