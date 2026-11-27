//! The control interface on the session bus, `io.github.worxbend.Gpaperd1`.
//!
//! Each method parses its arguments, sends a [`ControlRequest`] (a core
//! [`Command`] plus a `oneshot` for the [`Reply`]) to the interpreter, and
//! waits for the reply. The interpreter gives each request a [`ReplyId`]
//! through [`PendingReplies`], feeds `Event::Control` to the core, and
//! answers `Effect::Reply` from the same table. The core stays the only owner
//! of state; this module holds only the last [`Properties`] it published.
//!
//! [`own_name`] serves the object and takes the bus name with `DO_NOT_QUEUE`
//! alone, so a second daemon gets [`OwnError::AlreadyOwned`] and exits 0.
//! [`serve`] does that and hands the interpreter its [`ControlPort`]: the
//! requests to feed to the core, and a queue for the core's [`BusNews`],
//! which one task emits in order, so a slow bus never holds up the loop.
//!
//! zbus holds the interface's read lock for the whole of a method call,
//! including the wait for the core's reply. So nothing here takes the write
//! lock: the properties live in a shared snapshot behind their own mutex,
//! and [`update_properties`] never waits for a method call to finish.
//!
//! A remote image's id holds its URL with any credentials and query. Nothing
//! that leaves this module names it so: the `Status()` document, the
//! properties and the `Changed` signal carry its [`fetch::redacted`] form.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::catalog::ItemId;
use gpaperd_core::control::{
    BUS_NAME, Command, OBJECT_PATH, Reply, ReplyId, StatusReport, StyleTarget,
};
use gpaperd_core::event::Status;
use gpaperd_core::style::Style;
use tokio::sync::{mpsc, oneshot};
use zbus::fdo::{self, RequestNameFlags};
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{Connection, connection, interface};

use crate::fetch;

/// One method call on its way to the core.
#[derive(Debug)]
pub struct ControlRequest {
    pub command: Command,
    /// Where the interpreter sends the core's reply. A dropped sender
    /// answers the caller with `org.freedesktop.DBus.Error.Failed`.
    /// Once it is closed, [`RequestInbox`] does not deliver the command.
    pub reply: oneshot::Sender<Reply>,
}

/// The values of the interface's properties, as last derived from a
/// [`Status`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    /// `"light"` or `"dark"`.
    pub style: String,
    /// The paused styles, light first.
    pub paused: Vec<String>,
    /// The light slot's item id; `""` when none.
    pub current_light: String,
    /// The dark slot's item id; `""` when none.
    pub current_dark: String,
    /// Next rotation in seconds since the Unix epoch; `0` when none.
    pub next_change: u64,
}

impl Properties {
    pub fn from_status(status: &Status) -> Properties {
        let current = |style: Style| {
            status.current[style]
                .as_ref()
                .map_or_else(String::new, |item| fetch::loggable(item).into_owned())
        };
        let paused = Style::ALL
            .into_iter()
            .filter(|&style| status.paused[style].is_some())
            .map(|style| style.as_str().to_owned())
            .collect();
        Properties {
            style: status.visible.as_str().to_owned(),
            paused,
            current_light: current(Style::Light),
            current_dark: current(Style::Dark),
            next_change: status.next_change.map_or(0, unix_seconds),
        }
    }
}

/// Whole seconds since the Unix epoch; `0` before it.
fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// How long a method call waits for the core's reply by default.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The object served at [`OBJECT_PATH`].
#[derive(Debug)]
pub struct Control {
    requests: mpsc::UnboundedSender<ControlRequest>,
    timeout: Duration,
    /// What the getters read: the last report, signaled or not.
    properties: Arc<Mutex<Properties>>,
    /// Each property's value as last announced by `PropertiesChanged`.
    signaled: Mutex<Properties>,
}

impl Control {
    /// Sends every request to `requests` and waits [`REPLY_TIMEOUT`] for
    /// each reply. Properties start empty until the first
    /// [`update_properties`].
    pub fn new(requests: mpsc::UnboundedSender<ControlRequest>) -> Control {
        Control::with_timeout(requests, REPLY_TIMEOUT)
    }

    /// As [`Control::new`], but waits `timeout` for each reply.
    pub fn with_timeout(
        requests: mpsc::UnboundedSender<ControlRequest>,
        timeout: Duration,
    ) -> Control {
        Control {
            requests,
            timeout,
            properties: Arc::default(),
            signaled: Mutex::default(),
        }
    }

    /// Sends `command` to the core and waits for its reply.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.Failed` when the interpreter is gone,
    /// dropped the request without a reply, or did not reply in time.
    /// The timeout closes the reply sender, so a command the interpreter
    /// has not taken yet is not delivered.
    pub async fn ask(&self, command: Command) -> fdo::Result<Reply> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(ControlRequest { command, reply })
            .map_err(|_| fdo::Error::Failed("gpaperd is shutting down".to_owned()))?;
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(fdo::Error::Failed("gpaperd dropped the request".to_owned())),
            Err(_) => Err(fdo::Error::Failed(format!(
                "gpaperd did not reply within {}s",
                self.timeout.as_secs_f32()
            ))),
        }
    }

    /// A copy of the current property values.
    fn snapshot(&self) -> Properties {
        lock(&self.properties).clone()
    }
}

/// Locks the snapshot. A panic while it was held cannot leave it half
/// written: every writer replaces the whole value.
fn lock(properties: &Mutex<Properties>) -> std::sync::MutexGuard<'_, Properties> {
    properties.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Reply::Done` is `Ok`; `Reply::Failed` is a `Failed` error.
fn done(reply: Reply) -> fdo::Result<()> {
    match reply {
        Reply::Done => Ok(()),
        other => Err(unexpected(other)),
    }
}

/// `Reply::Reloaded` as the `(ok, error)` pair of `Reload()`.
fn reloaded(reply: Reply) -> fdo::Result<(bool, String)> {
    match reply {
        Reply::Reloaded(Ok(())) => Ok((true, String::new())),
        Reply::Reloaded(Err(error)) => Ok((false, error)),
        other => Err(unexpected(other)),
    }
}

/// `Reply::Status` as the JSON document of `Status()`.
fn status_json(reply: Reply) -> fdo::Result<String> {
    match reply {
        Reply::Status(report) => serde_json::to_string(&redacted_report(report))
            .map_err(|error| fdo::Error::Failed(format!("status: {error}"))),
        other => Err(unexpected(other)),
    }
}

/// `report` with every item id in the form that the log uses.
fn redacted_report(mut report: StatusReport) -> StatusReport {
    let redact = |item: &mut ItemId| *item = ItemId::new(fetch::loggable(item));
    for style in Style::ALL {
        let style = &mut report.styles[style];
        let bad = style.bad.iter_mut().map(|bad| &mut bad.id);
        style.current.iter_mut().chain(bad).for_each(redact);
    }
    report
}

/// `Failed(text)` carries the core's text; any other reply is a bug in the
/// core, named without its contents.
fn unexpected(reply: Reply) -> fdo::Error {
    let name = match reply {
        Reply::Failed(text) => return fdo::Error::Failed(text),
        Reply::Done => "Done",
        Reply::Reloaded(_) => "Reloaded",
        Reply::Status(_) => "Status",
    };
    fdo::Error::Failed(format!("gpaperd sent an unexpected {name} reply"))
}

#[interface(name = "io.github.worxbend.Gpaperd1")]
impl Control {
    /// Rotate now.
    async fn next(&self, style: &str) -> fdo::Result<()> {
        done(self.ask(Command::Next(parse_style(style)?)).await?)
    }

    /// Step back through the style's history.
    async fn previous(&self, style: &str) -> fdo::Result<()> {
        done(self.ask(Command::Previous(parse_style(style)?)).await?)
    }

    /// Stop the style's timer.
    async fn pause(&self, style: &str) -> fdo::Result<()> {
        done(self.ask(Command::Pause(parse_style(style)?)).await?)
    }

    /// Restart the style's timer.
    async fn resume(&self, style: &str) -> fdo::Result<()> {
        done(self.ask(Command::Resume(parse_style(style)?)).await?)
    }

    /// Re-read the config; the error text has line and column.
    #[zbus(out_args("ok", "error"))]
    async fn reload(&self) -> fdo::Result<(bool, String)> {
        reloaded(self.ask(Command::Reload).await?)
    }

    /// Revalidate every URL now.
    async fn refresh_remote(&self) -> fdo::Result<()> {
        done(self.ask(Command::RefreshRemote).await?)
    }

    /// Write back the user's original keys and pause both styles.
    async fn restore(&self) -> fdo::Result<()> {
        done(self.ask(Command::Restore).await?)
    }

    /// The `StatusReport` JSON document.
    #[zbus(out_args("json"))]
    async fn status(&self) -> fdo::Result<String> {
        status_json(self.ask(Command::Status).await?)
    }

    #[zbus(property)]
    fn style(&self) -> String {
        self.snapshot().style
    }

    #[zbus(property)]
    fn paused(&self) -> Vec<String> {
        self.snapshot().paused
    }

    #[zbus(property)]
    fn current_light(&self) -> String {
        self.snapshot().current_light
    }

    #[zbus(property)]
    fn current_dark(&self) -> String {
        self.snapshot().current_dark
    }

    #[zbus(property)]
    fn next_change(&self) -> u64 {
        self.snapshot().next_change
    }

    /// A new image is on screen in `style`: after every successful publish,
    /// and in uri mode after the key write that shows it.
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, style: &str, id: &str) -> zbus::Result<()>;
}

/// Parses a `style` argument. Anything but the four names is
/// `org.freedesktop.DBus.Error.InvalidArgs`.
pub fn parse_style(style: &str) -> fdo::Result<StyleTarget> {
    style
        .parse()
        .map_err(|error: gpaperd_core::control::UnknownStyleTarget| {
            fdo::Error::InvalidArgs(error.to_string())
        })
}

/// Why [`own_name`] did not serve the interface.
#[derive(Debug, thiserror::Error)]
pub enum OwnError {
    /// Another process owns [`BUS_NAME`]: another daemon runs. Exit 0.
    #[error("{BUS_NAME} is already owned on the session bus: gpaperd is running")]
    AlreadyOwned,
    /// The session bus cannot be reached or refused the request.
    #[error("session bus: {0}")]
    Bus(#[from] zbus::Error),
}

/// Connects to the session bus (`DBUS_SESSION_BUS_ADDRESS`), serves
/// `control` at [`OBJECT_PATH`] and takes [`BUS_NAME`] with `DO_NOT_QUEUE`
/// only: no queueing, no replacing and no being replaced.
///
/// Must run inside the tokio runtime that drives the connection.
///
/// # Errors
///
/// [`OwnError::AlreadyOwned`] when another process holds the name;
/// [`OwnError::Bus`] for any other failure.
pub async fn own_name(control: Control) -> Result<Connection, OwnError> {
    own_name_on(connection::Builder::session(), control).await
}

/// As [`own_name`], on the bus that `builder` connects to: for tests on a
/// private bus.
///
/// # Errors
///
/// As [`own_name`].
pub async fn own_name_on(
    builder: connection::Builder<'_>,
    control: Control,
) -> Result<Connection, OwnError> {
    let connection = builder.serve_at(OBJECT_PATH, control).build().await?;
    match connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DO_NOT_QUEUE)
        .await
    {
        Ok(_) => Ok(connection),
        Err(zbus::Error::NameTaken) => Err(OwnError::AlreadyOwned),
        Err(error) => Err(OwnError::Bus(error)),
    }
}

/// The served [`Control`], for the two functions below.
///
/// # Errors
///
/// When `connection` does not serve [`Control`] at [`OBJECT_PATH`].
pub async fn control_ref(connection: &Connection) -> zbus::Result<InterfaceRef<Control>> {
    connection
        .object_server()
        .interface::<_, Control>(OBJECT_PATH)
        .await
}

/// Emits `Changed(style, id)`: for `Effect::Changed`.
///
/// # Errors
///
/// When the signal cannot be sent.
pub async fn emit_changed(
    control: &InterfaceRef<Control>,
    style: Style,
    id: &str,
) -> zbus::Result<()> {
    Control::changed(control.signal_emitter(), style.as_str(), id).await
}

/// Stores the properties derived from `status` and emits
/// `PropertiesChanged` for each one that differs from its last signaled
/// value: for `Effect::Report`.
///
/// The getters always read the new values. A property whose signal is not
/// sent keeps its old signaled value, so the next report signals it again
/// unless the property is back to that value. One failure does not skip the
/// later properties of this update.
///
/// # Errors
///
/// When a signal cannot be sent. The first such error is returned after
/// the rest have been attempted.
pub async fn update_properties(
    control: &InterfaceRef<Control>,
    status: &Status,
) -> zbus::Result<()> {
    let new = Properties::from_status(status);
    // A read lock only: a method call waiting for the core holds one too.
    let iface = control.get().await;
    *lock(&iface.properties) = new.clone();
    let mut signaled = lock(&iface.signaled).clone();
    let emitter = control.signal_emitter();
    let mut first_error = None;
    if signaled.style != new.style {
        first_error = record_signal(first_error, iface.style_changed(emitter), || {
            signaled.style.clone_from(&new.style);
        })
        .await;
    }
    if signaled.paused != new.paused {
        first_error = record_signal(first_error, iface.paused_changed(emitter), || {
            signaled.paused.clone_from(&new.paused);
        })
        .await;
    }
    if signaled.current_light != new.current_light {
        first_error = record_signal(first_error, iface.current_light_changed(emitter), || {
            signaled.current_light.clone_from(&new.current_light);
        })
        .await;
    }
    if signaled.current_dark != new.current_dark {
        first_error = record_signal(first_error, iface.current_dark_changed(emitter), || {
            signaled.current_dark.clone_from(&new.current_dark);
        })
        .await;
    }
    if signaled.next_change != new.next_change {
        first_error = record_signal(first_error, iface.next_change_changed(emitter), || {
            signaled.next_change = new.next_change;
        })
        .await;
    }
    *lock(&iface.signaled) = signaled;
    first_error.map_or(Ok(()), Err)
}

/// Waits for one property's signal, runs `on_sent` once it is sent, and
/// keeps the first send error.
///
/// The error is the result, not a `Result`, so `?` cannot skip the later
/// properties of the same update.
async fn record_signal(
    first_error: Option<zbus::Error>,
    emit: impl Future<Output = zbus::Result<()>>,
    on_sent: impl FnOnce(),
) -> Option<zbus::Error> {
    match emit.await {
        Ok(()) => {
            on_sent();
            first_error
        }
        Err(error) => first_error.or(Some(error)),
    }
}

/// What the interpreter tells the bus: [`serve_news`] emits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusNews {
    /// `Effect::Changed`: emit `Changed(style, id)`.
    Changed { style: Style, item: ItemId },
    /// `Effect::Report`: update the properties and emit `PropertiesChanged`
    /// for those that differ.
    Status(Status),
}

/// The interpreter's end of the control interface.
#[derive(Debug)]
pub struct ControlPort {
    /// Method calls on their way to the core. A caller who already gave up
    /// is not included.
    pub requests: RequestInbox,
    /// Where the core's news for the bus goes.
    pub news: mpsc::UnboundedSender<BusNews>,
}

/// Control requests the interpreter has not taken.
///
/// A reply sender that is already closed means the caller hit
/// [`REPLY_TIMEOUT`] or hung up. That command is not delivered, so it is
/// not stepped.
#[derive(Debug)]
pub struct RequestInbox {
    incoming: mpsc::UnboundedReceiver<ControlRequest>,
}

impl RequestInbox {
    /// Wraps the receiver `ask` sends to.
    pub fn new(incoming: mpsc::UnboundedReceiver<ControlRequest>) -> RequestInbox {
        RequestInbox { incoming }
    }

    /// The next command whose caller is still waiting. `None` when the
    /// sender is gone and nothing open remains.
    pub async fn recv(&mut self) -> Option<ControlRequest> {
        while let Some(request) = self.incoming.recv().await {
            if !request.reply.is_closed() {
                return Some(request);
            }
        }
        None
    }
}

impl ControlPort {
    /// A port that no bus is behind: no request ever comes, and news goes
    /// nowhere. For a daemon that runs without the session bus.
    pub fn detached() -> ControlPort {
        let (_, requests) = mpsc::unbounded_channel();
        let (news, _) = mpsc::unbounded_channel();
        ControlPort {
            requests: RequestInbox::new(requests),
            news,
        }
    }

    /// Passes `news` on. Dropped when nobody serves the bus.
    pub fn tell(&self, news: BusNews) {
        let _ = self.news.send(news);
    }
}

/// Serves the control interface on the bus that `builder` connects to, as
/// [`own_name_on`] does, and starts the task that emits the news.
///
/// Keep the connection for as long as the interface should be served. Must
/// run inside a tokio runtime.
///
/// # Errors
///
/// As [`own_name`].
pub async fn serve(
    builder: connection::Builder<'_>,
) -> Result<(Connection, ControlPort), OwnError> {
    let (request_sender, requests) = mpsc::unbounded_channel();
    let connection = own_name_on(builder, Control::new(request_sender)).await?;
    let control = control_ref(&connection).await?;
    let (news, news_inbox) = mpsc::unbounded_channel();
    tokio::spawn(serve_news(control, news_inbox));
    Ok((
        connection,
        ControlPort {
            requests: RequestInbox::new(requests),
            news,
        },
    ))
}

/// Emits each [`BusNews`] in order until the sender is gone. A signal that
/// cannot be sent is logged; the next one is tried anyway.
pub async fn serve_news(
    control: InterfaceRef<Control>,
    mut news: mpsc::UnboundedReceiver<BusNews>,
) {
    while let Some(item) = news.recv().await {
        let sent = match &item {
            BusNews::Changed { style, item } => {
                emit_changed(&control, *style, &fetch::loggable(item)).await
            }
            BusNews::Status(status) => update_properties(&control, status).await,
        };
        if let Err(error) = sent {
            tracing::warn!(reason = %error, "D-Bus signal not sent");
        }
    }
}

/// The interpreter's table of control requests waiting for the core's
/// reply.
#[derive(Debug, Default)]
pub struct PendingReplies {
    next: u64,
    waiting: HashMap<ReplyId, oneshot::Sender<Reply>>,
}

impl PendingReplies {
    /// Keeps `reply` and returns the id to put in `Event::Control`.
    pub fn register(&mut self, reply: oneshot::Sender<Reply>) -> ReplyId {
        let id = ReplyId(self.next);
        self.next = self.next.wrapping_add(1);
        self.waiting.insert(id, reply);
        id
    }

    /// Sends `reply` to the caller of `id`. An unknown id, or a caller that
    /// gave up, is ignored: the reply has nowhere to go.
    pub fn answer(&mut self, id: ReplyId, reply: Reply) {
        if let Some(sender) = self.waiting.remove(&id) {
            let _ = sender.send(reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command as Process, Stdio};
    use std::time::Duration;

    use gpaperd_core::catalog::ItemId;
    use gpaperd_core::event::Status;
    use gpaperd_core::style::{PerStyle, Style};
    use tokio::sync::mpsc;
    use zbus::connection;
    use zbus::object_server::InterfaceRef;

    use super::{Control, control_ref, lock, own_name_on, record_signal, update_properties};

    /// A private session bus, killed on drop.
    struct PrivateBus {
        child: Child,
        address: String,
    }

    impl PrivateBus {
        fn start() -> PrivateBus {
            let mut child = Process::new("dbus-daemon")
                .args(["--session", "--print-address", "--nofork", "--nopidfile"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("dbus-daemon starts");
            let stdout = child.stdout.take().expect("piped stdout");
            let mut address = String::new();
            BufReader::new(stdout)
                .read_line(&mut address)
                .expect("dbus-daemon prints its address");
            let address = address.trim().to_owned();
            assert!(!address.is_empty(), "dbus-daemon printed no address");
            PrivateBus { child, address }
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn showing(light: &str) -> Status {
        Status {
            visible: Style::Light,
            current: PerStyle {
                light: Some(ItemId::new(light)),
                dark: None,
            },
            next_change: None,
            paused: PerStyle {
                light: None,
                dark: None,
            },
        }
    }

    /// Reports `light` as shown; a send on a closed bus must fail fast.
    async fn report(control: &InterfaceRef<Control>, light: &str) -> zbus::Result<()> {
        let status = showing(light);
        tokio::time::timeout(Duration::from_secs(10), update_properties(control, &status))
            .await
            .expect("the update finishes")
    }

    #[tokio::test]
    async fn a_failed_signal_keeps_the_new_value_readable_and_unsignaled() {
        let bus = PrivateBus::start();
        let (requests, _inbox) = mpsc::unbounded_channel();
        let builder = connection::Builder::address(bus.address.as_str());
        let connection = own_name_on(builder, Control::new(requests))
            .await
            .expect("serves");
        let control = control_ref(&connection).await.expect("control is served");
        update_properties(&control, &showing("/a.jpg"))
            .await
            .expect("the first report is signaled");
        connection.clone().close().await.expect("closes");

        let sent = report(&control, "/b.jpg").await;
        assert!(sent.is_err(), "a signal went out on a closed connection");
        {
            let iface = control.get().await;
            assert_eq!(iface.current_light(), "/b.jpg", "the getter rolled back");
            assert_eq!(lock(&iface.signaled).current_light, "/a.jpg");
        }

        let sent = report(&control, "/c.jpg").await;
        assert!(sent.is_err(), "the unsignaled change was not tried again");

        let sent = report(&control, "/a.jpg").await;
        assert!(
            sent.is_ok(),
            "the value clients last saw was signaled again"
        );
        assert_eq!(control.get().await.current_light(), "/a.jpg");
    }

    #[tokio::test]
    async fn a_failed_property_signal_does_not_drop_the_next_one() {
        let mut sent = Vec::new();
        let first_error = record_signal(None, async { Err(zbus::Error::Unsupported) }, || {
            sent.push("style");
        })
        .await;
        let first_error =
            record_signal(first_error, async { Ok(()) }, || sent.push("paused")).await;
        let first_error = record_signal(
            first_error,
            async { Err(zbus::Error::MissingParameter("light")) },
            || sent.push("light"),
        )
        .await;

        assert_eq!(sent, ["paused"], "only the sent property was recorded");
        assert!(
            matches!(&first_error, Some(zbus::Error::Unsupported)),
            "the first failure was replaced or dropped: {first_error:?}"
        );
    }
}
