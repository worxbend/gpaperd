//! Remote images: the Remote rows of the plan's corner-case table. Each
//! server is `wiremock` on 127.0.0.1, and the daemon's HTTP client goes
//! direct, so no request leaves the machine. The fetcher refuses a literal
//! loopback host, so every URL names `localhost`.

use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use gpaperd_core::catalog::ItemId;
use gpaperd_core::event::{Connectivity, Status};
use gpaperd_core::style::{PerStyle, Style};
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::world::{
    ALPHABETICAL, DARK_INTERVAL, Daemon, LIGHT_INTERVAL, PATIENCE, SEED, World, eventually,
    image_bytes, let_the_daemon_run, scenario,
};

const ROUTE: &str = "/walls/dawn.png";

fn image(mark: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(image_bytes(mark))
        .insert_header("Content-Type", "image/png")
}

async fn serve(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(ROUTE))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Answers a request that carries `If-None-Match: etag` with `answer`, once.
async fn serve_once_to_revalidation(server: &MockServer, etag: &str, answer: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(ROUTE))
        .and(header("If-None-Match", etag))
        .respond_with(answer)
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
}

fn url_on(server: &MockServer) -> Url {
    url_at(server.address().port())
}

/// `ROUTE` on `localhost`: a literal loopback host is refused before the
/// GET, and this name still reaches a listener bound on 127.0.0.1.
fn url_at(port: u16) -> Url {
    Url::parse(&format!("http://localhost:{port}{ROUTE}")).expect("a valid URL")
}

async fn requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .len()
}

async fn wait_for_requests(server: &MockServer, count: usize) {
    let waiting = async {
        while requests(server).await < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let waited = tokio::time::timeout(PATIENCE, waiting).await;
    assert!(waited.is_ok(), "timed out waiting for {count} requests");
}

/// A free port on 127.0.0.1 that nothing listens on: a request to it is
/// refused, as one is offline.
fn closed_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    listener.local_addr().expect("the bound address")
}

/// Starts the daemon on a config file whose light style reads
/// `light_sources` and whose dark style shows `x.png`.
fn start(world: &World, general: &str, light_sources: &[&str]) -> Daemon {
    world.add_images(Style::Dark, &["x.png"]);
    world.save_config_file(&world.config_text_with_light_sources(general, light_sources));
    world.start_daemon_from_config_file(SEED)
}

async fn light_shows(daemon: &mut Daemon, item: &ItemId) -> Status {
    let wanted = Some(item.clone());
    daemon
        .status_where(&format!("light to show {item}"), |status| {
            status.current.light == wanted
        })
        .await
}

#[test]
fn a_remote_only_style_fills_its_slot_when_the_first_blob_lands() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        serve(&server, image("dawn").set_delay(Duration::from_millis(300))).await;
        let url = url_on(&server);
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);

        let at_ready = daemon.ready().await;
        let first_light = daemon
            .status_where("light to show an image", |status| {
                status.current.light.is_some()
            })
            .await;

        // READY did not wait for the download, and light did not borrow
        // dark's image while it waited.
        assert_eq!(at_ready.current.light, None);
        assert_eq!(first_light.current.light, Some(ItemId::from_url(&url)));
        assert_eq!(world.shown(Style::Light), "dawn");
        assert_eq!(
            world.gnome.key(Style::Light),
            Some(world.slot_uri(Style::Light))
        );
        daemon.terminate().await;
    });
}

#[test]
fn a_cached_blob_is_shown_at_start_while_the_server_is_down() {
    scenario(async {
        let world = World::new();
        let url = url_at(closed_address().port());
        world.cache_remote_image(&url, "cached", "\"v1\"").await;
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);

        let at_ready = daemon.ready().await;
        let shown_at_ready = world.shown(Style::Light);
        world
            .cached_meta_where(&url, "the failed fetch to be recorded", |meta| {
                meta.last_error.is_some()
            })
            .await;
        let_the_daemon_run().await;

        // The cached image was part of the first apply.
        assert_eq!(at_ready.current.light, Some(ItemId::from_url(&url)));
        assert_eq!(shown_at_ready, "cached");
        assert_eq!(world.shown(Style::Light), "cached");
        assert!(world.logged("remote image not fetched"));
        daemon.terminate().await;
    });
}

#[test]
fn a_revalidation_answered_304_changes_nothing_visible() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        serve_once_to_revalidation(&server, "\"v1\"", ResponseTemplate::new(304)).await;
        let url = url_on(&server);
        world.cache_remote_image(&url, "cached", "\"v1\"").await;
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);
        // Each key is written once its own image passes its check, in either
        // order, so the count of writes waits for both styles.
        let both_shown = PerStyle {
            light: Some(ItemId::from_url(&url)),
            dark: Some(world.item(Style::Dark, "x.png")),
        };
        daemon
            .status_where("both styles to show their image", |status| {
                status.current == both_shown
            })
            .await;
        let slot_inode = world.slot_inode(Style::Light);
        let key_writes = world.gnome.key_writes().len();

        world
            .cached_meta_where(&url, "the 304 to be recorded", |meta| {
                meta.fetched_at.is_some()
            })
            .await;
        let_the_daemon_run().await;

        assert_eq!(requests(&server).await, 1);
        assert_eq!(world.shown(Style::Light), "cached");
        assert_eq!(world.slot_inode(Style::Light), slot_inode);
        assert_eq!(world.gnome.key_writes().len(), key_writes);
        daemon.terminate().await;
    });
}

#[test]
fn a_new_version_of_the_image_on_screen_is_published_again() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        // At start the cached version is current; an hour later it is not.
        serve_once_to_revalidation(&server, "\"v1\"", ResponseTemplate::new(304)).await;
        serve(&server, image("v2").insert_header("ETag", "\"v2\"")).await;
        let url = url_on(&server);
        let old_blob = world.cache_remote_image(&url, "v1", "\"v1\"").await;
        let hourly = "order = \"alphabetical\"\nremote_refresh = \"1h\"";
        let mut daemon = start(&world, hourly, &[url.as_str()]);
        light_shows(&mut daemon, &ItemId::from_url(&url)).await;
        world
            .cached_meta_where(&url, "the 304 to be recorded", |meta| {
                meta.fetched_at.is_some()
            })
            .await;
        let shown_at_start = world.shown(Style::Light);
        let slot_inode = world.slot_inode(Style::Light);

        world.clock.advance(4 * LIGHT_INTERVAL);
        eventually("light to show the new version", || {
            world.shown(Style::Light) == "v2"
        })
        .await;

        assert_eq!(shown_at_start, "v1");
        assert_ne!(world.slot_inode(Style::Light), slot_inode);
        // A new version gets a new blob; the old one is not overwritten.
        assert_eq!(
            fs::read(&old_blob).expect("the old blob"),
            image_bytes("v1")
        );
        daemon.terminate().await;
    });
}

#[test]
fn a_404_keeps_the_stale_image_and_warns_once() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        serve(&server, ResponseTemplate::new(404)).await;
        let url = url_on(&server);
        world.cache_remote_image(&url, "stale", "\"v1\"").await;
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);
        light_shows(&mut daemon, &ItemId::from_url(&url)).await;

        world
            .cached_meta_where(&url, "the URL to be marked gone", |meta| meta.gone)
            .await;
        let_the_daemon_run().await;

        assert_eq!(world.shown(Style::Light), "stale");
        assert_eq!(world.times_logged("remote image is gone"), 1);
        daemon.terminate().await;
    });
}

#[test]
fn an_offline_start_fetches_again_when_connectivity_becomes_full() {
    scenario(async {
        let world = World::new();
        let address = closed_address();
        let url = url_at(address.port());
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);
        daemon.ready().await;
        world
            .cached_meta_where(&url, "the failed fetch to be recorded", |meta| {
                meta.last_error.is_some()
            })
            .await;
        let_the_daemon_run().await;
        let listener = TcpListener::bind(address).expect("bind the address again");
        let server = MockServer::builder().listener(listener).start().await;
        serve(&server, image("dawn")).await;

        // The backoff is 30 s or more, and this clock does not move: only
        // the connectivity change can bring the image.
        world.gnome.change_connectivity(Connectivity::Full);
        light_shows(&mut daemon, &ItemId::from_url(&url)).await;

        assert_eq!(world.shown(Style::Light), "dawn");
        assert_eq!(requests(&server).await, 1);
        daemon.terminate().await;
    });
}

#[test]
fn a_url_removed_by_a_config_reload_leaves_the_rotation() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        serve(&server, image("dawn")).await;
        let url = url_on(&server);
        world.add_images(Style::Light, &["a.png"]);
        let mut daemon = start(&world, ALPHABETICAL, &[url.as_str()]);
        light_shows(&mut daemon, &ItemId::from_url(&url)).await;

        let light_dir = world.images_dir(Style::Light);
        let light_dir = light_dir.to_str().expect("a UTF-8 path");
        world.save_config_file(&world.config_text_with_light_sources(ALPHABETICAL, &[light_dir]));
        // Light's first scan, dark's, and light's after the reload.
        eventually("the scan after the reload", || {
            world.times_logged("sources scanned") == 3
        })
        .await;
        let_the_daemon_run().await;
        // Past any deadline that light may have now.
        world.clock.advance(DARK_INTERVAL);
        let after = light_shows(&mut daemon, &world.item(Style::Light, "a.png")).await;

        // a.png is the only image left: nothing rotates back to the URL.
        assert_eq!(after.next_change, None);
        assert_eq!(world.shown(Style::Light), "a.png");
        assert_eq!(requests(&server).await, 1);
        daemon.terminate().await;
    });
}

#[test]
fn a_wiped_cache_is_fetched_again_and_no_slot_goes_blank() {
    scenario(async {
        let world = World::new();
        let server = MockServer::start().await;
        serve(&server, image("dawn")).await;
        let url = url_on(&server);
        world.add_images(Style::Light, &["a.png"]);
        let light_dir = world.images_dir(Style::Light);
        let light_dir = light_dir.to_str().expect("a UTF-8 path");
        // In this order, a.png comes before the URL.
        let mut daemon = start(&world, ALPHABETICAL, &[light_dir, url.as_str()]);
        let remote = ItemId::from_url(&url);
        let local = world.item(Style::Light, "a.png");
        light_shows(&mut daemon, &local).await;
        world
            .cached_meta_where(&url, "the first blob", |meta| meta.blob.is_some())
            .await;
        let_the_daemon_run().await;
        world.clock.advance(LIGHT_INTERVAL);
        light_shows(&mut daemon, &remote).await;
        let remote_slot_inode = world.slot_inode(Style::Light);

        fs::remove_dir_all(world.paths().remote_cache_dir).expect("wipe the cache");
        // The slot keeps its own copy of the data.
        assert_eq!(world.shown(Style::Light), "dawn");
        assert_eq!(world.slot_inode(Style::Light), remote_slot_inode);
        world.clock.advance(LIGHT_INTERVAL);
        light_shows(&mut daemon, &local).await;
        // The next turn finds the blob missing and fetches it again.
        world.clock.advance(LIGHT_INTERVAL);
        wait_for_requests(&server, 2).await;
        world
            .cached_meta_where(&url, "the blob to be back", |meta| meta.blob.is_some())
            .await;
        let shown_while_refetching = world.shown(Style::Light);
        world.clock.advance(LIGHT_INTERVAL);
        let back = daemon
            .status_where("light to show the URL again", |status| {
                status.current.light == Some(remote.clone())
            })
            .await;

        assert_eq!(shown_while_refetching, "a.png");
        assert_eq!(back.current.light, Some(remote));
        assert_eq!(world.shown(Style::Light), "dawn");
        daemon.terminate().await;
    });
}
