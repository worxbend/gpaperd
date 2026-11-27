//! The fetcher against `wiremock` on 127.0.0.1, and against a raw TCP server
//! for what `wiremock` cannot send: a stalled body and a chunked body with no
//! `Content-Length`. No request leaves the machine: every client is built
//! with `ProxyPolicy::Direct`. A literal loopback host is refused, so these
//! tests ask for `localhost` except the one that locks that refusal.
//!
//! `https` to `http` redirects need a TLS server, which these tests do not
//! have. The redirect policy's decision is unit-tested in `fetch.rs`; here a
//! redirect to a `file:` URL stands in for a refused scheme change.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpaperd_core::event::FetchOutcome;
use gpaperd_daemon::cache::Cache;
use gpaperd_daemon::fetch::{
    BACKOFF_CEILING, BACKOFF_FLOOR, FetchSettings, Fetcher, MAX_REDIRECTS, ProxyPolicy,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A grey PNG of one pixel.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\0\0\0\0\x3a\x7e\x9b\x55\
    \0\0\0\nIDATx\xdac`\0\0\0\x02\0\x01\xe5\x27\xde\xfc\0\0\0\0IEND\xaeB`\x82";
/// Magic bytes only: the fetcher checks no more.
const GIF: &[u8] = b"GIF89a\x01\0\x01\0\0\0\0;";
const HTML: &str = "<!DOCTYPE html><html><body>Please log in to the Wi-Fi</body></html>";

/// Body limit of the test client, so that oversize needs only a few KiB.
const TEST_MAX_BODY: u64 = 4096;

struct Harness {
    _dir: TempDir,
    root: PathBuf,
    cache: Arc<Cache>,
    fetcher: Fetcher,
}

impl Harness {
    fn new() -> Harness {
        Harness::with_read_timeout(Duration::from_secs(5))
    }

    fn with_read_timeout(read_timeout: Duration) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let cache = Arc::new(Cache::new(root.clone()));
        let settings = FetchSettings {
            connect_timeout: Duration::from_secs(5),
            read_timeout,
            total_timeout: Duration::from_secs(20),
            max_body_bytes: TEST_MAX_BODY,
            proxy: ProxyPolicy::Direct,
        };
        let fetcher = Fetcher::new(Arc::clone(&cache), settings, 7).unwrap();
        Harness {
            _dir: dir,
            root,
            cache,
            fetcher,
        }
    }

    async fn stored(&self, url: &Url) -> PathBuf {
        match self.fetcher.fetch(url).await {
            FetchOutcome::Stored { blob } => blob,
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    async fn failed(&self, url: &Url) -> Duration {
        match self.fetcher.fetch(url).await {
            FetchOutcome::Failed { retry_in } => retry_in,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    async fn unreachable(&self, url: &Url) -> Duration {
        match self.fetcher.fetch(url).await {
            FetchOutcome::Unreachable { retry_in } => retry_in,
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    /// Every temp file anywhere in the cache.
    fn temp_files(&self) -> Vec<PathBuf> {
        if !self.root.exists() {
            return Vec::new();
        }
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .map(|entry| entry.into_path())
            .collect()
    }

    fn entry_files(&self, url: &Url) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.cache.entry_dir(url))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn assert_no_temp_files(&self) {
        assert_eq!(self.temp_files(), Vec::<PathBuf>::new());
    }
}

fn url_on(server: &MockServer, route: &str) -> Url {
    let mut url = Url::parse(&format!("{}{route}", server.uri())).unwrap();
    // Literal loopback is refused before the GET. This name still reaches
    // the server, which is bound on 127.0.0.1.
    url.set_host(Some("localhost")).unwrap();
    url
}

fn image(body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_bytes(body.to_vec())
        .insert_header("Content-Type", "image/png")
}

async fn serve(server: &MockServer, route: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Serves `first` once, then `conditional` to requests that carry the
/// validator `name: value`.
async fn serve_then_revalidate(
    server: &MockServer,
    route: &str,
    first: ResponseTemplate,
    (name, value): (&'static str, &'static str),
    conditional: ResponseTemplate,
) {
    Mock::given(method("GET"))
        .and(path(route))
        // Not `matchers::header`: it splits values at commas, and an HTTP
        // date has one.
        .and(move |request: &Request| request.headers.get(name).is_some_and(|sent| sent == value))
        .respond_with(conditional)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(first)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

fn blake3_name(body: &[u8], extension: &str) -> String {
    format!("{}.{extension}", blake3::hash(body).to_hex())
}

fn assert_first_failure_wait(retry_in: Duration) {
    assert!(
        (BACKOFF_FLOOR..BACKOFF_FLOOR * 2).contains(&retry_in),
        "{retry_in:?}"
    );
}

/// Accepts one connection on 127.0.0.1, reads the request, writes
/// `response`, then holds the connection open for `hold`.
async fn raw_server(response: Vec<u8>, hold: Duration) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await;
        let _ = socket.write_all(&response).await;
        tokio::time::sleep(hold).await;
    });
    Url::parse(&format!("http://localhost:{port}/image")).unwrap()
}

fn chunk(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = format!("{:x}\r\n", bytes.len()).into_bytes();
    encoded.extend_from_slice(bytes);
    encoded.extend_from_slice(b"\r\n");
    encoded
}

#[tokio::test]
async fn image_200_is_stored_under_its_content_hash() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/a.png",
        image(PNG).insert_header("ETag", "\"v1\""),
    )
    .await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    let blob = harness.stored(&url).await;

    assert_eq!(
        blob,
        harness.cache.entry_dir(&url).join(blake3_name(PNG, "png"))
    );
    assert_eq!(fs::read(&blob).unwrap(), PNG);
    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.etag.as_deref(), Some("\"v1\""));
    assert_eq!(meta.mime.as_deref(), Some("image/png"));
    assert_eq!(meta.blob, Some(blake3_name(PNG, "png")));
    assert!(meta.fetched_at.is_some());
    assert_eq!(meta.last_error, None);
    assert_eq!(
        harness.entry_files(&url),
        [blake3_name(PNG, "png"), "meta.toml".into()]
    );
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn stored_image_is_in_the_loaded_cache() {
    let server = MockServer::start().await;
    serve(&server, "/a.png", image(PNG)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    let blob = harness.stored(&url).await;

    let loaded = harness.cache.load();
    assert_eq!(loaded.len(), 1);
    assert_eq!((&loaded[0].url, &loaded[0].blob), (&url, &blob));
}

#[tokio::test]
async fn etag_revalidation_304_keeps_the_blob_and_updates_fetched_at() {
    let server = MockServer::start().await;
    let first = image(PNG).insert_header("ETag", "\"v1\"");
    let validator = ("If-None-Match", "\"v1\"");
    serve_then_revalidate(
        &server,
        "/a.png",
        first,
        validator,
        ResponseTemplate::new(304),
    )
    .await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");
    let blob = harness.stored(&url).await;
    let before = harness.cache.meta(&url).await.unwrap().unwrap();

    let outcome = harness.fetcher.fetch(&url).await;

    assert_eq!(outcome, FetchOutcome::NotModified);
    let after = harness.cache.meta(&url).await.unwrap().unwrap();
    assert!(after.fetched_at > before.fetched_at);
    assert_eq!(after.blob, before.blob);
    assert_eq!(fs::read(blob).unwrap(), PNG);
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn lost_blob_is_fetched_with_a_plain_get() {
    let server = MockServer::start().await;
    let first = image(PNG).insert_header("ETag", "\"v1\"");
    let validator = ("If-None-Match", "\"v1\"");
    serve_then_revalidate(
        &server,
        "/a.png",
        first,
        validator,
        ResponseTemplate::new(304),
    )
    .await;
    serve(&server, "/a.png", image(PNG)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");
    let blob = harness.stored(&url).await;
    fs::remove_file(&blob).unwrap();

    let again = harness.stored(&url).await;

    assert_eq!(again, blob);
    assert_eq!(fs::read(&again).unwrap(), PNG);
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn last_modified_revalidation_304_keeps_the_blob() {
    let server = MockServer::start().await;
    let date = "Sun, 06 Nov 1994 08:49:37 GMT";
    let first = image(PNG).insert_header("Last-Modified", date);
    serve_then_revalidate(
        &server,
        "/a.png",
        first,
        ("If-Modified-Since", date),
        ResponseTemplate::new(304),
    )
    .await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");
    harness.stored(&url).await;

    assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::NotModified);
    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.last_modified.as_deref(), Some(date));
    assert_eq!(meta.blob, Some(blake3_name(PNG, "png")));
}

#[tokio::test]
async fn new_version_gets_a_new_blob_and_the_old_one_stays() {
    let server = MockServer::start().await;
    let first = image(PNG).insert_header("ETag", "\"v1\"");
    let second = image(GIF).insert_header("ETag", "\"v2\"");
    serve_then_revalidate(&server, "/a", first, ("If-None-Match", "\"v1\""), second).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a");
    let old = harness.stored(&url).await;

    let new = harness.stored(&url).await;

    assert_ne!(new, old);
    assert_eq!(fs::read(&old).unwrap(), PNG);
    assert_eq!(fs::read(&new).unwrap(), GIF);
    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, Some(blake3_name(GIF, "gif")));
    assert_eq!(meta.etag.as_deref(), Some("\"v2\""));
    assert_eq!(meta.mime.as_deref(), Some("image/gif"));
}

#[tokio::test]
async fn a_third_version_deletes_all_but_the_two_latest_blobs() {
    let server = MockServer::start().await;
    let third = [PNG, b"!"].concat();
    for body in [PNG, GIF, &third] {
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(image(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    let harness = Harness::new();
    let url = url_on(&server, "/a");
    let first = harness.stored(&url).await;
    let second = harness.stored(&url).await;

    let latest = harness.stored(&url).await;

    assert!(!first.exists(), "{first:?}");
    let mut expected = vec![
        blake3_name(GIF, "gif"),
        blake3_name(&third, "png"),
        "meta.toml".to_owned(),
    ];
    expected.sort();
    assert_eq!(harness.entry_files(&url), expected);
    assert_eq!(fs::read(&second).unwrap(), GIF);
    assert_eq!(fs::read(&latest).unwrap(), third);
}

#[tokio::test]
async fn status_404_and_410_mark_gone_and_keep_the_stale_blob() {
    for status in [404, 410] {
        let server = MockServer::start().await;
        let harness = Harness::new();
        let url = url_on(&server, "/a.png");
        Mock::given(method("GET"))
            .respond_with(image(PNG))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        serve(&server, "/a.png", ResponseTemplate::new(status)).await;
        let blob = harness.stored(&url).await;

        assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::Gone);
        assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::Gone);

        let meta = harness.cache.meta(&url).await.unwrap().unwrap();
        assert!(meta.gone, "{status}");
        assert_eq!(meta.blob, Some(blake3_name(PNG, "png")));
        assert!(blob.is_file());
        assert_eq!(harness.cache.load().len(), 1);
    }
}

#[tokio::test]
async fn gone_without_a_cached_copy_records_it() {
    let server = MockServer::start().await;
    serve(&server, "/a.png", ResponseTemplate::new(404)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::Gone);

    assert!(harness.cache.meta(&url).await.unwrap().unwrap().gone);
    assert!(harness.cache.load().is_empty());
}

#[tokio::test]
async fn status_429_with_retry_after_seconds_waits_that_long() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/a.png",
        ResponseTemplate::new(429).insert_header("Retry-After", "600"),
    )
    .await;
    let harness = Harness::new();

    let retry_in = harness.failed(&url_on(&server, "/a.png")).await;

    assert_eq!(retry_in, Duration::from_secs(600));
}

#[tokio::test]
async fn status_429_with_retry_after_date_is_capped_at_the_ceiling() {
    let server = MockServer::start().await;
    let far =
        ResponseTemplate::new(429).insert_header("Retry-After", "Fri, 31 Dec 2100 23:59:59 GMT");
    serve(&server, "/far", far).await;
    let past =
        ResponseTemplate::new(429).insert_header("Retry-After", "Sun, 06 Nov 1994 08:49:37 GMT");
    serve(&server, "/past", past).await;
    let harness = Harness::new();

    assert_eq!(
        harness.failed(&url_on(&server, "/far")).await,
        BACKOFF_CEILING
    );
    assert_first_failure_wait(harness.failed(&url_on(&server, "/past")).await);
}

#[tokio::test]
async fn status_500_backs_off_exponentially_and_records_the_error() {
    let server = MockServer::start().await;
    serve(&server, "/a.png", ResponseTemplate::new(500)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    let first = harness.failed(&url).await;
    let second = harness.failed(&url).await;

    assert_first_failure_wait(first);
    assert!(
        (BACKOFF_FLOOR * 2..BACKOFF_FLOOR * 4).contains(&second),
        "{second:?}"
    );
    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(
        meta.last_error.as_deref(),
        Some("HTTP 500 Internal Server Error")
    );
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn success_and_forget_reset_the_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.png"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/a.png"))
        .respond_with(image(PNG))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    serve(&server, "/a.png", ResponseTemplate::new(503)).await;
    serve(&server, "/b.png", ResponseTemplate::new(503)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");
    let other = url_on(&server, "/b.png");
    harness.failed(&url).await;
    harness.failed(&url).await;

    harness.stored(&url).await;

    assert_eq!(
        harness.cache.meta(&url).await.unwrap().unwrap().last_error,
        None
    );
    assert_first_failure_wait(harness.failed(&url).await);
    harness.failed(&other).await;
    harness.fetcher.forget(&other);
    assert_first_failure_wait(harness.failed(&other).await);
}

#[tokio::test]
async fn html_body_is_rejected_and_the_old_blob_kept() {
    let server = MockServer::start().await;
    let portal = ResponseTemplate::new(200)
        .set_body_string(HTML)
        .insert_header("Content-Type", "text/html");
    serve_then_revalidate(
        &server,
        "/a.png",
        image(PNG).insert_header("ETag", "\"v1\""),
        ("If-None-Match", "\"v1\""),
        portal,
    )
    .await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");
    let blob = harness.stored(&url).await;

    assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::NotModified);

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, Some(blake3_name(PNG, "png")));
    assert_eq!(meta.last_error.as_deref(), Some("body is not an image"));
    assert_eq!(
        harness.entry_files(&url),
        [blake3_name(PNG, "png"), "meta.toml".into()]
    );
    assert_eq!(fs::read(blob).unwrap(), PNG);
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn empty_200_is_not_an_image() {
    let server = MockServer::start().await;
    serve(&server, "/a.png", ResponseTemplate::new(200)).await;
    let harness = Harness::new();

    assert_eq!(
        harness.fetcher.fetch(&url_on(&server, "/a.png")).await,
        FetchOutcome::NotModified
    );
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn oversized_content_length_is_rejected() {
    let server = MockServer::start().await;
    let mut big = PNG.to_vec();
    big.resize(TEST_MAX_BODY as usize + 1, 0);
    serve(&server, "/big.png", image(&big)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/big.png");

    assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::NotModified);

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, None);
    assert_eq!(
        meta.last_error,
        Some(format!("body is larger than {TEST_MAX_BODY} bytes"))
    );
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn body_of_exactly_the_limit_is_accepted() {
    let server = MockServer::start().await;
    let mut big = PNG.to_vec();
    big.resize(TEST_MAX_BODY as usize, 0);
    serve(&server, "/big.png", image(&big)).await;
    let harness = Harness::new();

    let blob = harness.stored(&url_on(&server, "/big.png")).await;

    assert_eq!(fs::metadata(blob).unwrap().len(), TEST_MAX_BODY);
}

#[tokio::test]
async fn oversized_chunked_body_without_content_length_is_rejected() {
    let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    response.extend(chunk(PNG));
    for _ in 0..(TEST_MAX_BODY / 512 + 1) {
        response.extend(chunk(&[0; 512]));
    }
    response.extend_from_slice(b"0\r\n\r\n");
    let url = raw_server(response, Duration::from_secs(1)).await;
    let harness = Harness::new();

    assert_eq!(harness.fetcher.fetch(&url).await, FetchOutcome::NotModified);

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, None);
    assert_eq!(
        meta.last_error,
        Some(format!("body is larger than {TEST_MAX_BODY} bytes"))
    );
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn stalled_body_hits_the_idle_read_timeout() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n".to_vec();
    response.extend_from_slice(PNG);
    let url = raw_server(response, Duration::from_secs(30)).await;
    let harness = Harness::with_read_timeout(Duration::from_millis(200));
    let started = Instant::now();

    assert_first_failure_wait(harness.unreachable(&url).await);

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(harness.cache.meta(&url).await.unwrap().unwrap().blob, None);
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn slow_headers_hit_the_idle_read_timeout() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/slow.png",
        image(PNG).set_delay(Duration::from_secs(5)),
    )
    .await;
    let harness = Harness::with_read_timeout(Duration::from_millis(200));
    let started = Instant::now();

    harness.unreachable(&url_on(&server, "/slow.png")).await;

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    harness.assert_no_temp_files();
}

/// `/r0` redirects to `/r1` and so on; `/r{hops}` serves the image.
/// `localhost`, not the literal `127.0.0.1` in `server.uri()`. A literal
/// loopback redirect is refused; this name is how a chain can still be followed.
fn localhost_url(server: &MockServer, route: &str) -> String {
    let base = Url::parse(&server.uri()).unwrap();
    format!(
        "http://localhost:{}{route}",
        base.port().expect("wiremock uri has a port")
    )
}

async fn serve_redirect_chain(server: &MockServer, hops: usize) {
    for hop in 0..hops {
        let next = localhost_url(server, &format!("/r{}", hop + 1));
        let redirect = ResponseTemplate::new(302).insert_header("Location", next.as_str());
        serve(server, &format!("/r{hop}"), redirect).await;
    }
    serve(server, &format!("/r{hops}"), image(PNG)).await;
}

#[tokio::test]
async fn up_to_the_redirect_limit_is_followed() {
    let server = MockServer::start().await;
    serve_redirect_chain(&server, MAX_REDIRECTS).await;
    let harness = Harness::new();

    harness.stored(&url_on(&server, "/r0")).await;
}

#[tokio::test]
async fn one_redirect_too_many_is_rejected() {
    let server = MockServer::start().await;
    serve_redirect_chain(&server, MAX_REDIRECTS + 1).await;
    let harness = Harness::new();
    let url = url_on(&server, "/r0");

    harness.failed(&url).await;

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert!(meta.last_error.unwrap().contains("too many redirects"));
}

#[tokio::test]
async fn redirect_to_a_non_http_scheme_is_rejected() {
    let server = MockServer::start().await;
    let to_file = ResponseTemplate::new(302).insert_header("Location", "file:///etc/passwd");
    serve(&server, "/a.png", to_file).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    harness.failed(&url).await;

    let meta = harness.cache.meta(&url).await.unwrap().unwrap();
    assert_eq!(meta.blob, None);
}

#[tokio::test]
async fn a_redirect_to_another_host_carries_no_referer() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    let next = localhost_url(&target, "/x.png");
    let redirect = ResponseTemplate::new(302).insert_header("Location", next.as_str());
    serve(&origin, "/a", redirect).await;
    serve(&target, "/x.png", image(PNG)).await;
    let harness = Harness::new();

    harness.stored(&url_on(&origin, "/a?token=hunter2")).await;

    let requests = target.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers.get("referer"), None);
}

#[tokio::test]
async fn logged_error_never_names_the_url() {
    let server = MockServer::start().await;
    let harness = Harness::new();
    let secret = format!("{}?token=hunter2", localhost_url(&server, "/r0"));
    let to_file = ResponseTemplate::new(302).insert_header("Location", "file:///x?token=hunter2");
    serve(&server, "/r0", to_file).await;
    let url = Url::parse(&secret).unwrap();

    harness.failed(&url).await;

    let error = harness
        .cache
        .meta(&url)
        .await
        .unwrap()
        .unwrap()
        .last_error
        .unwrap();
    assert!(!error.contains("hunter2"), "{error}");
    assert!(!error.contains("127.0.0.1"), "{error}");
}

#[tokio::test]
async fn refused_connection_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let harness = Harness::new();

    assert_first_failure_wait(
        harness
            .unreachable(&Url::parse(&format!("http://localhost:{port}/a.png")).unwrap())
            .await,
    );
}

#[tokio::test]
async fn non_http_url_is_not_fetched() {
    let harness = Harness::new();

    let retry_in = harness
        .failed(&Url::parse("file:///etc/passwd").unwrap())
        .await;

    assert_eq!(retry_in, BACKOFF_CEILING);
    assert!(!harness.root.exists());
}

#[tokio::test]
async fn two_concurrent_fetches_of_one_url_leave_one_entry() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/a.png",
        image(PNG).set_delay(Duration::from_millis(100)),
    )
    .await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    let (first, second) = tokio::join!(harness.fetcher.fetch(&url), harness.fetcher.fetch(&url));

    let blob = harness.cache.entry_dir(&url).join(blake3_name(PNG, "png"));
    assert_eq!(first, FetchOutcome::Stored { blob: blob.clone() });
    assert_eq!(second, FetchOutcome::Stored { blob: blob.clone() });
    assert_eq!(fs::read(&blob).unwrap(), PNG);
    assert_eq!(
        harness.cache.meta(&url).await.unwrap().unwrap().blob,
        Some(blake3_name(PNG, "png"))
    );
    assert_eq!(
        harness.entry_files(&url),
        [blake3_name(PNG, "png"), "meta.toml".into()]
    );
}

#[tokio::test]
async fn cancelled_fetch_leaves_no_temp_file() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n".to_vec();
    response.extend_from_slice(PNG);
    let url = raw_server(response, Duration::from_secs(30)).await;
    let harness = Harness::new();

    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), harness.fetcher.fetch(&url)).await;

    assert!(cancelled.is_err());
    harness.assert_no_temp_files();
}

#[tokio::test]
async fn unwritable_cache_is_a_failure_that_keeps_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let server = MockServer::start().await;
    serve(&server, "/a.png", image(PNG)).await;
    let harness = Harness::new();
    fs::create_dir_all(&harness.root).unwrap();
    fs::set_permissions(&harness.root, fs::Permissions::from_mode(0o500)).unwrap();
    let url = url_on(&server, "/a.png");

    let retry_in = harness.failed(&url).await;

    fs::set_permissions(&harness.root, fs::Permissions::from_mode(0o700)).unwrap();
    assert_first_failure_wait(retry_in);
    assert!(fs::read_dir(&harness.root).unwrap().next().is_none());
}

#[tokio::test]
async fn status_401_and_403_keep_the_blob_and_do_not_retry() {
    for status in [401, 403] {
        let server = MockServer::start().await;
        let harness = Harness::new();
        let url = url_on(&server, "/a.png");
        Mock::given(method("GET"))
            .respond_with(image(PNG))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        serve(&server, "/a.png", ResponseTemplate::new(status)).await;
        let blob = harness.stored(&url).await;

        assert_eq!(
            harness.fetcher.fetch(&url).await,
            FetchOutcome::NotModified,
            "{status}"
        );

        let meta = harness.cache.meta(&url).await.unwrap().unwrap();
        assert!(!meta.gone, "{status}");
        assert_eq!(meta.blob, Some(blake3_name(PNG, "png")), "{status}");
        assert!(
            meta.last_error.unwrap().contains(&status.to_string()),
            "{status}"
        );
        assert_eq!(fs::read(&blob).unwrap(), PNG, "{status}");
    }
}

#[tokio::test]
async fn redirect_to_a_literal_loopback_address_is_refused() {
    let server = MockServer::start().await;
    let next = format!("{}/x.png", server.uri());
    let redirect = ResponseTemplate::new(302).insert_header("Location", next.as_str());
    serve(&server, "/a.png", redirect).await;
    serve(&server, "/x.png", image(PNG)).await;
    let harness = Harness::new();
    let url = url_on(&server, "/a.png");

    harness.failed(&url).await;

    let error = harness
        .cache
        .meta(&url)
        .await
        .unwrap()
        .unwrap()
        .last_error
        .unwrap();
    assert!(
        error.contains("loopback, link-local, or private"),
        "{error}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn configured_literal_reserved_address_is_not_fetched() {
    let server = MockServer::start().await;
    serve(&server, "/a.png", image(PNG)).await;
    let harness = Harness::new();
    let literal = Url::parse(&format!("{}/a.png", server.uri())).unwrap();

    harness.failed(&literal).await;

    let error = harness
        .cache
        .meta(&literal)
        .await
        .unwrap()
        .unwrap()
        .last_error
        .unwrap();
    assert!(
        error.contains("loopback, link-local, or private"),
        "{error}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
    for target in [
        "http://10.1.2.3/x",
        "http://172.16.0.1/x",
        "http://192.168.0.1/x",
        "http://169.254.1.1/x",
        "http://[::1]/x",
        "http://[fe80::1]/x",
        "http://[fc00::1]/x",
    ] {
        let url = Url::parse(target).unwrap();
        harness.failed(&url).await;
        let error = harness
            .cache
            .meta(&url)
            .await
            .unwrap()
            .unwrap()
            .last_error
            .unwrap();
        assert!(
            error.contains("loopback, link-local, or private"),
            "{target}: {error}"
        );
    }
}

#[tokio::test]
async fn unreadable_meta_is_not_replaced_and_the_blob_stays() {
    for status in [500, 404] {
        let server = MockServer::start().await;
        let harness = Harness::new();
        let url = url_on(&server, "/a.png");
        Mock::given(method("GET"))
            .and(path("/a.png"))
            .respond_with(image(PNG))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        serve(&server, "/a.png", ResponseTemplate::new(status)).await;
        let blob = harness.stored(&url).await;
        let meta_path = harness.cache.entry_dir(&url).join("meta.toml");
        fs::write(&meta_path, b"not toml").unwrap();

        let outcome = harness.fetcher.fetch(&url).await;

        assert!(
            matches!(outcome, FetchOutcome::Failed { .. } | FetchOutcome::Gone),
            "{status}: {outcome:?}"
        );
        assert_eq!(fs::read(&meta_path).unwrap(), b"not toml", "{status}");
        assert_eq!(fs::read(&blob).unwrap(), PNG, "{status}");
        // shortcut: InvalidData collection still deletes the blob in cache.rs,
        // so this test does not call collect_garbage. Upgrade when that path keeps it.
    }
}
