//! `gpaperd check-config`: loads the config file as the daemon would, scans
//! each local source and looks each remote one up in the cache, then prints
//! what it found. For dotfile CI.
//!
//! It only reads. It talks to neither GNOME nor the session bus, and it
//! creates no slot, state file, cache entry or directory. A scan lists and
//! `stat`s files; it opens none, so an image counted here may still fail the
//! daemon's check before use.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gpaperd_core::catalog::LocalFile;
use gpaperd_core::config::Source;
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_daemon::cache::Cache;
use gpaperd_daemon::config_watch::{ConfigFile, LoadError};
use gpaperd_daemon::fetch;
use gpaperd_daemon::paths::Paths;
use gpaperd_daemon::scan::{self, ScanWarning};
use url::Url;

use crate::{EXIT_CONFIG, EXIT_FAILURE, config_file, environment};

/// `EX_NOINPUT`: there is no config file to check.
const EXIT_NO_INPUT: u8 = 66;

/// Runs the check and prints the report on stdout, or the error in the log.
/// Exit codes: 0 valid, 66 no such file, 78 a file that cannot be used
/// (with line and column when it does not parse), 1 no `HOME` or an
/// unreadable remote cache.
pub fn run(config_override: Option<&Path>) -> ExitCode {
    let paths = match Paths::resolve(&environment, config_override) {
        Ok(paths) => paths,
        Err(error) => {
            tracing::error!("cannot locate gpaperd's directories: {error}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let cache = Cache::new(paths.remote_cache_dir.clone());
    match check(&config_file(&paths), &paths.remote_cache_dir, &cache) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// Why the config file did not pass.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("config file {}: no such file", .0.display())]
    Missing(PathBuf),
    #[error("config file {}: {reason}", file.display())]
    Unusable { file: PathBuf, reason: LoadError },
    /// The cache root could not be listed. A missing root is not this error.
    #[error("remote cache {}: {reason}", dir.display())]
    CacheUnread { dir: PathBuf, reason: io::Error },
}

impl CheckError {
    fn exit_code(&self) -> u8 {
        match self {
            CheckError::Missing(_) => EXIT_NO_INPUT,
            CheckError::Unusable { .. } => EXIT_CONFIG,
            CheckError::CacheUnread { .. } => EXIT_FAILURE,
        }
    }
}

/// What the check found: each style's sources, in config order.
#[derive(Debug)]
pub struct Report {
    file: PathBuf,
    styles: PerStyle<Vec<SourceReport>>,
}

/// One source and what it holds now.
#[derive(Debug)]
struct SourceReport {
    /// The path, or the URL without credentials and query.
    name: String,
    found: Found,
}

#[derive(Debug)]
enum Found {
    /// Image files by name, and what the scan skipped.
    Local {
        images: usize,
        skipped: Vec<ScanWarning>,
    },
    /// Whether the cache holds a version from an earlier run.
    Remote { cached: bool },
}

/// Loads `file`, scans its local sources and looks its URLs up in `cache`.
/// `cache_dir` is the root `cache` reads. Blocking I/O, read only.
///
/// # Errors
///
/// [`CheckError::Missing`] when there is no such file,
/// [`CheckError::Unusable`] when it cannot be read or does not validate, and
/// [`CheckError::CacheUnread`] when the cache directory cannot be listed. A
/// missing cache directory is an empty cache, not an error.
pub fn check(
    file: &ConfigFile,
    cache_dir: &Path,
    cache: &Cache,
) -> Result<Report, CheckError> {
    let config = match file.load() {
        Ok(Some(config)) => config,
        Ok(None) => return Err(CheckError::Missing(file.path.clone())),
        Err(reason) => {
            return Err(CheckError::Unusable {
                file: file.path.clone(),
                reason,
            });
        }
    };
    // `Cache::load` maps every listing error to an empty set. A missing
    // directory stays empty; any other error must not become "not cached".
    list_cache_dir(cache_dir)?;
    let cached: BTreeSet<Url> = cache.load().into_iter().map(|blob| blob.url).collect();
    let styles = PerStyle::from_fn(|style| {
        config.styles[style]
            .sources
            .iter()
            .map(|source| source_report(source, &cached))
            .collect()
    });
    Ok(Report {
        file: file.path.clone(),
        styles,
    })
}

fn source_report(source: &Source, cached: &BTreeSet<Url>) -> SourceReport {
    match source {
        Source::Url(url) => SourceReport {
            name: fetch::redacted(url),
            found: Found::Remote {
                cached: cached.contains(url),
            },
        },
        Source::Path(path) | Source::File(path) | Source::Dir { path, .. } => {
            let scan = scan::scan(std::slice::from_ref(source));
            SourceReport {
                name: path.display().to_string(),
                found: Found::Local {
                    images: distinct_images(&scan.files),
                    skipped: scan.warnings,
                },
            }
        }
    }
}

/// One image per inode. A file reached twice, including through a symlink,
/// counts once.
fn distinct_images(files: &[LocalFile]) -> usize {
    files
        .iter()
        .map(|file| file.stamp.id)
        .collect::<BTreeSet<_>>()
        .len()
}

/// Lists `dir` the way the cache load does. A missing directory is empty.
/// Any other error is [`CheckError::CacheUnread`], because load would hide it.
fn list_cache_dir(dir: &Path) -> Result<(), CheckError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(cache_unread(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| cache_unread(dir, error))?;
        // `file_type` is part of the listing `Cache::load` hides on error.
        let _ = entry.file_type().map_err(|error| cache_unread(dir, error))?;
    }
    Ok(())
}

fn cache_unread(dir: &Path, reason: io::Error) -> CheckError {
    CheckError::CacheUnread {
        dir: dir.to_path_buf(),
        reason,
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "config file {}: valid", self.file.display())?;
        for style in Style::ALL {
            writeln!(f, "{style}:")?;
            let sources = &self.styles[style];
            if sources.is_empty() {
                writeln!(f, "  no sources")?;
            }
            for source in sources {
                write!(f, "  {}: ", source.name)?;
                match &source.found {
                    Found::Local { images, skipped } => {
                        let noun = if *images == 1 { "image" } else { "images" };
                        writeln!(f, "{images} {noun}")?;
                        for warning in skipped {
                            let path = warning.path.display();
                            writeln!(f, "    skipped {path}: {}", warning.reason)?;
                        }
                    }
                    Found::Remote { cached: true } => writeln!(f, "remote, cached")?,
                    Found::Remote { cached: false } => writeln!(f, "remote, not cached")?,
                }
            }
        }
        Ok(())
    }
}
