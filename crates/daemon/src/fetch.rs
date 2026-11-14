//! Downloads remote images into the [`Cache`].
//!
//! One [`Fetcher`] per run. It owns the HTTP client, caps the fetches that
//! run at once, and keeps each URL's count of consecutive failures for the
//! backoff. The core decides when a URL is fetched; the fetcher answers each
//! [`Effect::Fetch`](gpaperd_core::event::Effect::Fetch) with one
//! [`FetchOutcome`].
//!
//! Response handling, row by row of the plan's table:
//!
//! | Response or condition | Outcome |
//! | --- | --- |
//! | `200` with image bytes | streamed into a [`BlobDraft`], published: [`FetchOutcome::Stored`] |
//! | `304 Not Modified` | `fetched_at` updated: [`FetchOutcome::NotModified`] |
//! | `404` or `410` | `gone` set, warned once: [`FetchOutcome::Gone`] |
//! | `429`, `5xx` | [`FetchOutcome::Failed`], `Retry-After` honoured |
//! | network error, timeout | [`FetchOutcome::Unreachable`] |
//! | `200` that is not an image, by magic bytes | rejected, old blob kept: `Failed` |
//! | `Content-Length` or body over [`MAX_BODY_BYTES`] | rejected: `Failed` |
//! | more than [`MAX_REDIRECTS`] redirects, or `https` to `http` | rejected: `Failed` |
//! | `ENOSPC` while writing | temp file deleted, old blob kept: `Failed` |
//!
//! Only `http` and `https` are fetched. Every log line names the URL by
//! [`redacted`], never as it is.

use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpaperd_core::catalog::ItemId;
use gpaperd_core::event::FetchOutcome;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use reqwest::header::{
    ETAG, HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, RETRY_AFTER,
};
use reqwest::{Response, StatusCode, redirect};
use tokio::sync::Semaphore;
use url::{Position, Url};

use crate::cache::{BlobDraft, Cache, Meta};
use crate::probe::{self, HEADER_LEN, ImageFormat};

/// Longest wait for the TCP and TLS handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for the next bytes of a response: stops slow-loris servers.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest time for one whole fetch, body included.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest body accepted, by `Content-Length` and by the bytes that arrive.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// Redirects followed per fetch. One more fails it.
pub const MAX_REDIRECTS: usize = 5;
/// Fetches that run at once. The rest wait for a turn.
pub const MAX_CONCURRENT_FETCHES: usize = 4;
/// Shortest wait after a failed fetch.
pub const BACKOFF_FLOOR: Duration = Duration::from_secs(30);
/// Longest wait after a failed fetch, `Retry-After` included.
pub const BACKOFF_CEILING: Duration = Duration::from_secs(60 * 60);

/// How the HTTP client is built. [`FetchSettings::default`] is the daemon's;
/// tests shorten the limits and go direct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchSettings {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub total_timeout: Duration,
    pub max_body_bytes: u64,
    pub proxy: ProxyPolicy,
}

impl Default for FetchSettings {
    fn default() -> FetchSettings {
        FetchSettings {
            connect_timeout: CONNECT_TIMEOUT,
            read_timeout: READ_TIMEOUT,
            total_timeout: TOTAL_TIMEOUT,
            max_body_bytes: MAX_BODY_BYTES,
            proxy: ProxyPolicy::FromEnvironment,
        }
    }
}

/// Where requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyPolicy {
    /// Through the proxy that `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`
    /// name, as reqwest reads them by default. GNOME's proxy settings are
    /// not read.
    FromEnvironment,
    /// Straight to the server, whatever the environment says. For tests.
    Direct,
}

/// Doublings of [`BACKOFF_FLOOR`] after which the step is past
/// [`BACKOFF_CEILING`]: 30 s × 2⁷ > 1 h.
const MAX_DOUBLINGS: u32 = 7;

/// The daemon's HTTP side of the remote cache. `Send + Sync`: the
/// interpreter shares it with each fetch task through an `Arc`.
#[derive(Debug)]
pub struct Fetcher {
    cache: Arc<Cache>,
    client: reqwest::Client,
    max_body_bytes: u64,
    turns: Semaphore,
    /// Consecutive failures per URL. A URL with none is not in it.
    failures: Mutex<HashMap<Url, u32>>,
    jitter: Mutex<SmallRng>,
}

/// How a fetch that worked ended.
enum Answer {
    Stored(PathBuf),
    NotModified,
    Gone(StatusCode),
}

/// Why a fetch failed. The text goes to the log and to `last_error`, so it
/// never holds a URL.
#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("HTTP {status}")]
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    /// The server was not reached, or stopped answering.
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Redirect(String),
    #[error("body is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("body is not an image")]
    NotAnImage,
    #[error("cache write failed: {0}")]
    Cache(#[from] io::Error),
}

impl FetchError {
    /// `reqwest`'s own text names the URL; this one does not. A refused
    /// redirect reached a server, so it is not a network error.
    fn network(error: reqwest::Error) -> FetchError {
        let is_redirect = error.is_redirect();
        let error = error.without_url();
        let mut text = error.to_string();
        let mut cause = error.source();
        while let Some(inner) = cause {
            text.push_str(": ");
            text.push_str(&inner.to_string());
            cause = inner.source();
        }
        if is_redirect {
            FetchError::Redirect(text)
        } else {
            FetchError::Network(text)
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            FetchError::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl Fetcher {
    /// Builds the HTTP client from `settings`: the timeouts, at most
    /// [`MAX_REDIRECTS`] redirects and none from `https` to `http`, and the
    /// proxy policy. `seed` seeds the backoff jitter.
    ///
    /// # Errors
    ///
    /// The client cannot be built: the TLS backend failed to start.
    pub fn new(
        cache: Arc<Cache>,
        settings: FetchSettings,
        seed: u64,
    ) -> Result<Fetcher, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("gpaperd/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .timeout(settings.total_timeout)
            .redirect(redirect_policy())
            // A Referer would hand the URL's query, often a token, to the
            // host that a redirect leads to.
            .referer(false);
        if settings.proxy == ProxyPolicy::Direct {
            builder = builder.no_proxy();
        }
        Ok(Fetcher {
            cache,
            client: builder.build()?,
            max_body_bytes: settings.max_body_bytes,
            turns: Semaphore::new(MAX_CONCURRENT_FETCHES),
            failures: Mutex::default(),
            jitter: Mutex::new(SmallRng::seed_from_u64(seed)),
        })
    }

    /// Fetches `url` into the cache, as the module's table says, and logs
    /// the outcome. A conditional `GET` when the cache holds a version:
    /// `If-None-Match` and `If-Modified-Since` from its `meta.toml`.
    ///
    /// Waits for one of [`MAX_CONCURRENT_FETCHES`] turns first. A success
    /// resets `url`'s failure count; a failure raises it and comes with the
    /// wait that [`backoff`] gives for the new count. Writes `last_error`
    /// to `meta.toml` on a failure and clears it on a success.
    ///
    /// A URL that is not `http` or `https` is not requested: `Failed` with
    /// [`BACKOFF_CEILING`].
    ///
    /// Cancel-safe: dropping the future deletes a draft in progress and
    /// leaves the cache as it was. The future is `Send`.
    pub async fn fetch(&self, url: &Url) -> FetchOutcome {
        if !matches!(url.scheme(), "http" | "https") {
            tracing::warn!(url = %redacted(url), "not fetched: only http and https are");
            return FetchOutcome::Failed {
                retry_in: BACKOFF_CEILING,
            };
        }
        let _turn = self
            .turns
            .acquire()
            .await
            .expect("the fetch semaphore is never closed");
        let known = self.known_meta(url).await;
        let answer = self.download(url, known.as_ref()).await;
        self.conclude(url, known, answer).await
    }

    /// Forgets `url`'s failure count, because no style lists it any more.
    pub fn forget(&self, url: &Url) {
        self.failure_counts().remove(url);
    }

    /// The cached `meta.toml` of `url`. One that cannot be read is replaced
    /// by the next write, so the fetch goes on without it.
    async fn known_meta(&self, url: &Url) -> Option<Meta> {
        let meta = self.cache.meta(url).await.unwrap_or_else(|error| {
            tracing::warn!(url = %redacted(url), reason = %error, "cached metadata ignored");
            None
        })?;
        Some(self.without_lost_blob(url, meta).await)
    }

    /// `meta` without its cached version when the blob it names is gone,
    /// for example when the cache directory was wiped. A conditional `GET`
    /// could then get a `304` for bytes that are no longer on disk.
    async fn without_lost_blob(&self, url: &Url, meta: Meta) -> Meta {
        let Some(name) = &meta.blob else {
            return meta;
        };
        let blob = self.cache.entry_dir(url).join(name);
        // An error is treated as present: the probe of that blob tells.
        if tokio::fs::try_exists(&blob).await.unwrap_or(true) {
            return meta;
        }
        Meta {
            etag: None,
            last_modified: None,
            blob: None,
            mime: None,
            ..meta
        }
    }

    async fn download(&self, url: &Url, known: Option<&Meta>) -> Result<Answer, FetchError> {
        let has_blob = known.is_some_and(|meta| meta.blob.is_some());
        let mut request = self.client.get(url.clone());
        if let Some(meta) = known.filter(|_| has_blob) {
            request = request.headers(conditional_headers(meta));
        }
        let response = request.send().await.map_err(FetchError::network)?;
        match response.status() {
            StatusCode::OK => self.store(url, known, response).await.map(Answer::Stored),
            StatusCode::NOT_MODIFIED if has_blob => Ok(Answer::NotModified),
            status @ (StatusCode::NOT_FOUND | StatusCode::GONE) => Ok(Answer::Gone(status)),
            status => Err(FetchError::Status {
                status,
                retry_after: retry_after(&response),
            }),
        }
    }

    /// Streams a `200`'s body into a draft and publishes it.
    async fn store(
        &self,
        url: &Url,
        known: Option<&Meta>,
        mut response: Response,
    ) -> Result<PathBuf, FetchError> {
        let limit = self.max_body_bytes;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(FetchError::TooLarge { limit });
        }
        let base = meta_or_new(known.cloned(), url);
        let meta = Meta {
            etag: header_text(&response, ETAG),
            last_modified: header_text(&response, LAST_MODIFIED),
            ..base
        };
        let mut draft = self.cache.draft(url).await?;
        let format = self.stream_body(&mut response, &mut draft).await?;
        let meta = Meta {
            mime: Some(format.mime.to_owned()),
            fetched_at: Some(SystemTime::now()),
            last_error: None,
            gone: false,
            ..meta
        };
        Ok(self
            .cache
            .publish(url, draft, format.extension, meta)
            .await?)
    }

    /// Appends the body to `draft`, at most `max_body_bytes` of it, and
    /// tells its image format. A body whose start is no image stops as soon
    /// as that is clear.
    async fn stream_body(
        &self,
        response: &mut Response,
        draft: &mut BlobDraft,
    ) -> Result<&'static ImageFormat, FetchError> {
        let limit = self.max_body_bytes;
        let mut head = Vec::with_capacity(HEADER_LEN);
        let mut format = None;
        while let Some(chunk) = response.chunk().await.map_err(FetchError::network)? {
            if draft.len() + chunk.len() as u64 > limit {
                return Err(FetchError::TooLarge { limit });
            }
            if format.is_none() {
                let wanted = (HEADER_LEN - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..wanted]);
                if head.len() == HEADER_LEN {
                    format = Some(sniff(&head)?);
                }
            }
            draft.append(&chunk).await?;
        }
        format.map_or_else(|| sniff(&head), Ok)
    }

    /// Turns the answer into the outcome: failure count, `meta.toml`, log.
    async fn conclude(
        &self,
        url: &Url,
        known: Option<Meta>,
        answer: Result<Answer, FetchError>,
    ) -> FetchOutcome {
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => return self.failed(url, known, error).await,
        };
        self.forget(url);
        match answer {
            Answer::Stored(blob) => {
                tracing::info!(url = %redacted(url), blob = %blob.display(), "remote image stored");
                FetchOutcome::Stored { blob }
            }
            Answer::NotModified => {
                tracing::debug!(url = %redacted(url), "remote image not modified");
                let mut meta = meta_or_new(known, url);
                meta.fetched_at = Some(SystemTime::now());
                meta.last_error = None;
                meta.gone = false;
                self.save_meta(url, &meta).await;
                FetchOutcome::NotModified
            }
            Answer::Gone(status) => {
                let mut meta = meta_or_new(known, url);
                if !meta.gone {
                    tracing::warn!(url = %redacted(url), %status, "remote image is gone: the cached copy stays");
                }
                meta.gone = true;
                meta.last_error = Some(format!("HTTP {status}"));
                self.save_meta(url, &meta).await;
                FetchOutcome::Gone
            }
        }
    }

    async fn failed(&self, url: &Url, known: Option<Meta>, error: FetchError) -> FetchOutcome {
        let failures = self.count_failure(url);
        let retry_in = backoff(failures, error.retry_after(), self.next_jitter());
        let retry_secs = retry_in.as_secs();
        if failures == 1 {
            tracing::warn!(url = %redacted(url), reason = %error, retry_secs, "remote image not fetched");
        } else {
            tracing::debug!(url = %redacted(url), reason = %error, failures, retry_secs, "remote image not fetched");
        }
        let mut meta = meta_or_new(known, url);
        meta.last_error = Some(error.to_string());
        self.save_meta(url, &meta).await;
        if matches!(error, FetchError::Network(_)) {
            FetchOutcome::Unreachable { retry_in }
        } else {
            FetchOutcome::Failed { retry_in }
        }
    }

    /// Writes `meta`. Only the record of the fetch is lost when that fails.
    async fn save_meta(&self, url: &Url, meta: &Meta) {
        if let Err(error) = self.cache.record(url, meta).await {
            tracing::warn!(url = %redacted(url), reason = %error, "cached metadata not written");
        }
    }

    /// Raises `url`'s failure count and returns the new count.
    fn count_failure(&self, url: &Url) -> u32 {
        let mut counts = self.failure_counts();
        let count = counts.entry(url.clone()).or_default();
        *count = count.saturating_add(1);
        *count
    }

    fn failure_counts(&self) -> std::sync::MutexGuard<'_, HashMap<Url, u32>> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_jitter(&self) -> f64 {
        self.jitter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .random()
    }
}

/// The cached metadata, or a fresh one for a URL the cache does not know.
fn meta_or_new(known: Option<Meta>, url: &Url) -> Meta {
    known.unwrap_or_else(|| Meta::new(url.clone()))
}

/// At most [`MAX_REDIRECTS`], none to a scheme other than `http` or
/// `https`, and none to `http` once the chain was on `https`.
fn redirect_policy() -> redirect::Policy {
    let limited = redirect::Policy::limited(MAX_REDIRECTS);
    redirect::Policy::custom(move |attempt| {
        match redirect_refusal(attempt.previous(), attempt.url()) {
            Some(reason) => attempt.error(reason),
            None => limited.redirect(attempt),
        }
    })
}

/// Why a redirect to `next` after the chain `previous` is refused.
fn redirect_refusal(previous: &[Url], next: &Url) -> Option<&'static str> {
    match next.scheme() {
        "https" => None,
        "http" if previous.iter().any(|url| url.scheme() == "https") => {
            Some("redirect from https to http refused")
        }
        "http" => None,
        _ => Some("redirect to a scheme other than http or https refused"),
    }
}

/// `If-None-Match` and `If-Modified-Since` from the cached version. A value
/// that is no valid header (the file is the user's) is left out.
fn conditional_headers(meta: &Meta) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let validators = [
        (IF_NONE_MATCH, &meta.etag),
        (IF_MODIFIED_SINCE, &meta.last_modified),
    ];
    for (name, value) in validators {
        if let Some(value) = value.as_deref().and_then(|v| HeaderValue::from_str(v).ok()) {
            headers.insert(name, value);
        }
    }
    headers
}

fn header_text(response: &Response, name: reqwest::header::HeaderName) -> Option<String> {
    let value = response.headers().get(name)?.to_str().ok()?;
    Some(value.to_owned())
}

fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, SystemTime::now())
}

fn sniff(head: &[u8]) -> Result<&'static ImageFormat, FetchError> {
    probe::image_format(head).ok_or(FetchError::NotAnImage)
}

/// How long to wait after the `failures`-th failure in a row (from 1).
///
/// The step doubles from [`BACKOFF_FLOOR`] with each failure, and `jitter`,
/// a uniform sample from `[0, 1)`, spreads it up to the next step:
/// `step × (1 + jitter)`. The result is never shorter than `retry_after`
/// and never outside [`BACKOFF_FLOOR`]`..=`[`BACKOFF_CEILING`]; the ceiling
/// wins over a longer `retry_after`.
pub fn backoff(failures: u32, retry_after: Option<Duration>, jitter: f64) -> Duration {
    let doublings = failures.saturating_sub(1).min(MAX_DOUBLINGS);
    let step = (BACKOFF_FLOOR * (1 << doublings)).min(BACKOFF_CEILING);
    // A sample outside [0, 1] would make `mul_f64` panic or overshoot.
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    step.mul_f64(1.0 + jitter)
        .max(retry_after.unwrap_or_default())
        .clamp(BACKOFF_FLOOR, BACKOFF_CEILING)
}

/// The wait that a `Retry-After` header value asks for: delta-seconds, or
/// an HTTP-date measured from `now`. `None` for a value that is neither. A
/// date in the past is zero.
///
/// Only the IMF-fixdate form of HTTP-date is read (`Sun, 06 Nov 1994
/// 08:49:37 GMT`): RFC 9110 makes senders use it.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let date = parse_imf_fixdate(value)?;
    Some(date.duration_since(now).unwrap_or(Duration::ZERO))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Sun, 06 Nov 1994 08:49:37 GMT`. The weekday is not checked. A date
/// before the epoch is the epoch.
fn parse_imf_fixdate(value: &str) -> Option<SystemTime> {
    let (_weekday, rest) = value.split_once(", ")?;
    let [day, month, year, time, "GMT"] = rest.split(' ').collect::<Vec<_>>()[..] else {
        return None;
    };
    let [hour, minute, second] = time.split(':').collect::<Vec<_>>()[..] else {
        return None;
    };
    let day = two_digits(day, 1..=31)?;
    let month = MONTHS.iter().position(|name| *name == month)? as i64 + 1;
    let year: i64 = year
        .parse()
        .ok()
        .filter(|year| (1000..=9999).contains(year))?;
    let seconds_of_day = two_digits(hour, 0..=23)? * 3600
        + two_digits(minute, 0..=59)? * 60
        + two_digits(second, 0..=60)?;
    let seconds = days_from_civil(year, month, day) * 86_400 + seconds_of_day;
    let seconds = u64::try_from(seconds).unwrap_or(0);
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn two_digits(text: &str, range: std::ops::RangeInclusive<i64>) -> Option<i64> {
    if text.len() != 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|number| range.contains(number))
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `url` as it may be logged: scheme, host, port and path. User info and
/// query can hold credentials, and log lines go to the journal.
pub fn redacted(url: &Url) -> String {
    format!(
        "{}://{}",
        url.scheme(),
        &url[Position::BeforeHost..Position::AfterPath]
    )
}

/// How the log names `item`: a remote image by its [`redacted`] URL, a
/// local one by its path.
pub fn loggable(item: &ItemId) -> Cow<'_, str> {
    match Url::parse(item.as_str()) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Cow::Owned(redacted(&url)),
        // A local item's id is an absolute path, which is no URL.
        _ => Cow::Borrowed(item.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn first_failure_waits_between_the_floor_and_twice_the_floor() {
        assert_eq!(backoff(1, None, 0.0), BACKOFF_FLOOR);
        assert_eq!(backoff(1, None, 0.5), Duration::from_secs(45));
    }

    #[test]
    fn each_failure_doubles_the_step() {
        assert_eq!(backoff(2, None, 0.0), Duration::from_secs(60));
        assert_eq!(backoff(3, None, 0.0), Duration::from_secs(120));
        assert_eq!(backoff(7, None, 0.0), Duration::from_secs(1920));
    }

    #[test]
    fn backoff_stops_at_the_ceiling() {
        assert_eq!(backoff(8, None, 0.0), BACKOFF_CEILING);
        assert_eq!(backoff(u32::MAX, None, 0.99), BACKOFF_CEILING);
        assert_eq!(backoff(7, None, 0.99), BACKOFF_CEILING);
    }

    #[test]
    fn zero_failures_counts_as_one() {
        assert_eq!(backoff(0, None, 0.0), BACKOFF_FLOOR);
    }

    #[test]
    fn retry_after_is_honoured_within_the_ceiling() {
        let long = Duration::from_secs(600);

        assert_eq!(backoff(1, Some(long), 0.3), long);
        assert_eq!(backoff(1, Some(Duration::from_secs(5)), 0.0), BACKOFF_FLOOR);
        assert_eq!(backoff(1, Some(BACKOFF_CEILING * 3), 0.0), BACKOFF_CEILING);
    }

    #[test]
    fn jitter_out_of_range_does_not_panic() {
        for jitter in [f64::NAN, f64::INFINITY, -1.0, 7.0] {
            let wait = backoff(3, None, jitter);

            assert!(
                (BACKOFF_FLOOR..=BACKOFF_CEILING).contains(&wait),
                "{jitter}"
            );
        }
    }

    #[test]
    fn retry_after_in_seconds() {
        let now = SystemTime::now();

        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_as_an_http_date() {
        // 1994-11-06T08:49:37Z.
        let date = UNIX_EPOCH + Duration::from_secs(784_111_777);

        assert_eq!(
            parse_retry_after(
                "Sun, 06 Nov 1994 08:49:37 GMT",
                date - Duration::from_secs(90)
            ),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn retry_after_date_in_the_past_is_zero() {
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", SystemTime::now()),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn leap_day_parses() {
        // 2024-02-29T00:00:00Z.
        assert_eq!(
            parse_imf_fixdate("Thu, 29 Feb 2024 00:00:00 GMT"),
            Some(UNIX_EPOCH + Duration::from_secs(1_709_164_800))
        );
    }

    #[test]
    fn retry_after_garbage_is_none() {
        let now = SystemTime::now();
        for value in [
            "",
            "-5",
            "1.5",
            "soon",
            "99999999999999999999999",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
        ] {
            assert_eq!(parse_retry_after(value, now), None, "{value:?}");
        }
    }

    #[test]
    fn redirect_from_https_to_http_is_refused() {
        let chain = [url("https://a.example/x"), url("http://b.example/y")];

        assert!(redirect_refusal(&chain[..1], &url("http://c.example/")).is_some());
        assert!(redirect_refusal(&chain, &url("http://c.example/")).is_some());
    }

    #[test]
    fn redirects_within_http_or_up_to_https_are_allowed() {
        let chain = [url("http://a.example/x")];

        assert_eq!(redirect_refusal(&chain, &url("http://b.example/")), None);
        assert_eq!(redirect_refusal(&chain, &url("https://b.example/")), None);
    }

    #[test]
    fn redirect_to_another_scheme_is_refused() {
        let chain = [url("http://a.example/x")];

        assert!(redirect_refusal(&chain, &url("file:///etc/passwd")).is_some());
        assert!(redirect_refusal(&chain, &url("ftp://a.example/x")).is_some());
    }

    #[test]
    fn sniff_names_the_format_by_magic_bytes() {
        let png = sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
        let jpeg = sniff(b"\xff\xd8\xff\xe0\0\x10JFIF").unwrap();

        assert_eq!((png.extension, png.mime), ("png", "image/png"));
        assert_eq!((jpeg.extension, jpeg.mime), ("jpg", "image/jpeg"));
        assert!(matches!(
            sniff(b"<!DOCTYPE html>"),
            Err(FetchError::NotAnImage)
        ));
        assert!(matches!(sniff(b""), Err(FetchError::NotAnImage)));
    }

    #[test]
    fn redacted_drops_credentials_and_query() {
        let secret = url("https://user:pw@example.com:8443/a/b.jpg?token=x#frag");

        assert_eq!(redacted(&secret), "https://example.com:8443/a/b.jpg");
    }

    #[test]
    fn loggable_redacts_a_remote_item_and_keeps_a_local_path() {
        let remote = ItemId::from_url(&url("https://user:pw@example.com/a.jpg?token=x"));
        let local = ItemId::new("/home/user/Pictures/a b.jpg");

        assert_eq!(loggable(&remote), "https://example.com/a.jpg");
        assert_eq!(loggable(&local), "/home/user/Pictures/a b.jpg");
    }

    #[test]
    fn invalid_validators_are_left_out() {
        let mut meta = Meta::new(url("https://example.com/a.jpg"));
        meta.etag = Some("\"ok\"".into());
        meta.last_modified = Some("bad\nvalue".into());

        let headers = conditional_headers(&meta);

        assert_eq!(headers.get(IF_NONE_MATCH).unwrap(), "\"ok\"");
        assert!(headers.get(IF_MODIFIED_SINCE).is_none());
    }
}
