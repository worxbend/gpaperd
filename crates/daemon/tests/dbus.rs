//! The control interface on a private `dbus-daemon`, one per test, driven
//! by a fake command handler in place of the core.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use gpaperd_core::catalog::ItemId;
use gpaperd_core::control::{BadItem, Command, Reply, StatusReport, StyleReport, StyleTarget};
use gpaperd_core::event::{Pause, Status};
use gpaperd_core::style::{PerStyle, Style};
use gpaperd_ctl::Gpaperd1Proxy;
use gpaperd_daemon::dbus::{
    BusNews, Control, ControlPort, ControlRequest, OwnError, REPLY_TIMEOUT, RequestInbox,
    control_ref, emit_changed, own_name_on, serve_news, update_properties,
};
use tokio::sync::mpsc;
use zbus::{Connection, DBusError, connection};

/// Waits this long for any one bus event before the test fails.
const STEP: Duration = Duration::from_secs(10);

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

    fn builder(&self) -> connection::Builder<'static> {
        connection::Builder::address(self.address.as_str())
    }

    async fn client(&self) -> Gpaperd1Proxy<'static> {
        let connection = self.builder().build().await.expect("client connects");
        Gpaperd1Proxy::new(&connection).await.expect("proxy builds")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The commands the fake core saw, in order.
type Seen = Arc<Mutex<Vec<Command>>>;

/// Serves `Control` on `bus` with a fake core that records each command
/// and answers it with `answer`.
async fn serve(
    bus: &PrivateBus,
    timeout: Duration,
    answer: impl Fn(Command) -> Option<Reply> + Send + 'static,
) -> (Connection, Seen) {
    let (requests, incoming) = mpsc::unbounded_channel::<ControlRequest>();
    let mut incoming = RequestInbox::new(incoming);
    let connection = own_name_on(bus.builder(), Control::with_timeout(requests, timeout))
        .await
        .expect("owns the name");
    let seen = Seen::default();
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        // Unanswered requests stay here, so their callers time out rather
        // than see a dropped sender.
        let mut ignored = Vec::new();
        while let Some(request) = incoming.recv().await {
            record.lock().unwrap().push(request.command.clone());
            match answer(request.command) {
                Some(reply) => {
                    let _ = request.reply.send(reply);
                }
                None => ignored.push(request.reply),
            }
        }
    });
    (connection, seen)
}

fn report() -> StatusReport {
    StatusReport {
        version: 1,
        visible: Style::Dark,
        next_change: Some(1_800_000_000),
        styles: PerStyle {
            light: StyleReport {
                current: Some(ItemId::new("/pics/a.jpg")),
                paused: vec![Pause::User],
                items: 3,
                usable: 2,
                bad: Vec::new(),
            },
            dark: StyleReport::default(),
        },
    }
}

/// The fake core's answers: what a healthy core sends.
fn healthy(command: Command) -> Option<Reply> {
    Some(match command {
        Command::Reload => Reply::Reloaded(Err("line 3, column 7: bad interval".to_owned())),
        Command::Status => Reply::Status(report()),
        _ => Reply::Done,
    })
}

fn error_name(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, _, _) => name.as_str().to_owned(),
        zbus::Error::FDO(error) => error.name().as_str().to_owned(),
        other => panic!("expected a D-Bus error reply, got {other:?}"),
    }
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("bus event in time")
}

#[tokio::test]
async fn every_method_round_trips_through_the_command_queue() {
    let bus = PrivateBus::start();
    let (_server, seen) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;

    within(client.next("visible")).await.unwrap();
    within(client.previous("light")).await.unwrap();
    within(client.pause("both")).await.unwrap();
    within(client.resume("dark")).await.unwrap();
    let reload = within(client.reload()).await.unwrap();
    within(client.refresh_remote()).await.unwrap();
    within(client.restore()).await.unwrap();
    let json = within(client.status()).await.unwrap();

    assert_eq!(reload, (false, "line 3, column 7: bad interval".to_owned()));
    let parsed: StatusReport = serde_json::from_str(&json).expect("status is the report");
    assert_eq!(parsed, report());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Command::Next(StyleTarget::Visible),
            Command::Previous(StyleTarget::Light),
            Command::Pause(StyleTarget::Both),
            Command::Resume(StyleTarget::Dark),
            Command::Reload,
            Command::RefreshRemote,
            Command::Restore,
            Command::Status,
        ]
    );
}

#[tokio::test]
async fn a_successful_reload_is_ok_with_an_empty_error() {
    let bus = PrivateBus::start();
    let (_server, _) = serve(&bus, Duration::from_secs(30), |_| {
        Some(Reply::Reloaded(Ok(())))
    })
    .await;
    let client = bus.client().await;

    assert_eq!(
        within(client.reload()).await.unwrap(),
        (true, String::new())
    );
}

#[tokio::test]
async fn a_failed_reply_is_a_failed_error_with_its_text() {
    let bus = PrivateBus::start();
    let (_server, _) = serve(&bus, Duration::from_secs(30), |_| {
        Some(Reply::Failed("no original keys saved".to_owned()))
    })
    .await;
    let client = bus.client().await;

    let error = within(client.restore()).await.unwrap_err();

    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.Failed");
    assert!(
        error.to_string().contains("no original keys saved"),
        "{error}"
    );
}

#[tokio::test]
async fn an_unknown_style_is_invalid_args_and_never_reaches_the_core() {
    let bus = PrivateBus::start();
    let (_server, seen) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;

    for bad in ["", "Light", "all", "visible "] {
        for result in [
            within(client.next(bad)).await,
            within(client.previous(bad)).await,
            within(client.pause(bad)).await,
            within(client.resume(bad)).await,
        ] {
            let error = result.unwrap_err();
            assert_eq!(
                error_name(&error),
                "org.freedesktop.DBus.Error.InvalidArgs",
                "{bad:?}"
            );
        }
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn set_passes_the_style_and_the_catalog_id_to_the_core() {
    let bus = PrivateBus::start();
    let (_server, seen) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;

    within(client.set("light", "/pics/a b.jpg")).await.unwrap();
    within(client.set("both", "HTTPS://Example.com/x.jpg"))
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Command::Set(StyleTarget::Light, ItemId::new("/pics/a b.jpg")),
            Command::Set(StyleTarget::Both, ItemId::new("https://example.com/x.jpg")),
        ]
    );
}

#[tokio::test]
async fn set_of_no_absolute_path_or_web_url_is_invalid_args_and_never_reaches_the_core() {
    let bus = PrivateBus::start();
    let (_server, seen) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;

    for (style, id) in [
        ("light", "pics/a.jpg"),
        ("light", "ftp://example.com/a.jpg"),
        ("light", ""),
        ("all", "/pics/a.jpg"),
    ] {
        let error = within(client.set(style, id)).await.unwrap_err();
        assert_eq!(
            error_name(&error),
            "org.freedesktop.DBus.Error.InvalidArgs",
            "{style:?} {id:?}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_rejected_reply_is_invalid_args_with_its_text() {
    let bus = PrivateBus::start();
    let (_server, _) = serve(&bus, Duration::from_secs(30), |_| {
        Some(Reply::Rejected(
            "the image is not in the sources of dark".to_owned(),
        ))
    })
    .await;
    let client = bus.client().await;

    let error = within(client.set("dark", "/pics/a.jpg")).await.unwrap_err();

    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");
    assert!(
        error
            .to_string()
            .contains("the image is not in the sources of dark"),
        "{error}"
    );
}

#[tokio::test]
async fn a_handler_that_never_replies_times_out_with_failed() {
    let bus = PrivateBus::start();
    let (_server, seen) = serve(&bus, Duration::from_millis(200), |_| None).await;
    let client = bus.client().await;

    let error = within(client.next("visible")).await.unwrap_err();

    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.Failed");
    assert!(error.to_string().contains("did not reply"), "{error}");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Command::Next(StyleTarget::Visible)]
    );
}

#[tokio::test]
async fn a_caller_who_is_still_waiting_is_delivered() {
    let (sender, incoming) = mpsc::unbounded_channel();
    let (news, _news) = mpsc::unbounded_channel();
    let mut port = ControlPort {
        requests: RequestInbox::new(incoming),
        news,
    };
    let control = Control::with_timeout(sender, REPLY_TIMEOUT);
    let asking = tokio::spawn(async move { control.ask(Command::Status).await });

    let request = port
        .requests
        .recv()
        .await
        .expect("the command is delivered");
    assert_eq!(request.command, Command::Status);
    request
        .reply
        .send(Reply::Status(report()))
        .expect("the caller is still waiting");

    let reply = asking.await.unwrap().unwrap();
    assert!(matches!(reply, Reply::Status(_)));
}

#[tokio::test]
async fn a_timed_out_command_is_not_delivered() {
    let (sender, incoming) = mpsc::unbounded_channel();
    let (news, _news) = mpsc::unbounded_channel();
    let mut port = ControlPort {
        requests: RequestInbox::new(incoming),
        news,
    };
    let control = Control::with_timeout(sender, Duration::from_millis(50));

    let error = control
        .ask(Command::Next(StyleTarget::Visible))
        .await
        .unwrap_err();

    drop(control);
    assert!(error.to_string().contains("did not reply"), "{error}");
    assert!(
        port.requests.recv().await.is_none(),
        "timed-out command was delivered"
    );
}

#[tokio::test]
async fn a_dropped_reply_is_a_failed_error() {
    let bus = PrivateBus::start();
    let (requests, mut incoming) = mpsc::unbounded_channel::<ControlRequest>();
    let _server = own_name_on(bus.builder(), Control::new(requests))
        .await
        .expect("owns the name");
    tokio::spawn(async move { while incoming.recv().await.is_some() {} });
    let client = bus.client().await;

    let error = within(client.status()).await.unwrap_err();

    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.Failed");
}

#[tokio::test]
async fn a_second_owner_gets_already_owned() {
    let bus = PrivateBus::start();
    let (_first, _) = serve(&bus, Duration::from_secs(30), healthy).await;
    let (requests, _incoming) = mpsc::unbounded_channel();

    let second = own_name_on(bus.builder(), Control::new(requests)).await;

    assert!(matches!(second, Err(OwnError::AlreadyOwned)), "{second:?}");
}

#[tokio::test]
async fn an_unreachable_bus_is_a_bus_error() {
    let (requests, _incoming) = mpsc::unbounded_channel();
    let builder = connection::Builder::address("unix:path=/nonexistent/gpaperd-test-bus");

    let result = own_name_on(builder, Control::new(requests)).await;

    assert!(matches!(result, Err(OwnError::Bus(_))), "{result:?}");
}

#[tokio::test]
async fn changed_reaches_a_client() {
    let bus = PrivateBus::start();
    let (server, _) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;
    let mut changes = client.receive_changed().await.expect("subscribes");
    let control = control_ref(&server).await.expect("control is served");

    emit_changed(&control, Style::Dark, "/pics/night.png")
        .await
        .expect("emits");

    let signal = within(changes.next()).await.expect("a Changed signal");
    let args = signal.args().expect("signal args");
    assert_eq!(args.style(), &"dark");
    assert_eq!(args.id(), &"/pics/night.png");
}

fn status(light: &str, paused_dark: bool) -> Status {
    Status {
        visible: Style::Light,
        current: PerStyle {
            light: Some(ItemId::new(light)),
            dark: None,
        },
        next_change: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        paused: PerStyle {
            light: None,
            dark: paused_dark.then_some(Pause::User),
        },
    }
}

#[tokio::test]
async fn properties_follow_the_status_and_announce_each_change() {
    let bus = PrivateBus::start();
    let (server, _) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;
    let control = control_ref(&server).await.expect("control is served");
    let mut lights = client.receive_current_light_changed().await;
    let mut pauses = client.receive_paused_changed().await;

    update_properties(&control, &status("/pics/a.jpg", false))
        .await
        .expect("updates");
    let light = within(lights.next()).await.expect("CurrentLight changed");
    assert_eq!(light.get().await.unwrap(), "/pics/a.jpg");

    update_properties(&control, &status("/pics/b.jpg", true))
        .await
        .expect("updates");
    let light = within(lights.next()).await.expect("CurrentLight changed");
    assert_eq!(light.get().await.unwrap(), "/pics/b.jpg");
    let paused = within(pauses.next()).await.expect("Paused changed");
    assert_eq!(paused.get().await.unwrap(), vec!["dark".to_owned()]);

    assert_eq!(within(client.style()).await.unwrap(), "light");
    assert_eq!(within(client.current_dark()).await.unwrap(), "");
    assert_eq!(within(client.next_change()).await.unwrap(), 1_800_000_000);
}

#[tokio::test]
async fn updating_properties_does_not_wait_for_a_method_call_in_flight() {
    let bus = PrivateBus::start();
    let (server, seen) = serve(&bus, Duration::from_secs(30), |_| None).await;
    let client = bus.client().await;
    let control = control_ref(&server).await.expect("control is served");
    let pending = tokio::spawn(async move { client.status().await });
    within(async {
        while seen.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    within(update_properties(&control, &status("/pics/a.jpg", false)))
        .await
        .expect("updates");

    pending.abort();
}

/// A remote image's id: its URL as the config lists it, with credentials
/// and a query that must not leave the daemon.
const SECRET_URL: &str = "https://me:hunter2@img.test/day.jpg?token=s3cret";
/// How the bus names that image.
const REDACTED_URL: &str = "https://img.test/day.jpg";

fn assert_no_secret(text: &str) {
    for secret in ["me:", "hunter2", "token", "s3cret"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[tokio::test]
async fn status_names_a_remote_image_without_credentials_or_query() {
    let bus = PrivateBus::start();
    let answer = |_| {
        let mut report = report();
        report.styles.light.current = Some(ItemId::new(SECRET_URL));
        report.styles.dark.bad = vec![BadItem {
            id: ItemId::new(SECRET_URL),
            reason: "not an image".to_owned(),
        }];
        Some(Reply::Status(report))
    };
    let (_server, _) = serve(&bus, Duration::from_secs(30), answer).await;
    let client = bus.client().await;

    let json = within(client.status()).await.unwrap();

    assert_no_secret(&json);
    let parsed: StatusReport = serde_json::from_str(&json).expect("status is the report");
    assert_eq!(parsed.styles.light.current, Some(ItemId::new(REDACTED_URL)));
    assert_eq!(parsed.styles.dark.bad[0].id, ItemId::new(REDACTED_URL));
}

#[tokio::test]
async fn properties_name_a_remote_image_without_credentials_or_query() {
    let bus = PrivateBus::start();
    let (server, _) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;
    let control = control_ref(&server).await.expect("control is served");

    update_properties(&control, &status(SECRET_URL, false))
        .await
        .expect("updates");

    let light = within(client.current_light()).await.unwrap();
    assert_eq!(light, REDACTED_URL);
}

#[tokio::test]
async fn changed_names_a_remote_image_without_credentials_or_query() {
    let bus = PrivateBus::start();
    let (server, _) = serve(&bus, Duration::from_secs(30), healthy).await;
    let client = bus.client().await;
    let mut changes = client.receive_changed().await.expect("subscribes");
    let control = control_ref(&server).await.expect("control is served");
    let (news, inbox) = mpsc::unbounded_channel();
    tokio::spawn(serve_news(control, inbox));

    let item = ItemId::new(SECRET_URL);
    news.send(BusNews::Changed {
        style: Style::Light,
        item,
    })
    .expect("the news task listens");

    let signal = within(changes.next()).await.expect("a Changed signal");
    let args = signal.args().expect("signal args");
    assert_eq!(args.id(), &REDACTED_URL);
}
