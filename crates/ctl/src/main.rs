//! `gpaperctl`: controls a running gpaperd over the session bus.

#![forbid(unsafe_code)]

mod summary;

use std::fmt;
use std::fs;
use std::process::ExitCode;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use gpaperd_core::control::{StatusReport, StyleTarget};
use gpaperd_ctl::{CALL_TIMEOUT, Gpaperd1Proxy, is_timeout};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use zbus::proxy::CacheProperties;
use zbus::{DBusError, connection, fdo};

#[derive(Debug, Parser)]
#[command(version, about = "Control a running gpaperd")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Cmd {
    /// Rotate now.
    Next(StyleArg),
    /// Show the previous image again.
    Previous(StyleArg),
    /// Show one image of the style's sources now.
    Set {
        #[command(flatten)]
        style: StyleArg,
        /// A local file or a URL, as the config lists it.
        image: String,
    },
    /// Ban the image on screen for good and show the next one.
    Ban(StyleArg),
    /// Let a banned image be shown again.
    Unban {
        /// The image as `status` lists it: a path or a URL.
        id: String,
    },
    /// Copy the image on screen into favorites_dir.
    Favorite(StyleArg),
    /// Stop rotating.
    Pause(StyleArg),
    /// Start rotating again.
    Resume(StyleArg),
    /// Re-read the config file.
    Reload,
    /// Fetch every remote image now.
    Refresh,
    /// Put the original wallpaper back and pause both styles.
    Restore,
    /// Show what the daemon is doing.
    Status {
        /// Print the daemon's JSON document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, PartialEq, Eq, clap::Args)]
struct StyleArg {
    /// light, dark, visible or both.
    #[arg(long, default_value = "visible", value_parser = StyleTarget::from_str)]
    style: StyleTarget,
}

/// Why a command did not succeed. Every one exits 1.
#[derive(Debug)]
enum Failure {
    /// Nobody owns the daemon's bus name.
    NotRunning,
    /// The daemon owns its name but sent no answer within [`CALL_TIMEOUT`].
    NoAnswer,
    /// The session bus itself cannot be reached.
    NoBus(zbus::Error),
    /// `set` names a local file that cannot be resolved, or whose path is
    /// not UTF-8 and so has no catalog id.
    NoImage { image: String, reason: String },
    /// `Reload` answered `(false, error)`.
    ReloadFailed(String),
    /// Any other D-Bus error, the daemon's own included.
    Bus(zbus::Error),
    /// `Status` sent a document this client cannot read.
    BadStatus(serde_json::Error),
}

impl Failure {
    /// Tells a missing daemon apart from other errors of a call.
    fn from_call(error: zbus::Error) -> Failure {
        if names_missing_daemon(&error) {
            Failure::NotRunning
        } else if is_timeout(&error) {
            Failure::NoAnswer
        } else {
            Failure::Bus(error)
        }
    }
}

/// The bus's answers to a call to a name that has no owner.
fn names_missing_daemon(error: &zbus::Error) -> bool {
    const NO_OWNER: [&str; 2] = [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    match error {
        zbus::Error::MethodError(name, _, _) => NO_OWNER.contains(&name.as_str()),
        zbus::Error::FDO(error) => matches!(
            **error,
            fdo::Error::ServiceUnknown(_) | fdo::Error::NameHasNoOwner(_)
        ),
        _ => false,
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::NotRunning => f.write_str("gpaperd is not running"),
            Failure::NoAnswer => f.write_str("gpaperd did not answer"),
            Failure::NoBus(error) => write!(f, "cannot reach the session bus: {error}"),
            Failure::NoImage { image, reason } => write!(
                f,
                "cannot find {}: {}",
                summary::replace_controls(image),
                summary::replace_controls(reason)
            ),
            Failure::ReloadFailed(error) => {
                write!(f, "reload failed: {}", summary::replace_controls(error))
            }
            Failure::Bus(error) => match error_message(error) {
                Some(message) => write!(f, "{}", summary::replace_controls(message)),
                None => write!(f, "{}", summary::replace_controls(&error.to_string())),
            },
            Failure::BadStatus(error) => write!(
                f,
                "cannot read the daemon's status: {}",
                summary::replace_controls(&error.to_string())
            ),
        }
    }
}

/// The text a D-Bus error reply carries, without its error name.
fn error_message(error: &zbus::Error) -> Option<&str> {
    match error {
        zbus::Error::MethodError(_, detail, _) => detail.as_deref(),
        zbus::Error::FDO(error) => error.description(),
        _ => None,
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match zbus::block_on(run(cli.command)) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("gpaperctl: {failure}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one command against the daemon and returns what to print.
async fn run(command: Cmd) -> Result<String, Failure> {
    let connection = connection::Builder::session()
        .method_timeout(CALL_TIMEOUT)
        .build()
        .await
        .map_err(Failure::NoBus)?;
    let daemon = Gpaperd1Proxy::builder(&connection)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(Failure::Bus)?;
    match command {
        Cmd::Next(arg) => daemon.next(arg.style.as_str()).await,
        Cmd::Previous(arg) => daemon.previous(arg.style.as_str()).await,
        Cmd::Set { style, image } => daemon.set(style.style.as_str(), &image_id(&image)?).await,
        Cmd::Ban(arg) => daemon.ban(arg.style.as_str()).await,
        Cmd::Unban { id } => daemon.unban(&id).await,
        Cmd::Favorite(arg) => daemon.favorite(arg.style.as_str()).await,
        Cmd::Pause(arg) => daemon.pause(arg.style.as_str()).await,
        Cmd::Resume(arg) => daemon.resume(arg.style.as_str()).await,
        Cmd::Refresh => daemon.refresh_remote().await,
        Cmd::Restore => daemon.restore().await,
        Cmd::Reload => return reload(&daemon).await,
        Cmd::Status { json } => return status(&daemon, json).await,
    }
    .map_err(Failure::from_call)?;
    Ok(String::new())
}

/// The `id` that `Set` takes for `image`. A URL goes as typed: the daemon
/// puts it in the form the catalog keeps. A path is made absolute and
/// canonical here, against this process's working directory, as the
/// daemon's scanner makes catalog ids.
fn image_id(image: &str) -> Result<String, Failure> {
    // The config tells a URL from a path the same way.
    if image.contains("://") {
        return Ok(image.to_owned());
    }
    let no_image = |reason: String| Failure::NoImage {
        image: image.to_owned(),
        reason,
    };
    let path = fs::canonicalize(image).map_err(|error| no_image(error.to_string()))?;
    path.into_os_string()
        .into_string()
        .map_err(|_| no_image("the path is not UTF-8".to_owned()))
}

async fn reload(daemon: &Gpaperd1Proxy<'_>) -> Result<String, Failure> {
    let (ok, error) = daemon.reload().await.map_err(Failure::from_call)?;
    if ok {
        Ok(String::new())
    } else {
        Err(Failure::ReloadFailed(error))
    }
}

/// The daemon's JSON as sent, or the human summary of it.
async fn status(daemon: &Gpaperd1Proxy<'_>, json: bool) -> Result<String, Failure> {
    let document = daemon.status().await.map_err(Failure::from_call)?;
    if json {
        return Ok(document + "\n");
    }
    let report: StatusReport = serde_json::from_str(&document).map_err(Failure::BadStatus)?;
    Ok(summary::summary(
        &report,
        Timestamp::now(),
        &TimeZone::system(),
    ))
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cmd, clap::Error> {
        Cli::try_parse_from(std::iter::once("gpaperctl").chain(args.iter().copied()))
            .map(|cli| cli.command)
    }

    fn style(style: StyleTarget) -> StyleArg {
        StyleArg { style }
    }

    #[test]
    fn style_commands_default_to_the_visible_style() {
        assert_eq!(
            parse(&["next"]).ok(),
            Some(Cmd::Next(style(StyleTarget::Visible)))
        );
        assert_eq!(
            parse(&["previous"]).ok(),
            Some(Cmd::Previous(style(StyleTarget::Visible)))
        );
        assert_eq!(
            parse(&["pause"]).ok(),
            Some(Cmd::Pause(style(StyleTarget::Visible)))
        );
        assert_eq!(
            parse(&["resume"]).ok(),
            Some(Cmd::Resume(style(StyleTarget::Visible)))
        );
        assert_eq!(
            parse(&["ban"]).ok(),
            Some(Cmd::Ban(style(StyleTarget::Visible)))
        );
        assert_eq!(
            parse(&["favorite"]).ok(),
            Some(Cmd::Favorite(style(StyleTarget::Visible)))
        );
    }

    #[test]
    fn unban_takes_the_id_as_typed() {
        let parsed = parse(&["unban", "/pics/gone.jpg"]).ok();
        let expected = Cmd::Unban {
            id: "/pics/gone.jpg".to_owned(),
        };
        assert_eq!(parsed, Some(expected));
    }

    #[test]
    fn unban_needs_an_id() {
        let error = parse(&["unban"]).expect_err("rejected");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn style_takes_each_target_name() {
        for target in [
            StyleTarget::Light,
            StyleTarget::Dark,
            StyleTarget::Visible,
            StyleTarget::Both,
        ] {
            let parsed = parse(&["pause", "--style", target.as_str()]).ok();
            assert_eq!(parsed, Some(Cmd::Pause(style(target))));
        }
    }

    #[test]
    fn an_unknown_style_is_a_usage_error() {
        let error = parse(&["next", "--style", "all"]).expect_err("rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn set_takes_an_image_and_defaults_to_the_visible_style() {
        let parsed = parse(&["set", "day.jpg"]).ok();
        let expected = Cmd::Set {
            style: style(StyleTarget::Visible),
            image: "day.jpg".to_owned(),
        };
        assert_eq!(parsed, Some(expected));

        let parsed = parse(&["set", "--style", "both", "https://example.com/a.jpg"]).ok();
        let expected = Cmd::Set {
            style: style(StyleTarget::Both),
            image: "https://example.com/a.jpg".to_owned(),
        };
        assert_eq!(parsed, Some(expected));
    }

    #[test]
    fn set_needs_an_image() {
        let error = parse(&["set"]).expect_err("rejected");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn an_unresolved_image_names_the_image_and_why() {
        let failure = image_id("/no/such/dir/x\u{1b}.jpg").expect_err("missing");
        let shown = failure.to_string();
        assert!(
            shown.starts_with("cannot find /no/such/dir/x?.jpg: "),
            "{shown}"
        );
    }

    #[test]
    fn a_url_image_is_sent_as_typed() {
        assert_eq!(
            image_id("HTTPS://Example.com/a.jpg").ok().as_deref(),
            Some("HTTPS://Example.com/a.jpg")
        );
    }

    #[test]
    fn plain_commands_parse() {
        assert_eq!(parse(&["reload"]).ok(), Some(Cmd::Reload));
        assert_eq!(parse(&["refresh"]).ok(), Some(Cmd::Refresh));
        assert_eq!(parse(&["restore"]).ok(), Some(Cmd::Restore));
        assert_eq!(parse(&["status"]).ok(), Some(Cmd::Status { json: false }));
        assert_eq!(
            parse(&["status", "--json"]).ok(),
            Some(Cmd::Status { json: true })
        );
    }

    #[test]
    fn a_command_is_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["refresh-remote"]).is_err());
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn missing_daemon_errors_read_as_not_running() {
        let unknown = fdo::Error::ServiceUnknown("no such name".to_owned());
        let failure = Failure::from_call(zbus::Error::FDO(Box::new(unknown)));
        assert_eq!(failure.to_string(), "gpaperd is not running");

        let other = fdo::Error::Failed("busy".to_owned());
        let failure = Failure::from_call(zbus::Error::FDO(Box::new(other)));
        assert!(matches!(failure, Failure::Bus(_)));
        assert_eq!(failure.to_string(), "busy");
    }

    #[test]
    fn a_call_without_an_answer_reads_as_no_answer() {
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        let error = zbus::Error::InputOutput(std::sync::Arc::new(timed_out));
        let failure = Failure::from_call(error);
        assert_eq!(failure.to_string(), "gpaperd did not answer");
    }

    #[test]
    fn reload_failure_shows_the_daemon_text() {
        let failure = Failure::ReloadFailed("line 3, column 7: bad interval".to_owned());
        assert_eq!(
            failure.to_string(),
            "reload failed: line 3, column 7: bad interval"
        );
    }

    #[test]
    fn reload_and_bus_errors_replace_controls() {
        let reload = Failure::ReloadFailed("line 1\r\n\0\u{1b}: bad".to_owned());
        assert_eq!(reload.to_string(), "reload failed: line 1????: bad");

        let bus = fdo::Error::Failed("no\r\n\0\u{1b}keys".to_owned());
        let failure = Failure::from_call(zbus::Error::FDO(Box::new(bus)));
        assert_eq!(failure.to_string(), "no????keys");

        let unknown = r#"{"version":1,"visible":"\u001b]0;x\u0007"}"#;
        let error = serde_json::from_str::<StatusReport>(unknown).unwrap_err();
        let shown = Failure::BadStatus(error).to_string();
        assert!(shown.starts_with("cannot read the daemon's status: "));
        assert!(!shown.contains(char::is_control), "{shown:?}");
    }
}
