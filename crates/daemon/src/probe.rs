//! Just-in-time validation of an image file, run right before it is shown.
//!
//! [`probe`] checks the file and its magic bytes. [`Prober`] adds the glycin
//! decode, which is what tells whether gnome-shell can show the image.

use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, Read};
use std::panic;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime};

use glycin::{Limits, Loader, MainContextSelector, SandboxSelector};
use gpaperd_core::catalog::{BadReason, FileStamp};
use infer::image;
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, oneshot};

use crate::scan;

/// How much of a file the magic-byte check reads. `infer` reads the same
/// amount when it is given a path.
pub const HEADER_LEN: usize = 8192;

/// How long glycin may take over each of its two steps, the header and the
/// first frame. A loader that hangs must not hold up a rotation. A 4K image
/// decodes in well under a second. The same limit bounds the stat and the
/// header read the rotation waits on.
const DECODE_TIMEOUT: Duration = Duration::from_secs(30);

/// A file that was modified less than this ago may still be written to.
/// Shorter than the core's `PROBE_RETRY_DELAY`, so a file that is only
/// corrupt gets its final answer at the first retry.
const SETTLE_TIME: Duration = Duration::from_secs(5);

/// How many answers of glycin are remembered at most.
const MAX_REMEMBERED_FILES: usize = 8192;

/// A grey PNG of one pixel. glycin decodes it wherever glycin works at all.
const KNOWN_GOOD_PNG: &[u8] =
    b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\0\0\0\0\x3a\x7e\x9b\x55\
    \0\0\0\nIDATx\xdac`\0\0\0\x02\0\x01\xe5\x27\xde\xfc\0\0\0\0IEND\xaeB`\x82";

/// An image format that the daemon accepts: one the scan lists.
#[derive(Debug)]
pub struct ImageFormat {
    matches: fn(&[u8]) -> bool,
    /// The file name extension a remote blob of this format gets.
    pub extension: &'static str,
    pub mime: &'static str,
}

/// One entry per scanned format. `infer` knows more image formats (PSD,
/// ICO, raw photos); they are left out because the scan does not list them.
/// AVIF before HEIF: an AVIF file also passes the HEIF check.
const IMAGE_FORMATS: [ImageFormat; 10] = [
    format(image::is_jpeg, "jpg", "image/jpeg"),
    format(image::is_png, "png", "image/png"),
    format(image::is_webp, "webp", "image/webp"),
    format(image::is_jxl, "jxl", "image/jxl"),
    format(image::is_avif, "avif", "image/avif"),
    format(image::is_heif, "heic", "image/heif"),
    format(image::is_tiff, "tif", "image/tiff"),
    format(image::is_bmp, "bmp", "image/bmp"),
    format(image::is_gif, "gif", "image/gif"),
    format(is_qoi, "qoi", "image/qoi"),
];

const fn format(
    matches: fn(&[u8]) -> bool,
    extension: &'static str,
    mime: &'static str,
) -> ImageFormat {
    ImageFormat {
        matches,
        extension,
        mime,
    }
}

/// The accepted format whose magic bytes `header` starts with: the first
/// 8 KiB of a file are enough. `None` for anything else.
pub fn image_format(header: &[u8]) -> Option<&'static ImageFormat> {
    IMAGE_FORMATS.iter().find(|format| (format.matches)(header))
}

/// Checks that `path` can be shown. Blocking I/O; reads only the start of
/// the file.
///
/// Checks, in this order:
/// 1. The file exists, else [`BadReason::Missing`].
/// 2. It is a regular file after following symlinks, else
///    [`BadReason::NotRegularFile`].
/// 3. It can be opened and read, else [`BadReason::Unreadable`] with the OS
///    error text.
/// 4. Its magic bytes are one of the scanned formats (JPEG, PNG, WebP, JPEG
///    XL, AVIF, HEIF, TIFF, BMP, GIF, QOI), whatever the extension says, else
///    [`BadReason::NotAnImage`].
///
/// A dangling symlink counts as missing, and so does a path whose parent is
/// no longer a directory.
pub fn probe(path: &Path) -> Result<(), BadReason> {
    let metadata = fs::metadata(path).map_err(bad_reason)?;
    if !metadata.is_file() {
        return Err(BadReason::NotRegularFile);
    }
    let header = read_header(path).map_err(bad_reason)?;
    image_format(&header)
        .map(|_| ())
        .ok_or(BadReason::NotAnImage)
}

/// The whole just-in-time check: [`probe`], then a decode by glycin, the
/// sandboxed loader stack that gnome-shell 51 uses itself.
///
/// It remembers what glycin said about a file, by the file's
/// `(dev, ino, size, mtime in ns)`. A file that still has the stamp of an
/// earlier check is not decoded again. One `Prober` serves both styles for
/// the whole run: every method takes `&self`, so tasks can share it.
///
/// Use a `Prober` in one tokio runtime only: the one of its first check. Its
/// loader processes and its GLib thread stay with that runtime.
#[derive(Debug)]
pub struct Prober {
    /// What glycin said about each file, by the stamp the file had then.
    decoded: Mutex<HashMap<FileStamp, Decoded>>,
    /// Holds only a sandbox that passed `trial`. While it is empty, each
    /// check starts and tries a new one.
    sandbox: OnceCell<Sandbox>,
    /// Tells whether a new sandbox works: [`Sandbox::decodes`]. A field only
    /// so that a unit test can stand in for glycin and bwrap.
    trial: Trial,
}

/// How a [`Prober`] tries a new sandbox before it keeps it.
type Trial = for<'a> fn(&'a Sandbox) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

impl Default for Prober {
    fn default() -> Prober {
        Prober {
            decoded: Mutex::default(),
            sandbox: OnceCell::new(),
            trial: Sandbox::decodes,
        }
    }
}

/// glycin as one [`Prober`] uses it: loader processes and a GLib thread of
/// its own.
///
/// glycin's defaults are one pool of processes and one thread for the whole
/// process. Both stay with the tokio runtime that used glycin first, and in
/// every later runtime each decode fails. A test binary has one runtime per
/// test.
///
/// Dropping it ends the thread.
#[derive(Debug)]
struct Sandbox {
    loaders: Arc<glycin::Pool>,
    context: glib::MainContext,
    /// The thread serves `context` until this is dropped.
    _thread_stopper: oneshot::Sender<()>,
}

impl Sandbox {
    /// Starts the GLib thread. Must run inside the tokio runtime that the
    /// checks run in.
    fn start() -> Sandbox {
        let context = glib::MainContext::new();
        let runtime = Handle::current();
        let (thread_stopper, stopped) = oneshot::channel::<()>();
        let served = context.clone();
        thread::spawn(move || {
            // glycin starts its tasks from this thread.
            let _in_runtime = runtime.enter();
            // Ends with an error when the sender is dropped: the stop signal.
            let _ = served.block_on(stopped);
        });
        Sandbox {
            loaders: glycin::Pool::new(glycin::PoolConfig::new()),
            context,
            _thread_stopper: thread_stopper,
        }
    }

    /// Sets `loader` up to decode in the bwrap sandbox or not at all, and to
    /// give up after [`DECODE_TIMEOUT`] per step.
    fn loader(&self, mut loader: Loader) -> Loader {
        loader
            .pool(Arc::clone(&self.loaders))
            .main_context_selector(MainContextSelector::Specific(self.context.clone()))
            // Not the default `Auto`: where bwrap is refused, that decodes
            // without a sandbox.
            .sandbox_selector(SandboxSelector::Bwrap)
            .limits(Limits::default().timeout(DECODE_TIMEOUT));
        loader
    }

    /// Whether an image that is known to be good loads. Warns when not.
    /// Boxed, so that it fits [`Trial`].
    fn decodes(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let known_good = Loader::new_vec(KNOWN_GOOD_PNG.to_vec());
            match self.loader(known_good).load().await {
                Ok(_image) => true,
                Err(error) => {
                    tracing::warn!(
                        reason = %first_line(&error),
                        "glycin cannot decode in its sandbox: no image passes until a later check can"
                    );
                    false
                }
            }
        })
    }
}

/// What glycin said about a file's content: its size when the header and the
/// first frame decode, else the first line of glycin's error text.
type Decoded = Result<Dimensions, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dimensions {
    width: u32,
    height: u32,
}

impl Dimensions {
    fn of(image: &glycin::Image) -> Dimensions {
        let details = image.details();
        Dimensions {
            width: details.width(),
            height: details.height(),
        }
    }

    /// Step 2 of [`Prober::check`].
    fn within(self, max_pixels: u64) -> Result<(), BadReason> {
        if u64::from(self.width) * u64::from(self.height) <= max_pixels {
            Ok(())
        } else {
            Err(BadReason::TooLarge {
                width: self.width,
                height: self.height,
            })
        }
    }
}

impl Prober {
    pub fn new() -> Prober {
        Prober::default()
    }

    /// Checks that the shell can show `path`. Blocking I/O runs off the
    /// caller's thread. The runtime must have its I/O driver and its timers
    /// enabled (`enable_io`, `enable_time`). Without them glycin's tasks
    /// panic on glycin's thread, and that ends like a sandbox that cannot
    /// start. A stat or header read that outlasts the decode limit comes
    /// back as [`BadReason::Truncated`].
    ///
    /// Checks, in this order:
    /// 1. Everything [`probe`] checks, with the same reasons.
    /// 2. glycin reads the image header: `width * height` must be at most
    ///    `max_pixels`, else [`BadReason::TooLarge`].
    /// 3. glycin decodes the first frame, else [`BadReason::Undecodable`]
    ///    with the first line of glycin's error text: corrupt content or no
    ///    loader for the format. A loader that took too long is
    ///    [`BadReason::Truncated`], and that is not remembered. The header
    ///    alone does not show a cut-off file: glycin 4.0.5 loads a PNG, a
    ///    WebP and a JPEG XL that are cut short, and fails only on the frame.
    ///
    /// A failed decode of a file that is still being written is
    /// [`BadReason::Truncated`] instead: its stamp changed while the check
    /// ran, it was modified moments ago, or its mtime is ahead of the clock.
    /// That verdict is not remembered.
    ///
    /// The remembered answer is what glycin said, not the verdict, so a
    /// later call with another `max_pixels` needs no second decode. An image
    /// that is too large is never decoded, so nothing is remembered of it.
    ///
    /// The sandbox is tried with an image that is known to be good. When it
    /// cannot start (no `bwrap`, a container that blocks it, or a decode
    /// that timed out), nothing can be decoded. That is logged as a warning,
    /// and the check is [`BadReason::Truncated`]: it says nothing about the
    /// file, the core probes it again, and a later check tries the sandbox
    /// again. Magic bytes are not a pass.
    #[expect(
        clippy::manual_async_fn,
        reason = "an `async fn` cannot promise the `Send` future that the interpreter spawns"
    )]
    pub fn check(
        &self,
        path: &Path,
        max_pixels: u64,
    ) -> impl Future<Output = Result<(), BadReason>> + Send {
        async move {
            let stamp = off_thread(path, examine).await?;
            let Some(sandbox) = self.sandbox().await else {
                return Err(BadReason::Truncated);
            };
            match self.remembered(&stamp) {
                Some(decoded) => decoded.map_err(BadReason::Undecodable)?.within(max_pixels),
                None => self.decode(sandbox, path, stamp, max_pixels).await,
            }
        }
    }

    /// glycin's sandbox, kept from the first call where it decodes. `None`
    /// when it cannot decode yet. A failed sandbox is dropped, which ends its
    /// thread, so one bad start does not disable checks for the whole run.
    async fn sandbox(&self) -> Option<&Sandbox> {
        let start = || async {
            let sandbox = Sandbox::start();
            (self.trial)(&sandbox).await.then_some(sandbox).ok_or(())
        };
        self.sandbox.get_or_try_init(start).await.ok()
    }

    /// Steps 2 and 3 of [`Prober::check`], for content that glycin has not
    /// seen yet.
    async fn decode(
        &self,
        sandbox: &Sandbox,
        path: &Path,
        stamp: FileStamp,
        max_pixels: u64,
    ) -> Result<(), BadReason> {
        let file = Loader::new(gio::File::for_path(path));
        let mut image = match sandbox.loader(file).load().await {
            Ok(image) => image,
            Err(error) => return Err(self.refused(path, stamp, error).await),
        };
        let dimensions = Dimensions::of(&image);
        // Before the frame, so that an image of any size is refused without
        // decoding it.
        dimensions.within(max_pixels)?;
        if let Err(error) = image.next_frame().await {
            return Err(self.refused(path, stamp, error).await);
        }
        self.remember(stamp, Ok(dimensions));
        Ok(())
    }

    /// Why a file that glycin could not decode is refused. Remembers glycin's
    /// answer when it is final: the file is not changing, and glycin did not
    /// just run out of time.
    async fn refused(&self, path: &Path, before: FileStamp, error: glycin::Error) -> BadReason {
        let after = match off_thread(path, current_stamp).await {
            Ok(after) => after,
            Err(gone) => return gone,
        };
        // A timeout says nothing about the content. `Truncated` is retried.
        // `Undecodable` would stick until the stamp changes.
        if is_unfinished(&before, &after, SystemTime::now()) || error.is_timeout() {
            return BadReason::Truncated;
        }
        let text = first_line(&error);
        self.remember(before, Err(text.clone()));
        BadReason::Undecodable(text)
    }

    fn remembered(&self, stamp: &FileStamp) -> Option<Decoded> {
        self.cache().get(stamp).cloned()
    }

    fn remember(&self, stamp: FileStamp, decoded: Decoded) {
        let mut cache = self.cache();
        // shortcut: a full cache is emptied, upgrade to dropping only the
        // oldest answers if decodes after that show up as a cost
        if cache.len() >= MAX_REMEMBERED_FILES {
            cache.clear();
        }
        cache.insert(stamp, decoded);
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<FileStamp, Decoded>> {
        // No panic can leave the map half-written: a poisoned lock is good.
        self.decoded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The first line of glycin's error text. The lines after it hold the
/// loader's command line or the whole loader configuration.
fn first_line(error: &glycin::Error) -> String {
    let text = error.to_string();
    text.lines().next().unwrap_or_default().to_owned()
}

/// Whether a file whose decode failed may still be written to: its stamp
/// changed while the check ran, or it was modified less than [`SETTLE_TIME`]
/// before `now`. Or after `now`: the clock of a file server may run ahead.
fn is_unfinished(before: &FileStamp, after: &FileStamp, now: SystemTime) -> bool {
    if after != before {
        return true;
    }
    match now.duration_since(after.mtime) {
        Ok(age) => age < SETTLE_TIME,
        // The error's duration is how far ahead the file clock is, not an age.
        // A skew of `SETTLE_TIME` or more is still a file that may be written.
        Err(_ahead) => true,
    }
}

/// Runs `inspect`, which blocks on the file system, off the caller's thread.
/// Gives up after [`DECODE_TIMEOUT`], the same limit as a glycin step.
async fn off_thread(
    path: &Path,
    inspect: fn(&Path) -> Result<FileStamp, BadReason>,
) -> Result<FileStamp, BadReason> {
    inspect_within(path, inspect, DECODE_TIMEOUT).await
}

/// `limit` is [`DECODE_TIMEOUT`] on the check path. A stuck read is
/// [`BadReason::Truncated`]: the core probes that again, and the stat is
/// not [`BadReason::Undecodable`].
///
/// Nothing cancels the read: a thread stuck on a dead mount stays in the
/// syscall. The rotation must not wait with it. The read runs on a thread
/// of its own, not in tokio's blocking pool, so stuck reads cannot fill
/// that pool and starve the state writes that also use it.
async fn inspect_within(
    path: &Path,
    inspect: fn(&Path) -> Result<FileStamp, BadReason>,
    limit: Duration,
) -> Result<FileStamp, BadReason> {
    let path = path.to_owned();
    let (send, read) = oneshot::channel();
    let started = thread::Builder::new()
        .name("gpaperd-stat".to_owned())
        .spawn(move || {
            // The receiver is gone when the check gave up: nobody waits.
            let _ = send.send(panic::catch_unwind(move || inspect(&path)));
        });
    if let Err(error) = started {
        // Not the file's fault: `Truncated` is probed again.
        tracing::warn!(reason = %error, "cannot start a thread to read a file");
        return Err(BadReason::Truncated);
    }
    let finished = match tokio::time::timeout(limit, read).await {
        Ok(finished) => finished.expect("the read thread catches panics and always sends"),
        Err(_elapsed) => return Err(BadReason::Truncated),
    };
    // A panic is a defect: pass it on.
    finished.unwrap_or_else(|failure| panic::resume_unwind(failure))
}

/// Step 1 of [`Prober::check`]. Returns the stamp that the file had before
/// its content was read. Blocking I/O.
fn examine(path: &Path) -> Result<FileStamp, BadReason> {
    let before = current_stamp(path)?;
    probe(path)?;
    Ok(before)
}

/// Blocking I/O.
fn current_stamp(path: &Path) -> Result<FileStamp, BadReason> {
    let metadata = fs::metadata(path).map_err(bad_reason)?;
    scan::file_stamp(&metadata).map_err(bad_reason)
}

fn bad_reason(err: io::Error) -> BadReason {
    match err.kind() {
        // NotADirectory: a parent of the path was replaced by a file.
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => BadReason::Missing,
        _ => BadReason::Unreadable(err.to_string()),
    }
}

fn read_header(path: &Path) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    File::open(path)?
        .take(HEADER_LEN as u64)
        .read_to_end(&mut header)?;
    Ok(header)
}

/// `infer` has no QOI matcher.
fn is_qoi(header: &[u8]) -> bool {
    header.starts_with(b"qoif")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use gpaperd_core::catalog::FileId;
    use tempfile::TempDir;

    use super::*;

    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn file_with(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, content).expect("write fixture");
        path
    }

    #[test]
    fn png_header_passes() {
        let dir = TempDir::new().expect("temp dir");
        let png = file_with(&dir, "wall.png", PNG_HEADER);

        assert_eq!(probe(&png), Ok(()));
    }

    #[test]
    fn every_scanned_format_passes_whatever_the_file_name() {
        let headers: [(&str, &[u8]); 11] = [
            ("jpeg", b"\xff\xd8\xff\xe0\0\x10JFIF\0"),
            ("png", PNG_HEADER),
            ("webp", b"RIFF\x24\0\0\0WEBPVP8 "),
            ("jxl codestream", b"\xff\x0a\0\0"),
            (
                "jxl container",
                b"\0\0\0\x0cJXL \r\n\x87\n\0\0\0\x14ftypjxl ",
            ),
            ("avif", b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf"),
            ("heic", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"),
            ("tiff", b"II*\0\x08\0\0\0\x0e\0\0\x01"),
            ("bmp", b"BM\x36\0\x0c\0\0\0\0\0"),
            ("gif", b"GIF89a\x01\0\x01\0"),
            ("qoi", b"qoif\0\0\0\x01\0\0\0\x01\x04\0"),
        ];
        let dir = TempDir::new().expect("temp dir");

        for (format, header) in headers {
            let file = file_with(&dir, "wallpaper.dat", header);
            assert_eq!(probe(&file), Ok(()), "{format}");
        }
    }

    #[test]
    fn text_file_renamed_png_is_not_an_image() {
        let dir = TempDir::new().expect("temp dir");
        let fake = file_with(&dir, "notes.png", b"shopping list: milk, eggs\n");

        assert_eq!(probe(&fake), Err(BadReason::NotAnImage));
    }

    #[test]
    fn empty_file_is_not_an_image() {
        let dir = TempDir::new().expect("temp dir");
        let empty = file_with(&dir, "empty.png", b"");

        assert_eq!(probe(&empty), Err(BadReason::NotAnImage));
    }

    #[test]
    fn image_format_the_scan_does_not_list_is_rejected() {
        let dir = TempDir::new().expect("temp dir");
        let photoshop = file_with(&dir, "layers.png", b"8BPS\0\x01\0\0\0\0\0\0\0\x03");

        assert_eq!(probe(&photoshop), Err(BadReason::NotAnImage));
    }

    #[test]
    fn deleted_file_is_missing() {
        let dir = TempDir::new().expect("temp dir");
        let png = file_with(&dir, "wall.png", PNG_HEADER);
        fs::remove_file(&png).expect("delete fixture");

        assert_eq!(probe(&png), Err(BadReason::Missing));
    }

    #[test]
    fn file_below_a_parent_that_became_a_file_is_missing() {
        let dir = TempDir::new().expect("temp dir");
        let former_dir = file_with(&dir, "album", b"now a file");

        assert_eq!(probe(&former_dir.join("wall.png")), Err(BadReason::Missing));
    }

    #[test]
    fn directory_is_not_a_regular_file() {
        let dir = TempDir::new().expect("temp dir");

        assert_eq!(probe(dir.path()), Err(BadReason::NotRegularFile));
    }

    #[test]
    fn symlink_is_followed_to_its_target() {
        let dir = TempDir::new().expect("temp dir");
        let png = file_with(&dir, "wall.png", PNG_HEADER);
        let link = dir.path().join("link");
        symlink(&png, &link).expect("create symlink");

        assert_eq!(probe(&link), Ok(()));
    }

    #[test]
    fn dangling_symlink_is_missing() {
        let dir = TempDir::new().expect("temp dir");
        let link = dir.path().join("link.png");
        symlink(dir.path().join("gone.png"), &link).expect("create symlink");

        assert_eq!(probe(&link), Err(BadReason::Missing));
    }

    #[test]
    fn file_without_read_permission_is_unreadable() {
        let dir = TempDir::new().expect("temp dir");
        let locked = file_with(&dir, "locked.png", PNG_HEADER);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod fixture");
        if File::open(&locked).is_ok() {
            // Root ignores file modes, so the case cannot be staged.
            return;
        }

        let verdict = probe(&locked);

        assert!(
            matches!(&verdict, Err(BadReason::Unreadable(reason)) if reason.contains("ermission")),
            "{verdict:?}"
        );
    }

    const AN_HOUR: Duration = Duration::from_secs(60 * 60);

    fn stamp_modified_at(mtime: SystemTime) -> FileStamp {
        FileStamp {
            id: FileId { dev: 1, ino: 2 },
            size: 3,
            mtime,
        }
    }

    /// Sets the file's mtime to an hour ago: nobody writes to it any more.
    fn aged(path: PathBuf) -> PathBuf {
        let file = File::options()
            .write(true)
            .open(&path)
            .expect("open fixture");
        file.set_modified(SystemTime::now() - AN_HOUR)
            .expect("set mtime");
        path
    }

    #[test]
    fn image_of_exactly_max_pixels_is_within() {
        let dimensions = Dimensions {
            width: 16,
            height: 12,
        };

        assert_eq!(dimensions.within(16 * 12), Ok(()));
    }

    #[test]
    fn image_of_one_pixel_more_than_max_pixels_is_too_large() {
        let dimensions = Dimensions {
            width: 16,
            height: 12,
        };

        assert_eq!(
            dimensions.within(16 * 12 - 1),
            Err(BadReason::TooLarge {
                width: 16,
                height: 12
            })
        );
    }

    #[test]
    fn largest_dimensions_do_not_overflow_the_pixel_count() {
        let dimensions = Dimensions {
            width: u32::MAX,
            height: u32::MAX,
        };

        assert_eq!(dimensions.within(u64::MAX), Ok(()));
    }

    #[test]
    fn file_that_rests_for_an_hour_is_finished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now - AN_HOUR);

        assert!(!is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_that_rests_for_the_settle_time_is_finished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now - SETTLE_TIME);

        assert!(!is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_modified_a_second_ago_is_unfinished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now - Duration::from_secs(1));

        assert!(is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_modified_a_second_ahead_of_the_clock_is_unfinished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now + Duration::from_secs(1));

        assert!(is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_modified_the_settle_time_ahead_of_the_clock_is_unfinished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now + SETTLE_TIME);

        assert!(is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_modified_an_hour_ahead_of_the_clock_is_unfinished() {
        let now = SystemTime::now();
        let stamp = stamp_modified_at(now + AN_HOUR);

        assert!(is_unfinished(&stamp, &stamp, now));
    }

    #[test]
    fn file_whose_stamp_changed_during_the_check_is_unfinished() {
        let now = SystemTime::now();
        let before = stamp_modified_at(now - AN_HOUR);
        let after = FileStamp { size: 4, ..before };

        assert!(is_unfinished(&before, &after, now));
    }

    #[tokio::test]
    async fn failed_decode_of_a_resting_file_is_undecodable_and_remembered() {
        let dir = TempDir::new().expect("temp dir");
        let png = aged(file_with(&dir, "wall.png", PNG_HEADER));
        let stamp = current_stamp(&png).expect("stat fixture");
        let prober = Prober::new();

        let reason = prober
            .refused(&png, stamp, glycin::Error::other("no pixels"))
            .await;

        let text = "Other: no pixels".to_owned();
        assert_eq!(reason, BadReason::Undecodable(text.clone()));
        assert_eq!(prober.remembered(&stamp), Some(Err(text)));
    }

    #[tokio::test]
    async fn failed_decode_of_a_file_just_written_is_truncated_and_not_remembered() {
        let dir = TempDir::new().expect("temp dir");
        let png = file_with(&dir, "wall.png", PNG_HEADER);
        let stamp = current_stamp(&png).expect("stat fixture");
        let prober = Prober::new();

        let reason = prober
            .refused(&png, stamp, glycin::Error::other("no pixels"))
            .await;

        assert_eq!(reason, BadReason::Truncated);
        assert_eq!(prober.remembered(&stamp), None);
    }

    #[tokio::test]
    async fn failed_decode_of_a_file_that_grew_meanwhile_is_truncated() {
        let dir = TempDir::new().expect("temp dir");
        let png = aged(file_with(&dir, "wall.png", PNG_HEADER));
        let before = current_stamp(&png).expect("stat fixture");
        aged(file_with(&dir, "wall.png", b"\x89PNG\r\n\x1a\n and more"));
        let prober = Prober::new();

        let reason = prober
            .refused(&png, before, glycin::Error::other("no pixels"))
            .await;

        assert_eq!(reason, BadReason::Truncated);
    }

    #[tokio::test]
    async fn failed_decode_of_a_file_deleted_meanwhile_is_missing() {
        let dir = TempDir::new().expect("temp dir");
        let png = aged(file_with(&dir, "wall.png", PNG_HEADER));
        let stamp = current_stamp(&png).expect("stat fixture");
        fs::remove_file(&png).expect("delete fixture");
        let prober = Prober::new();

        let reason = prober
            .refused(&png, stamp, glycin::Error::other("no pixels"))
            .await;

        assert_eq!(reason, BadReason::Missing);
    }

    #[tokio::test]
    async fn failed_decode_of_a_file_an_hour_ahead_is_truncated_and_not_remembered() {
        let dir = TempDir::new().expect("temp dir");
        let png = file_with(&dir, "wall.png", PNG_HEADER);
        let file = File::options()
            .write(true)
            .open(&png)
            .expect("open fixture");
        file.set_modified(SystemTime::now() + AN_HOUR)
            .expect("set mtime");
        let stamp = current_stamp(&png).expect("stat fixture");
        assert!(
            stamp.mtime > SystemTime::now(),
            "mtime stayed in the future"
        );
        let prober = Prober::new();

        let reason = prober
            .refused(&png, stamp, glycin::Error::other("no pixels"))
            .await;

        assert_eq!(reason, BadReason::Truncated);
        assert_eq!(prober.remembered(&stamp), None);
    }

    #[tokio::test]
    async fn timed_out_decode_is_truncated_and_not_remembered() {
        let dir = TempDir::new().expect("temp dir");
        let png = aged(file_with(&dir, "wall.png", PNG_HEADER));
        let stamp = current_stamp(&png).expect("stat fixture");
        let error = glycin_timeout().await;
        let prober = Prober::new();

        let reason = prober.refused(&png, stamp, error).await;

        assert_eq!(reason, BadReason::Truncated);
        assert_eq!(prober.remembered(&stamp), None);
    }

    /// glycin's timeout kind is not constructable outside that crate. A limit
    /// of zero is how this test reaches [`glycin::Error::is_timeout`]. It is
    /// not a daemon deadline: the decode limit stays [`DECODE_TIMEOUT`].
    async fn glycin_timeout() -> glycin::Error {
        let sandbox = Sandbox::start();
        let mut loader = sandbox.loader(Loader::new_vec(KNOWN_GOOD_PNG.to_vec()));
        loader.limits(Limits::default().timeout(Duration::ZERO));
        match tokio::time::timeout(DECODE_TIMEOUT, loader.load()).await {
            Ok(Err(error)) if error.is_timeout() => error,
            Ok(other) => panic!("zero glycin limit did not time out: {other:?}"),
            Err(_elapsed) => panic!("zero glycin limit waited out the decode timeout"),
        }
    }

    /// How many times [`passes_from_the_second_trial`] ran. Only
    /// `failed_sandbox_is_tried_again_and_a_working_one_is_kept` uses it.
    static TRIALS: AtomicUsize = AtomicUsize::new(0);

    /// Fails the first trial and passes every later one, as a `bwrap` that
    /// was fixed meanwhile.
    fn passes_from_the_second_trial(
        _sandbox: &Sandbox,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let earlier_trials = TRIALS.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { earlier_trials > 0 })
    }

    /// The retry of `tests/probe.rs`, without bwrap: the trial stands in for
    /// glycin, and glycin's answer for the file is already remembered, so
    /// nothing is decoded.
    #[tokio::test]
    async fn failed_sandbox_is_tried_again_and_a_working_one_is_kept() {
        let dir = TempDir::new().expect("temp dir");
        let png = aged(file_with(&dir, "wall.png", PNG_HEADER));
        let prober = Prober {
            trial: passes_from_the_second_trial,
            ..Prober::new()
        };
        let one_pixel = Dimensions {
            width: 1,
            height: 1,
        };
        prober.remember(current_stamp(&png).expect("stat fixture"), Ok(one_pixel));

        let refused = prober.check(&png, 1).await;
        let passed = prober.check(&png, 1).await;
        let passed_again = prober.check(&png, 1).await;

        assert_eq!(
            (refused, passed, passed_again),
            (Err(BadReason::Truncated), Ok(()), Ok(()))
        );
        assert_eq!(
            TRIALS.load(Ordering::Relaxed),
            2,
            "a kept sandbox is tried again"
        );
    }

    static RELEASE_STUCK_READ: AtomicBool = AtomicBool::new(false);

    /// Held by each test that stages a stuck read. One test's release must
    /// not free the read of another.
    static STUCK_READ_TEST: Mutex<()> = Mutex::new(());

    struct ReleaseStuckRead {
        _one_test_at_a_time: MutexGuard<'static, ()>,
    }

    impl ReleaseStuckRead {
        fn arm() -> ReleaseStuckRead {
            let guard = STUCK_READ_TEST
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            RELEASE_STUCK_READ.store(false, Ordering::Relaxed);
            ReleaseStuckRead {
                _one_test_at_a_time: guard,
            }
        }
    }

    impl Drop for ReleaseStuckRead {
        fn drop(&mut self) {
            RELEASE_STUCK_READ.store(true, Ordering::Relaxed);
        }
    }

    fn block_until_released(_path: &Path) -> Result<FileStamp, BadReason> {
        while !RELEASE_STUCK_READ.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(1));
        }
        Err(BadReason::Missing)
    }

    #[tokio::test]
    async fn stuck_stat_is_truncated_not_undecodable() {
        let _release = ReleaseStuckRead::arm();
        // `off_thread` passes [`DECODE_TIMEOUT`]. Zero is the same race, and
        // it does not wait that limit out. The outer bound is that limit, so
        // a stuck read that ignores it fails instead of hanging the suite.
        let read = inspect_within(Path::new("unused"), block_until_released, Duration::ZERO);
        let verdict = tokio::time::timeout(DECODE_TIMEOUT, read).await;

        assert_eq!(verdict, Ok(Err(BadReason::Truncated)));
    }

    #[test]
    fn stuck_read_leaves_the_blocking_pool_free() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("build a tokio runtime");
        // Declared after the runtime, so it is dropped first: a runtime that
        // waits for a stuck pool thread on its drop is released.
        let _release = ReleaseStuckRead::arm();

        let freed = runtime.block_on(async {
            let read = inspect_within(Path::new("unused"), block_until_released, Duration::ZERO);
            assert_eq!(read.await, Err(BadReason::Truncated));
            let other_work = tokio::task::spawn_blocking(|| ());
            tokio::time::timeout(DECODE_TIMEOUT, other_work).await
        });

        assert!(
            freed.is_ok(),
            "other blocking work waited behind the stuck read"
        );
    }

    #[test]
    fn full_cache_is_emptied_before_the_next_answer() {
        let prober = Prober::new();
        let answer = Ok(Dimensions {
            width: 1,
            height: 1,
        });
        let stamps = (0..=MAX_REMEMBERED_FILES as u64).map(|size| FileStamp {
            size,
            ..stamp_modified_at(SystemTime::UNIX_EPOCH)
        });

        for stamp in stamps {
            prober.remember(stamp, answer.clone());
        }

        assert_eq!(prober.cache().len(), 1);
    }
}
