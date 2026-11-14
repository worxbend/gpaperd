//! The just-in-time check against real image files and the real glycin
//! loaders.
//!
//! The files in `tests/fixtures` are 16 x 12 pixels, made with ImageMagick,
//! `cjxl`, `heif-enc` and `avifenc`. `truncated.png` is `tiny.png` cut inside
//! its pixel data. `corrupt.png` is `tiny.png` with its pixel data
//! overwritten; it has the same length. `huge.png` has a valid header that
//! claims 30000 x 30000 pixels. `notes.png` is text.
//!
//! Where glycin's sandbox cannot start, a check falls back to the magic
//! bytes. Every test that needs a decode asks [`decodes_here`] and then
//! asserts the fallback verdict instead.
//!
//! glycin only reads below the XDG directories: the loader configuration.
//! The one cache it writes is for the SVG loader, and no fixture is an SVG.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use std::{mem, thread};

use gpaperd_core::catalog::BadReason;
use gpaperd_core::config::DEFAULT_MAX_PIXELS;
use gpaperd_daemon::probe::Prober;
use tempfile::TempDir;

/// Width times height of the `tiny` fixtures.
const TINY_PIXELS: u64 = 16 * 12;

/// Set in a child process that runs without a `bwrap` that works.
const NO_SANDBOX_VAR: &str = "GPAPERD_TEST_NO_SANDBOX";

/// Whether glycin can decode in its sandbox on this machine: asked of glycin
/// itself, not of the code under test.
///
/// Asked once, on a thread and in a runtime of its own. glycin's own thread
/// stays with the first runtime that uses it, so that runtime is never
/// dropped, and no test's runtime may be the first.
static DECODES_HERE: LazyLock<bool> = LazyLock::new(|| {
    let ask_glycin = || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a tokio runtime");
        let mut loader = glycin::Loader::new(gio::File::for_path(fixture("tiny.png")));
        loader.sandbox_selector(glycin::SandboxSelector::Bwrap);
        // glycin's image must be dropped inside the runtime too.
        let decodes = runtime.block_on(async { loader.load().await.is_ok() });
        mem::forget(runtime);
        decodes
    };
    thread::spawn(ask_glycin).join().expect("ask glycin")
});

fn decodes_here() -> bool {
    *DECODES_HERE
}

/// What a check of a file with an image's magic bytes must say: `decoded`
/// where glycin decodes, and `Ok` where only the magic bytes are checked.
fn verdict_here(decoded: Result<(), BadReason>) -> Result<(), BadReason> {
    if decodes_here() { decoded } else { Ok(()) }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A copy of a fixture that was modified just now, as a file is that someone
/// still writes to.
fn fresh_copy(dir: &TempDir, name: &str) -> PathBuf {
    let copy = dir.path().join(name);
    fs::copy(fixture(name), &copy).expect("copy fixture");
    copy
}

/// A copy of a fixture that was modified an hour ago: nobody writes to it.
fn resting_copy(dir: &TempDir, name: &str) -> PathBuf {
    let copy = fresh_copy(dir, name);
    set_mtime(&copy, an_hour_ago());
    copy
}

fn an_hour_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(60 * 60)
}

fn set_mtime(path: &Path, mtime: SystemTime) {
    let file = File::options().write(true).open(path).expect("open file");
    file.set_modified(mtime).expect("set mtime");
}

/// Replaces the content of `path` and keeps its stamp: same inode, same
/// size, same mtime.
fn swap_content_keeping_stamp(path: &Path, content_of: &str) {
    let before = fs::metadata(path).expect("stat file");
    let content = fs::read(fixture(content_of)).expect("read fixture");
    assert_eq!(content.len() as u64, before.len(), "same size");
    fs::write(path, content).expect("overwrite file");
    set_mtime(path, before.modified().expect("mtime"));
}

/// Asserts that glycin refused to decode the file. Where glycin cannot
/// decode at all, asserts that the file passed on its magic bytes.
fn assert_undecodable(verdict: &Result<(), BadReason>) {
    if decodes_here() {
        assert!(
            matches!(verdict, Err(BadReason::Undecodable(text)) if !text.contains('\n')),
            "{verdict:?}"
        );
    } else {
        assert_eq!(verdict, &Ok(()));
    }
}

#[tokio::test]
async fn image_of_every_format_with_a_loader_passes() {
    let prober = Prober::new();

    for name in [
        "tiny.png",
        "tiny.jpg",
        "tiny.webp",
        "tiny.jxl",
        "tiny.heic",
        "tiny.avif",
    ] {
        let verdict = prober.check(&fixture(name), DEFAULT_MAX_PIXELS).await;

        assert_eq!(verdict, Ok(()), "{name}");
    }
}

#[tokio::test]
async fn cut_off_file_that_was_just_written_is_truncated() {
    let dir = TempDir::new().expect("temp dir");
    let copying = fresh_copy(&dir, "truncated.png");

    let verdict = Prober::new().check(&copying, DEFAULT_MAX_PIXELS).await;

    assert_eq!(verdict, verdict_here(Err(BadReason::Truncated)));
}

#[tokio::test]
async fn cut_off_file_that_rests_is_undecodable() {
    let dir = TempDir::new().expect("temp dir");
    let cut_off = resting_copy(&dir, "truncated.png");

    let verdict = Prober::new().check(&cut_off, DEFAULT_MAX_PIXELS).await;

    assert_undecodable(&verdict);
}

#[tokio::test]
async fn corrupt_file_is_undecodable() {
    let dir = TempDir::new().expect("temp dir");
    let corrupt = resting_copy(&dir, "corrupt.png");

    let verdict = Prober::new().check(&corrupt, DEFAULT_MAX_PIXELS).await;

    assert_undecodable(&verdict);
}

#[tokio::test]
async fn text_file_with_an_image_name_is_not_an_image() {
    let verdict = Prober::new()
        .check(&fixture("notes.png"), DEFAULT_MAX_PIXELS)
        .await;

    assert_eq!(verdict, Err(BadReason::NotAnImage));
}

#[tokio::test]
async fn file_that_is_not_there_is_missing() {
    let verdict = Prober::new()
        .check(&fixture("no-such.png"), DEFAULT_MAX_PIXELS)
        .await;

    assert_eq!(verdict, Err(BadReason::Missing));
}

#[tokio::test]
async fn header_that_claims_more_than_max_pixels_is_too_large() {
    let verdict = Prober::new()
        .check(&fixture("huge.png"), DEFAULT_MAX_PIXELS)
        .await;

    let too_large = BadReason::TooLarge {
        width: 30000,
        height: 30000,
    };
    assert_eq!(verdict, verdict_here(Err(too_large)));
}

#[tokio::test]
async fn image_of_exactly_max_pixels_passes() {
    let verdict = Prober::new().check(&fixture("tiny.png"), TINY_PIXELS).await;

    assert_eq!(verdict, Ok(()));
}

#[tokio::test]
async fn image_of_one_pixel_more_than_max_pixels_is_too_large() {
    let verdict = Prober::new()
        .check(&fixture("tiny.png"), TINY_PIXELS - 1)
        .await;

    let too_large = BadReason::TooLarge {
        width: 16,
        height: 12,
    };
    assert_eq!(verdict, verdict_here(Err(too_large)));
}

#[tokio::test]
async fn file_with_the_stamp_of_a_passed_check_is_not_decoded_again() {
    let dir = TempDir::new().expect("temp dir");
    let image = resting_copy(&dir, "tiny.png");
    let prober = Prober::new();
    let first = prober.check(&image, DEFAULT_MAX_PIXELS).await;
    swap_content_keeping_stamp(&image, "corrupt.png");

    let second = prober.check(&image, DEFAULT_MAX_PIXELS).await;

    assert_eq!((first, second), (Ok(()), Ok(())));
}

#[tokio::test]
async fn another_prober_decodes_the_file_itself() {
    let dir = TempDir::new().expect("temp dir");
    let image = resting_copy(&dir, "tiny.png");
    let first = Prober::new().check(&image, DEFAULT_MAX_PIXELS).await;
    swap_content_keeping_stamp(&image, "corrupt.png");

    let second = Prober::new().check(&image, DEFAULT_MAX_PIXELS).await;

    assert_eq!(first, Ok(()));
    assert_undecodable(&second);
}

#[tokio::test]
async fn remembered_size_is_checked_against_the_max_pixels_of_each_call() {
    let dir = TempDir::new().expect("temp dir");
    let image = resting_copy(&dir, "tiny.png");
    let prober = Prober::new();
    let first = prober.check(&image, DEFAULT_MAX_PIXELS).await;
    swap_content_keeping_stamp(&image, "corrupt.png");

    let second = prober.check(&image, TINY_PIXELS - 1).await;

    let too_large = BadReason::TooLarge {
        width: 16,
        height: 12,
    };
    assert_eq!(first, Ok(()));
    assert_eq!(second, verdict_here(Err(too_large)));
}

#[tokio::test]
async fn failed_decode_is_remembered_too() {
    let dir = TempDir::new().expect("temp dir");
    let image = resting_copy(&dir, "corrupt.png");
    let prober = Prober::new();
    let first = prober.check(&image, DEFAULT_MAX_PIXELS).await;
    swap_content_keeping_stamp(&image, "tiny.png");

    let second = prober.check(&image, DEFAULT_MAX_PIXELS).await;

    assert_undecodable(&first);
    assert_eq!(second, first);
}

#[tokio::test]
async fn changed_file_is_decoded_again() {
    let dir = TempDir::new().expect("temp dir");
    let image = resting_copy(&dir, "corrupt.png");
    let prober = Prober::new();
    let first = prober.check(&image, DEFAULT_MAX_PIXELS).await;
    fs::copy(fixture("tiny.png"), &image).expect("repair the file");

    let second = prober.check(&image, DEFAULT_MAX_PIXELS).await;

    assert_undecodable(&first);
    assert_eq!(second, Ok(()));
}

#[tokio::test]
async fn checks_at_the_same_time_all_get_their_answer() {
    let prober = Prober::new();
    let (tiny, huge) = (fixture("tiny.png"), fixture("huge.png"));

    let (first, second, third) = tokio::join!(
        prober.check(&tiny, DEFAULT_MAX_PIXELS),
        prober.check(&tiny, DEFAULT_MAX_PIXELS),
        prober.check(&huge, DEFAULT_MAX_PIXELS),
    );

    let too_large = BadReason::TooLarge {
        width: 30000,
        height: 30000,
    };
    assert_eq!(
        (first, second, third),
        (Ok(()), Ok(()), verdict_here(Err(too_large)))
    );
}

#[tokio::test]
async fn without_bwrap_only_the_magic_bytes_are_checked() {
    if is_child_without_sandbox() {
        assert_only_the_magic_bytes_are_checked().await;
    } else {
        let empty_dir = TempDir::new().expect("temp dir");
        expect_child_to_pass(
            "without_bwrap_only_the_magic_bytes_are_checked",
            empty_dir.path(),
        );
    }
}

/// glycin itself would go on without a sandbox here. The check must not.
#[tokio::test]
async fn with_a_bwrap_that_may_not_make_namespaces_only_the_magic_bytes_are_checked() {
    if is_child_without_sandbox() {
        assert_only_the_magic_bytes_are_checked().await;
    } else {
        let bin_dir = TempDir::new().expect("temp dir");
        let refusing_bwrap = bin_dir.path().join("bwrap");
        let script =
            "#!/bin/sh\necho 'bwrap: No permissions to create a new namespace' >&2\nexit 1\n";
        fs::write(&refusing_bwrap, script).expect("write the bwrap stand-in");
        fs::set_permissions(&refusing_bwrap, fs::Permissions::from_mode(0o755))
            .expect("make the bwrap stand-in executable");
        expect_child_to_pass(
            "with_a_bwrap_that_may_not_make_namespaces_only_the_magic_bytes_are_checked",
            bin_dir.path(),
        );
    }
}

fn is_child_without_sandbox() -> bool {
    std::env::var_os(NO_SANDBOX_VAR).is_some()
}

/// Starts the test `name` again as a child process whose `PATH` is only
/// `path_dir`, and asserts that it passed. The child tells itself apart by
/// `NO_SANDBOX_VAR`.
fn expect_child_to_pass(name: &str, path_dir: &Path) {
    let output = Command::new(std::env::current_exe().expect("path of the test binary"))
        .args([name, "--exact", "--nocapture"])
        .env("PATH", path_dir)
        .env(NO_SANDBOX_VAR, "1")
        .output()
        .expect("run the child test process");
    let report = String::from_utf8_lossy(&output.stdout);
    // A name that matches no test runs none, and that is a success too.
    assert!(
        output.status.success() && report.contains("1 passed"),
        "child failed:\n{report}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What a child process without a sandbox must see: files that only a decode
/// would refuse pass, and the magic bytes still count.
async fn assert_only_the_magic_bytes_are_checked() {
    let prober = Prober::new();

    let corrupt = prober
        .check(&fixture("corrupt.png"), DEFAULT_MAX_PIXELS)
        .await;
    let huge = prober.check(&fixture("huge.png"), DEFAULT_MAX_PIXELS).await;
    let text = prober
        .check(&fixture("notes.png"), DEFAULT_MAX_PIXELS)
        .await;

    assert!(!decodes_here(), "the child must not have a sandbox");
    assert_eq!(
        (corrupt, huge, text),
        (Ok(()), Ok(()), Err(BadReason::NotAnImage))
    );
}
