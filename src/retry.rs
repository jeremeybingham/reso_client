// src/retry.rs

//! Retry with backoff, client-side rate limiting and request counters
//!
//! Servers answer 429 (too many requests) or 502/503/504 (temporarily unavailable) when a client
//! is too fast or the server is struggling, and vendors suspend clients that keep exceeding their
//! limits. This module holds the three pieces the client uses to behave:
//!
//! - [`RetryPolicy`]: how often and how long to wait before repeating a failed request. Network
//!   errors and 429/502/503/504 are retried with exponential backoff and jitter; a `Retry-After`
//!   header (seconds, or an HTTP date) replaces the computed wait. Other 4xx answers are never
//!   retried.
//! - [`RateLimit`]: a ceiling on requests per second, hour and day and bytes per hour and day for one
//!   client (one server). Requests wait their turn instead of failing.
//! - [`ClientStats`]: counters of requests sent, retries made and body bytes received, for a
//!   client as a whole or (with [`count_requests`]) for one piece of work.
//! - [`RateUsage`]: how much of each configured [`RateLimit`] window is used now.
//!
//! The defaults retry nothing and limit nothing, so a client behaves as before until configured.

use std::collections::hash_map::RandomState;
use std::collections::VecDeque;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

/// How a failed request is repeated
///
/// Retried: network errors (including a response body cut off partway, #125) and the statuses
/// 429, 502, 503 and 504. A complete body that is not valid JSON is not retried.
///
/// The wait before retry number `n` (counting from 0) is `initial_backoff * 2^n`, capped at
/// `max_backoff`, then reduced by up to half at random so that many clients do not retry in step.
/// When the server sends `Retry-After`, that wait is used instead, unless it is longer than
/// `max_retry_after`: then the request is not retried and the server's answer is returned.
///
/// # Examples
///
/// ```
/// # use reso_client::RetryPolicy;
/// # use std::time::Duration;
/// let policy = RetryPolicy::new(4).with_backoff(Duration::from_secs(1), Duration::from_secs(30));
/// assert_eq!(policy.max_retries, 4);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (0 = never retry, the default)
    pub max_retries: u32,
    /// Wait before the first retry (default 500 ms)
    pub initial_backoff: Duration,
    /// Longest computed wait (default 30 s)
    pub max_backoff: Duration,
    /// Longest `Retry-After` that is honoured (default 120 s); a longer one ends the retrying
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// A policy that retries up to `max_retries` times, with the default waits
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Self::default()
        }
    }

    /// Set the first and the longest computed wait
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }

    /// Set the longest `Retry-After` that is honoured
    pub fn with_max_retry_after(mut self, max: Duration) -> Self {
        self.max_retry_after = max;
        self
    }

    /// The wait before retry number `retry` (from 0), or `None` when the server asked for a wait
    /// longer than `max_retry_after`
    pub(crate) fn delay_for(&self, retry: u32, retry_after: Option<Duration>) -> Option<Duration> {
        if let Some(wait) = retry_after {
            return (wait <= self.max_retry_after).then_some(wait);
        }
        let factor = 2u32.checked_pow(retry).unwrap_or(u32::MAX);
        let base = self
            .initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff);
        Some(base.mul_f64(0.5 + 0.5 * random_unit()))
    }
}

/// Whether an HTTP status is worth repeating: 429, 502, 503 and 504
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// A random number in `[0, 1]` (no `rand` dependency: std's per-instance random hasher keys)
fn random_unit() -> f64 {
    let bits = RandomState::new().build_hasher().finish();
    bits as f64 / u64::MAX as f64
}

/// Read a `Retry-After` value: whole seconds, or an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`)
///
/// A date in the past gives a zero wait. Only the preferred IMF-fixdate form is read; anything
/// else gives `None` and the computed backoff is used.
pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target = parse_http_date(value)?;
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts[..] else {
        return None;
    };
    if zone != "GMT" {
        return None;
    }
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == month)? as i64
        + 1;
    let mut hms = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if hms.next().is_some() || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    // Days since 1970-01-01 (proleptic Gregorian; Howard Hinnant's days_from_civil)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + m * 60 + s;
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

/// Client-side ceilings on how fast one client may talk to its server
///
/// A request that would exceed a ceiling waits until it would not. The limits are per client,
/// so use one client per server. A client shared by several tasks shares its limits.
///
/// - `per_second`: requests started in any one second.
/// - `per_hour`: requests started in any one hour.
/// - `per_day`: requests started in any 24 hours.
/// - `bytes_per_hour`, `bytes_per_day`: response body bytes received in any hour or 24 hours. The
///   size of a response is known only after it arrives, so one response can overshoot the
///   ceiling; the next request waits until enough older bytes have aged out.
///
/// # Examples
///
/// ```
/// # use reso_client::RateLimit;
/// let limit = RateLimit::default().per_second(2).per_hour(5_000).bytes_per_day(4_000_000_000);
/// assert_eq!(limit.per_second, Some(2));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests per second (`None` = unlimited)
    pub per_second: Option<u32>,
    /// Requests per hour (`None` = unlimited)
    pub per_hour: Option<u32>,
    /// Requests per 24 hours (`None` = unlimited)
    pub per_day: Option<u32>,
    /// Response bytes per hour (`None` = unlimited)
    pub bytes_per_hour: Option<u64>,
    /// Response bytes per 24 hours (`None` = unlimited)
    pub bytes_per_day: Option<u64>,
}

impl RateLimit {
    /// Allow at most `n` requests per second (0 is treated as 1)
    pub fn per_second(mut self, n: u32) -> Self {
        self.per_second = Some(n);
        self
    }

    /// Allow at most `n` requests per hour (0 is treated as 1)
    pub fn per_hour(mut self, n: u32) -> Self {
        self.per_hour = Some(n);
        self
    }

    /// Allow at most `n` requests per rolling 24 hours (0 is treated as 1)
    pub fn per_day(mut self, n: u32) -> Self {
        self.per_day = Some(n);
        self
    }

    /// Allow at most `n` response bytes per hour
    pub fn bytes_per_hour(mut self, n: u64) -> Self {
        self.bytes_per_hour = Some(n);
        self
    }

    /// Allow at most `n` response bytes per 24 hours
    pub fn bytes_per_day(mut self, n: u64) -> Self {
        self.bytes_per_day = Some(n);
        self
    }
}

const SECOND: Duration = Duration::from_secs(1);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86_400);

#[derive(Default)]
struct LimiterState {
    /// Start times of requests, oldest first, kept as long as the longest request window
    starts: VecDeque<Instant>,
    /// Response sizes received in the last 24 hours, oldest first
    bytes: VecDeque<(Instant, u64)>,
}

impl LimiterState {
    fn prune(&mut self, limit: &RateLimit, now: Instant) {
        let window = if limit.per_day.is_some() {
            DAY
        } else if limit.per_hour.is_some() {
            HOUR
        } else {
            SECOND
        };
        while self
            .starts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            self.starts.pop_front();
        }
        while self
            .bytes
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) >= DAY)
        {
            self.bytes.pop_front();
        }
    }

    /// How long to wait before another request may start, if at all
    fn wait(&self, limit: &RateLimit, now: Instant) -> Option<Duration> {
        let mut wait: Option<Duration> = None;
        let mut need = |d: Duration| wait = Some(wait.map_or(d, |w| w.max(d)));

        for (max, window) in [
            (limit.per_second, SECOND),
            (limit.per_hour, HOUR),
            (limit.per_day, DAY),
        ] {
            let Some(max) = max else { continue };
            let max = max.max(1) as usize;
            let in_window: Vec<Instant> = self
                .starts
                .iter()
                .copied()
                .filter(|t| now.duration_since(*t) < window)
                .collect();
            if in_window.len() >= max {
                // The request that frees a slot when it ages out of the window
                let t = in_window[in_window.len() - max];
                need((t + window).saturating_duration_since(now));
            }
        }
        for (max, window) in [(limit.bytes_per_hour, HOUR), (limit.bytes_per_day, DAY)] {
            let Some(max) = max else { continue };
            let in_window: Vec<&(Instant, u64)> = self
                .bytes
                .iter()
                .filter(|(t, _)| now.duration_since(*t) < window)
                .collect();
            let mut left: u64 = in_window.iter().map(|(_, n)| n).sum();
            if left >= max {
                for (t, n) in in_window {
                    left -= n;
                    if left < max {
                        need((*t + window).saturating_duration_since(now));
                        break;
                    }
                }
            }
        }
        wait
    }
}

/// How much of one [`RateLimit`] window is used: requests started, or response bytes received,
/// in the window that ends now
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowUsage {
    /// Requests or bytes counted in the window
    pub used: u64,
    /// The configured ceiling
    pub limit: u64,
}

impl WindowUsage {
    /// `used / limit` (0 for a limit of 0); above 1 when a large response overshot a byte ceiling
    pub fn fraction(&self) -> f64 {
        if self.limit == 0 {
            0.0
        } else {
            self.used as f64 / self.limit as f64
        }
    }
}

/// How much of a client's [`RateLimit`] is used now, from
/// [`ResoClient::rate_usage`](crate::ResoClient::rate_usage) or
/// [`Downloader::rate_usage`](crate::Downloader::rate_usage)
///
/// One field per window of the limit; a window with no ceiling configured is `None`. Windows
/// roll: `per_hour` counts the requests started in the hour that ends now. A client whose limit
/// was replaced (a new client after a rate share or secret change) starts again from zero, while
/// the server may still count the old client's requests.
///
/// # Examples
///
/// ```
/// # use reso_client::{ClientConfig, RateLimit, ResoClient};
/// let config = ClientConfig::new("https://api.mls.com/odata", "token")
///     .with_rate_limit(RateLimit::default().per_hour(7_200));
/// let client = ResoClient::with_config(config)?;
/// let usage = client.rate_usage();
/// assert_eq!(usage.per_hour.map(|w| (w.used, w.limit)), Some((0, 7_200)));
/// assert_eq!(usage.per_second, None);
/// # Ok::<(), reso_client::ResoError>(())
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateUsage {
    /// Requests started in the last second
    pub per_second: Option<WindowUsage>,
    /// Requests started in the last hour
    pub per_hour: Option<WindowUsage>,
    /// Requests started in the last 24 hours
    pub per_day: Option<WindowUsage>,
    /// Response bytes received in the last hour
    pub bytes_per_hour: Option<WindowUsage>,
    /// Response bytes received in the last 24 hours
    pub bytes_per_day: Option<WindowUsage>,
}

/// Paces requests to a [`RateLimit`]
///
/// Two locks: `queue` is held by one waiting request at a time (across its sleep), so waiters are
/// released in arrival order; `state` is held only for moments, so [`usage`](Self::usage) never
/// waits behind a throttled request.
pub(crate) struct Limiter {
    limit: RateLimit,
    queue: tokio::sync::Mutex<()>,
    state: std::sync::Mutex<LimiterState>,
}

impl Limiter {
    pub(crate) fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            queue: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(LimiterState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LimiterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until a request may start, then count it. Waiters queue on a lock, so they are
    /// released one at a time in arrival order.
    pub(crate) async fn acquire(&self) {
        if self.limit == RateLimit::default() {
            return;
        }
        let _turn = self.queue.lock().await;
        loop {
            let wait = {
                let now = Instant::now();
                let mut state = self.state();
                state.prune(&self.limit, now);
                let wait = state.wait(&self.limit, now);
                if wait.is_none() {
                    state.starts.push_back(now);
                }
                wait
            };
            match wait {
                None => return,
                Some(wait) => tokio::time::sleep(wait).await,
            }
        }
    }

    /// Count response bytes against the hourly and daily ceilings
    pub(crate) async fn record_bytes(&self, n: u64) {
        if (self.limit.bytes_per_day.is_none() && self.limit.bytes_per_hour.is_none()) || n == 0 {
            return;
        }
        self.state().bytes.push_back((Instant::now(), n));
    }

    /// How much of each configured window is used now
    pub(crate) fn usage(&self) -> RateUsage {
        let now = Instant::now();
        let mut state = self.state();
        state.prune(&self.limit, now);
        let requests = |max: Option<u32>, window: Duration| {
            max.map(|limit| WindowUsage {
                used: state
                    .starts
                    .iter()
                    .filter(|t| now.duration_since(**t) < window)
                    .count() as u64,
                limit: u64::from(limit),
            })
        };
        let bytes = |max: Option<u64>, window: Duration| {
            max.map(|limit| WindowUsage {
                used: state
                    .bytes
                    .iter()
                    .filter(|(t, _)| now.duration_since(*t) < window)
                    .map(|(_, n)| n)
                    .sum(),
                limit,
            })
        };
        RateUsage {
            per_second: requests(self.limit.per_second, SECOND),
            per_hour: requests(self.limit.per_hour, HOUR),
            per_day: requests(self.limit.per_day, DAY),
            bytes_per_hour: bytes(self.limit.bytes_per_hour, HOUR),
            bytes_per_day: bytes(self.limit.bytes_per_day, DAY),
        }
    }
}

/// Counters of what a client has done, from [`ResoClient::stats`](crate::ResoClient::stats)
///
/// `requests` counts every HTTP request to the API (retries and a request repeated after a 401
/// included; token requests are not). `retries` counts the repeats made by the [`RetryPolicy`].
/// `bytes` counts response body bytes read, error bodies included. The four response counters
/// sort every response that arrived by status: `ok` (2xx and 3xx, a 304 included), `throttled`
/// (429), `client_errors` (other 4xx) and `server_errors` (5xx); requests that got no response
/// (network errors) are in `requests` only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientStats {
    /// HTTP requests sent
    pub requests: u64,
    /// Requests repeated because of a retryable failure
    pub retries: u64,
    /// Response body bytes received
    pub bytes: u64,
    /// Responses with a 2xx or 3xx status
    pub ok: u64,
    /// Responses with status 429 (Too Many Requests)
    pub throttled: u64,
    /// Responses with a 4xx status other than 429
    pub client_errors: u64,
    /// Responses with a 5xx status
    pub server_errors: u64,
}

impl ClientStats {
    /// What happened since `earlier` (a snapshot taken before): each counter minus its old value
    #[must_use]
    pub fn since(&self, earlier: &ClientStats) -> ClientStats {
        ClientStats {
            requests: self.requests.saturating_sub(earlier.requests),
            retries: self.retries.saturating_sub(earlier.retries),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            ok: self.ok.saturating_sub(earlier.ok),
            throttled: self.throttled.saturating_sub(earlier.throttled),
            client_errors: self.client_errors.saturating_sub(earlier.client_errors),
            server_errors: self.server_errors.saturating_sub(earlier.server_errors),
        }
    }
}

#[derive(Default)]
pub(crate) struct Counters {
    requests: AtomicU64,
    retries: AtomicU64,
    bytes: AtomicU64,
    ok: AtomicU64,
    throttled: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
}

impl Counters {
    pub(crate) fn request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one response by its status code
    pub(crate) fn status(&self, code: u16) {
        let counter = match code {
            429 => &self.throttled,
            400..=499 => &self.client_errors,
            500..=599 => &self.server_errors,
            _ => &self.ok,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ClientStats {
        ClientStats {
            requests: self.requests.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            ok: self.ok.load(Ordering::Relaxed),
            throttled: self.throttled.load(Ordering::Relaxed),
            client_errors: self.client_errors.load(Ordering::Relaxed),
            server_errors: self.server_errors.load(Ordering::Relaxed),
        }
    }
}

/// Counters of one [`count_requests`] scope, with the scope it is nested in
struct Scope {
    counters: Counters,
    parent: Option<Arc<Scope>>,
}

tokio::task_local! {
    static SCOPE: Arc<Scope>;
}

/// Run `future` and count the requests [`ResoClient`](crate::ResoClient)s make while it runs
///
/// A client's [`stats`](crate::ResoClient::stats) count everything it did; when several tasks share
/// one client (feeds of one server syncing at once), the difference of two snapshots mixes their
/// requests. This counts only what `future` itself does, with any client: requests made inside a
/// task spawned from it are not counted, nor are [`Downloader`](crate::Downloader) requests (it
/// has its own [`stats`](crate::Downloader::stats)). Scopes nest: an inner scope's requests are
/// counted in the outer one too.
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{count_requests, QueryBuilder, ResoClient};
/// # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
/// let query = QueryBuilder::new("Property").top(10).build()?;
/// let (result, stats) = count_requests(client.execute(&query)).await;
/// let records = result?;
/// println!("{} requests, {} bytes", stats.requests, stats.bytes);
/// # Ok(())
/// # }
/// ```
pub async fn count_requests<F: Future>(future: F) -> (F::Output, ClientStats) {
    let scope = Arc::new(Scope {
        counters: Counters::default(),
        parent: SCOPE.try_with(Arc::clone).ok(),
    });
    let output = SCOPE.scope(Arc::clone(&scope), future).await;
    (output, scope.counters.snapshot())
}

/// Apply `count` to the counters of every [`count_requests`] scope the current task is in
pub(crate) fn count_in_scopes(count: impl Fn(&Counters)) {
    let _ = SCOPE.try_with(|scope| {
        let mut next = Some(scope);
        while let Some(s) = next {
            count(&s.counters);
            next = s.parent.as_ref();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777 - 60); // 1 minute before the date
        assert_eq!(parse_retry_after("7", now), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(60))
        );
        // A date in the past means no wait; junk means no value
        assert_eq!(
            parse_retry_after(
                "Sun, 06 Nov 1994 08:49:37 GMT",
                UNIX_EPOCH + Duration::from_secs(1_000_000_000)
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(
            parse_retry_after("Sun, 31 Foo 1994 08:49:37 GMT", now),
            None
        );
    }

    #[test]
    fn malformed_http_dates_are_ignored() {
        for value in [
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, xx Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 19x4 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 06 Nov 1994 08:49:37:01 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 aa:49:37 GMT",
        ] {
            assert_eq!(parse_http_date(value), None, "{value}");
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = RetryPolicy::new(5).with_backoff(Duration::from_secs(1), Duration::from_secs(8));
        for _ in 0..50 {
            let d0 = p.delay_for(0, None).unwrap();
            assert!(d0 >= Duration::from_millis(500) && d0 <= Duration::from_secs(1));
            let d2 = p.delay_for(2, None).unwrap();
            assert!(d2 >= Duration::from_secs(2) && d2 <= Duration::from_secs(4));
            let big = p.delay_for(40, None).unwrap();
            assert!(big >= Duration::from_secs(4) && big <= Duration::from_secs(8));
        }
    }

    #[test]
    fn retry_after_replaces_backoff_unless_too_long() {
        let p = RetryPolicy::new(3).with_max_retry_after(Duration::from_secs(10));
        assert_eq!(
            p.delay_for(0, Some(Duration::from_secs(9))),
            Some(Duration::from_secs(9))
        );
        assert_eq!(p.delay_for(0, Some(Duration::from_secs(11))), None);
    }

    #[test]
    fn only_the_listed_statuses_retry() {
        for s in [429, 502, 503, 504] {
            assert!(is_retryable_status(s));
        }
        for s in [400, 401, 403, 404, 500, 501] {
            assert!(!is_retryable_status(s));
        }
    }

    /// Start times observed by a paused-clock run never put more than `n` in any window
    async fn start_times(limit: RateLimit, count: usize) -> Vec<Duration> {
        let limiter = Limiter::new(limit);
        let t0 = Instant::now();
        let mut times = Vec::new();
        for _ in 0..count {
            limiter.acquire().await;
            times.push(t0.elapsed());
        }
        times
    }

    #[tokio::test(start_paused = true)]
    async fn per_second_limit_is_never_exceeded() {
        let times = start_times(RateLimit::default().per_second(3), 10).await;
        for (i, t) in times.iter().enumerate() {
            let in_window = times
                .iter()
                .filter(|u| *u <= t && *t - **u < SECOND)
                .count();
            assert!(
                in_window <= 3,
                "request {i} at {t:?}: {in_window} in one second"
            );
        }
        assert!(*times.last().unwrap() >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn per_hour_limit_is_never_exceeded() {
        let times = start_times(RateLimit::default().per_hour(4), 9).await;
        for t in &times {
            let in_window = times.iter().filter(|u| *u <= t && *t - **u < HOUR).count();
            assert!(in_window <= 4);
        }
        assert!(*times.last().unwrap() >= Duration::from_secs(7200));
    }

    #[tokio::test(start_paused = true)]
    async fn both_limits_apply_together() {
        let times = start_times(RateLimit::default().per_second(2).per_hour(3), 5).await;
        // The second-limit holds the third back 1 s; the hour limit holds the fourth back an hour
        assert!(times[2] >= SECOND && times[2] < HOUR);
        assert!(times[3] >= HOUR);
        assert!(times[4] >= HOUR);
    }

    #[tokio::test(start_paused = true)]
    async fn bytes_per_day_waits_for_old_bytes_to_age_out() {
        let limiter = Limiter::new(RateLimit::default().bytes_per_day(1000));
        let t0 = Instant::now();
        limiter.acquire().await;
        limiter.record_bytes(600).await;
        limiter.acquire().await; // 600 < 1000: no wait
        limiter.record_bytes(600).await;
        assert!(t0.elapsed() < Duration::from_secs(1));
        limiter.acquire().await; // 1200 >= 1000: waits until the first 600 age out
        assert!(t0.elapsed() >= DAY);
    }

    #[tokio::test(start_paused = true)]
    async fn bytes_per_hour_waits_for_old_bytes_to_age_out() {
        let limiter = Limiter::new(RateLimit::default().bytes_per_hour(1000));
        let t0 = Instant::now();
        limiter.acquire().await;
        limiter.record_bytes(600).await;
        limiter.acquire().await; // 600 < 1000: no wait
        limiter.record_bytes(600).await;
        assert!(t0.elapsed() < Duration::from_secs(1));
        limiter.acquire().await; // 1200 >= 1000: waits for the first 600 to age out of the hour
        assert!(t0.elapsed() >= HOUR && t0.elapsed() < DAY);
    }

    #[tokio::test(start_paused = true)]
    async fn per_day_limit_is_never_exceeded() {
        let times = start_times(RateLimit::default().per_hour(3).per_day(5), 7).await;
        assert!(times[2] < HOUR);
        assert!(times[3] >= HOUR && times[4] >= HOUR && times[4] < DAY);
        assert!(times[5] >= DAY && times[6] >= DAY);
    }

    #[tokio::test(start_paused = true)]
    async fn no_limit_never_waits() {
        let times = start_times(RateLimit::default(), 100).await;
        assert!(times.iter().all(|t| *t == Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn usage_counts_each_configured_window() {
        let limiter = Limiter::new(
            RateLimit::default()
                .per_second(10)
                .per_hour(100)
                .bytes_per_day(1000),
        );
        assert_eq!(limiter.usage().per_day, None);
        for _ in 0..3 {
            limiter.acquire().await;
        }
        limiter.record_bytes(400).await;
        let usage = limiter.usage();
        assert_eq!(usage.per_second, Some(WindowUsage { used: 3, limit: 10 }));
        assert_eq!(
            usage.per_hour,
            Some(WindowUsage {
                used: 3,
                limit: 100
            })
        );
        assert_eq!(usage.bytes_per_hour, None);
        assert_eq!(usage.bytes_per_day.map(|w| w.used), Some(400));
        assert!((usage.bytes_per_day.unwrap().fraction() - 0.4).abs() < 1e-9);
        // The second window empties, the hour and day ones do not
        tokio::time::advance(Duration::from_secs(2)).await;
        let usage = limiter.usage();
        assert_eq!(usage.per_second.map(|w| w.used), Some(0));
        assert_eq!(usage.per_hour.map(|w| w.used), Some(3));
        assert_eq!(usage.bytes_per_day.map(|w| w.used), Some(400));
        assert_eq!(
            Limiter::new(RateLimit::default()).usage(),
            RateUsage::default()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn usage_does_not_wait_behind_a_held_request() {
        let limiter = Arc::new(Limiter::new(RateLimit::default().per_hour(1)));
        limiter.acquire().await;
        // The second request waits an hour, holding its place in the queue
        let waiting = tokio::spawn({
            let limiter = Arc::clone(&limiter);
            async move { limiter.acquire().await }
        });
        tokio::task::yield_now().await;
        let usage = limiter.usage();
        assert_eq!(usage.per_hour, Some(WindowUsage { used: 1, limit: 1 }));
        assert!(!waiting.is_finished());
        waiting.await.unwrap();
        assert_eq!(limiter.usage().per_hour.map(|w| w.used), Some(1));
    }

    #[tokio::test]
    async fn scopes_nest_and_count_only_their_own_work() {
        let (((), inner), outer) = count_requests(async {
            count_in_scopes(Counters::request);
            count_requests(async { count_in_scopes(|c| c.bytes(5)) }).await
        })
        .await;
        assert_eq!((inner.requests, inner.bytes), (0, 5));
        assert_eq!((outer.requests, outer.bytes), (1, 5));
        // Outside any scope nothing is counted, and nothing fails
        count_in_scopes(Counters::request);
        // A second scope running at the same time keeps its own counts
        let a = count_requests(async { count_in_scopes(Counters::request) });
        let b = count_requests(async {
            count_in_scopes(Counters::request);
            count_in_scopes(Counters::request);
        });
        let (((), a), ((), b)) = tokio::join!(a, b);
        assert_eq!((a.requests, b.requests), (1, 2));
    }
}
