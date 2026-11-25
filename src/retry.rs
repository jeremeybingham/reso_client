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
//! - [`RateLimit`]: a ceiling on requests per second, requests per hour and bytes per day for one
//!   client (one server). Requests wait their turn instead of failing.
//! - [`ClientStats`]: counters of requests sent, retries made and body bytes received.
//!
//! The defaults retry nothing and limit nothing, so a client behaves as before until configured.

use std::collections::hash_map::RandomState;
use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::time::Instant;

/// How a failed request is repeated
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
/// - `bytes_per_day`: response body bytes received in any 24 hours. The size of a response is
///   known only after it arrives, so one response can overshoot the ceiling; the next request
///   waits until enough older bytes have aged out.
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
    /// Start times of requests in the last hour, oldest first
    starts: VecDeque<Instant>,
    /// Response sizes received in the last 24 hours, oldest first
    bytes: VecDeque<(Instant, u64)>,
    bytes_total: u64,
}

impl LimiterState {
    fn prune(&mut self, now: Instant) {
        while self
            .starts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= HOUR)
        {
            self.starts.pop_front();
        }
        while let Some(&(t, n)) = self.bytes.front() {
            if now.duration_since(t) < DAY {
                break;
            }
            self.bytes_total -= n;
            self.bytes.pop_front();
        }
    }

    /// How long to wait before another request may start, if at all
    fn wait(&self, limit: &RateLimit, now: Instant) -> Option<Duration> {
        let mut wait: Option<Duration> = None;
        let mut need = |d: Duration| wait = Some(wait.map_or(d, |w| w.max(d)));

        for (max, window) in [(limit.per_second, SECOND), (limit.per_hour, HOUR)] {
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
        if let Some(max) = limit.bytes_per_day {
            if self.bytes_total >= max {
                let mut left = self.bytes_total;
                for &(t, n) in &self.bytes {
                    left -= n;
                    if left < max {
                        need((t + DAY).saturating_duration_since(now));
                        break;
                    }
                }
            }
        }
        wait
    }
}

/// Paces requests to a [`RateLimit`]
pub(crate) struct Limiter {
    limit: RateLimit,
    state: Mutex<LimiterState>,
}

impl Limiter {
    pub(crate) fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// Wait until a request may start, then count it. Waiters queue on the lock, so they are
    /// released one at a time in arrival order.
    pub(crate) async fn acquire(&self) {
        if self.limit == RateLimit::default() {
            return;
        }
        let mut state = self.state.lock().await;
        loop {
            let now = Instant::now();
            state.prune(now);
            match state.wait(&self.limit, now) {
                None => {
                    state.starts.push_back(now);
                    return;
                }
                Some(wait) => tokio::time::sleep(wait).await,
            }
        }
    }

    /// Count response bytes against the daily ceiling
    pub(crate) async fn record_bytes(&self, n: u64) {
        if self.limit.bytes_per_day.is_none() || n == 0 {
            return;
        }
        let mut state = self.state.lock().await;
        state.bytes.push_back((Instant::now(), n));
        state.bytes_total += n;
    }
}

/// Counters of what a client has done, from [`ResoClient::stats`](crate::ResoClient::stats)
///
/// `requests` counts every HTTP request to the API (retries and a request repeated after a 401
/// included; token requests are not). `retries` counts the repeats made by the [`RetryPolicy`].
/// `bytes` counts response body bytes read, error bodies included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientStats {
    /// HTTP requests sent
    pub requests: u64,
    /// Requests repeated because of a retryable failure
    pub retries: u64,
    /// Response body bytes received
    pub bytes: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    requests: AtomicU64,
    retries: AtomicU64,
    bytes: AtomicU64,
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

    pub(crate) fn snapshot(&self) -> ClientStats {
        ClientStats {
            requests: self.requests.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
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
    async fn no_limit_never_waits() {
        let times = start_times(RateLimit::default(), 100).await;
        assert!(times.iter().all(|t| *t == Duration::ZERO));
    }
}
