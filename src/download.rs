// src/download.rs

//! File downloads (for example `Media` records' `MediaURL`), paced separately from the API
//!
//! A [`Downloader`] is made from a [`ResoClient`] with [`ResoClient::downloader`]. It shares the
//! client's access token but has its own [`RetryPolicy`], [`RateLimit`], timeout and counters, so
//! downloads do not use up the API's budget and the API does not slow downloads. Each download is
//! a conditional GET (`If-None-Match`, `If-Modified-Since`) streamed to `<dest>.part` and renamed
//! to `dest` once complete, so a failed or interrupted download never leaves a partial file under
//! the final name. [`Downloader::download_into`] streams the body into any [`DownloadTarget`]
//! instead (object storage, a hasher, memory), with the same retries and limits.
//!
//! Media files usually live on a CDN, not on the API host, so the API token is sent only where
//! [`SendToken`] allows (by default: the API's own scheme, host and port).

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{Client, StatusCode, Url};
use tokio::io::AsyncWriteExt;
use tracing::debug;

use crate::client::ResoClient;
use crate::error::{ResoError, Result};
use crate::retry::{
    is_retryable_status, parse_retry_after, ClientStats, Counters, Limiter, RateLimit, RetryPolicy,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// When to send the API access token with a file request
///
/// # Examples
///
/// ```
/// # use reso_client::SendToken;
/// assert_eq!(SendToken::parse("same_host"), Some(SendToken::SameHost));
/// assert_eq!(SendToken::default(), SendToken::SameHost);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SendToken {
    /// Never send it
    Never,
    /// Only to the API's own origin (scheme, host and port of the base URL)
    #[default]
    SameHost,
    /// To every file URL (only for servers whose file host needs the API token)
    Always,
}

impl SendToken {
    /// Parse `never`, `same_host` or `always`
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "never" => Some(Self::Never),
            "same_host" => Some(Self::SameHost),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// Settings of a [`Downloader`]
///
/// The default is a 120-second timeout per file, no retries, no rate limit and
/// [`SendToken::SameHost`].
///
/// # Examples
///
/// ```
/// # use reso_client::{DownloadConfig, RateLimit, RetryPolicy, SendToken};
/// # use std::time::Duration;
/// let config = DownloadConfig::default()
///     .with_timeout(Duration::from_secs(300))
///     .with_retry(RetryPolicy::new(5))
///     .with_rate_limit(RateLimit::default().per_second(4))
///     .with_send_token(SendToken::Never);
/// assert_eq!(config.send_token, SendToken::Never);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadConfig {
    /// Timeout for one whole file request, body included
    pub timeout: Duration,
    /// How failed downloads are repeated (network errors and 429/502/503/504)
    pub retry: RetryPolicy,
    /// Pace of file requests, separate from the API's
    pub rate_limit: RateLimit,
    /// When the API token goes with a file request
    pub send_token: SendToken,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            retry: RetryPolicy::default(),
            rate_limit: RateLimit::default(),
            send_token: SendToken::default(),
        }
    }
}

impl DownloadConfig {
    /// Set the timeout for one file request
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the retry policy
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Set the rate limit
    pub fn with_rate_limit(mut self, rate_limit: RateLimit) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    /// Set when the API token is sent
    pub fn with_send_token(mut self, send_token: SendToken) -> Self {
        self.send_token = send_token;
        self
    }
}

/// What a previous download of the same file returned, for a conditional request
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    /// The `ETag` header, sent back as `If-None-Match`
    pub etag: Option<String>,
    /// The `Last-Modified` header, sent back as `If-Modified-Since`
    pub last_modified: Option<String>,
}

/// A file written by [`Downloader::download_to`]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedFile {
    /// Bytes written
    pub bytes: u64,
    /// The response's `ETag`, if any
    pub etag: Option<String>,
    /// The response's `Last-Modified`, if any
    pub last_modified: Option<String>,
    /// The response's `Content-Type`, if any
    pub content_type: Option<String>,
}

/// The result of [`Downloader::download_to`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// The server answered 304: the file on disk is current and was not touched
    NotModified,
    /// The file was written to `dest`
    Saved(SavedFile),
}

/// Where [`Downloader::download_into`] streams a response body
///
/// One download may take several attempts (retries after a network error or a 429/5xx): each
/// attempt that gets a body calls [`start`](Self::start), then [`write`](Self::write) per chunk,
/// then either [`finish`](Self::finish) once the body is complete or [`abort`](Self::abort) when
/// it broke off, so a target throws away a partial body and the next attempt starts clean. A 304
/// calls none of them.
///
/// # Examples
///
/// ```
/// # use reso_client::{DownloadTarget, Result, SavedFile};
/// /// Keeps the body in memory
/// #[derive(Default)]
/// struct Memory(Vec<u8>);
///
/// impl DownloadTarget for Memory {
///     async fn start(&mut self, _file: &SavedFile) -> Result<()> {
///         self.0.clear();
///         Ok(())
///     }
///     async fn write(&mut self, chunk: &[u8]) -> Result<()> {
///         self.0.extend_from_slice(chunk);
///         Ok(())
///     }
///     async fn finish(&mut self) -> Result<()> {
///         Ok(())
///     }
///     async fn abort(&mut self) {
///         self.0.clear();
///     }
/// }
/// ```
pub trait DownloadTarget: Send {
    /// A body begins; `file` holds the response headers (its `bytes` is 0)
    fn start(&mut self, file: &SavedFile) -> impl Future<Output = Result<()>> + Send;
    /// The next chunk of the body
    fn write(&mut self, chunk: &[u8]) -> impl Future<Output = Result<()>> + Send;
    /// The body is complete: make it durable
    fn finish(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// The attempt failed after `start`: drop what was written
    fn abort(&mut self) -> impl Future<Output = ()> + Send;
}

/// The target of [`Downloader::download_to`]: `<dest>.part`, renamed to `dest` when complete
struct FileTarget<'p> {
    dest: &'p Path,
    part: PathBuf,
    file: Option<tokio::fs::File>,
}

impl DownloadTarget for FileTarget<'_> {
    async fn start(&mut self, _file: &SavedFile) -> Result<()> {
        let file = tokio::fs::File::create(&self.part)
            .await
            .map_err(|e| io_error(&self.part, &e))?;
        self.file = Some(file);
        Ok(())
    }

    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        match self.file.as_mut() {
            Some(file) => file
                .write_all(chunk)
                .await
                .map_err(|e| io_error(&self.part, &e)),
            None => Err(ResoError::Io(format!("{}: not open", self.part.display()))),
        }
    }

    async fn finish(&mut self) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await.map_err(|e| io_error(&self.part, &e))?;
        }
        tokio::fs::rename(&self.part, self.dest)
            .await
            .map_err(|e| io_error(self.dest, &e))
    }

    async fn abort(&mut self) {
        self.file = None;
        let _ = tokio::fs::remove_file(&self.part).await;
    }
}

/// Downloads files with its own retry, pacing and counters; see the [module docs](self)
///
/// # Examples
///
/// ```no_run
/// # use reso_client::{ClientConfig, DownloadConfig, DownloadOutcome, ResoClient, Validators};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ResoClient::with_config(ClientConfig::new("https://api.mls.com/odata", "token"))?;
/// let downloader = client.downloader(DownloadConfig::default())?;
/// let outcome = downloader
///     .download_to("https://cdn.mls.com/photo/1.jpg", "media/1".as_ref(), &Validators::default())
///     .await?;
/// if let DownloadOutcome::Saved(file) = outcome {
///     println!("{} bytes, {:?}", file.bytes, file.content_type);
/// }
/// # Ok(())
/// # }
/// ```
pub struct Downloader<'a> {
    client: &'a ResoClient,
    http: Client,
    config: DownloadConfig,
    limiter: Limiter,
    counters: Counters,
    api_origin: Option<String>,
}

/// What one attempt decided
enum Attempt {
    Done(DownloadOutcome),
    Retry {
        error: ResoError,
        retry_after: Option<Duration>,
    },
}

impl ResoClient {
    /// A [`Downloader`] that shares this client's token and has its own limits
    ///
    /// # Errors
    ///
    /// [`ResoError::Config`] when the HTTP client cannot be built.
    pub fn downloader(&self, config: DownloadConfig) -> Result<Downloader<'_>> {
        // File requests go through the same proxy, CAs and User-Agent as the API's
        let http = self.network().client(config.timeout)?;
        Ok(Downloader {
            client: self,
            http,
            limiter: Limiter::new(config.rate_limit.clone()),
            counters: Counters::default(),
            api_origin: Url::parse(self.base_url())
                .ok()
                .map(|u| u.origin().ascii_serialization()),
            config,
        })
    }
}

impl Downloader<'_> {
    /// Counters of file requests, retries and body bytes (separate from the client's)
    pub fn stats(&self) -> ClientStats {
        self.counters.snapshot()
    }

    /// Whether the API token goes with a request to `url`
    pub fn sends_token_to(&self, url: &str) -> bool {
        match self.config.send_token {
            SendToken::Never => false,
            SendToken::Always => true,
            SendToken::SameHost => match (&self.api_origin, Url::parse(url)) {
                (Some(api), Ok(u)) => u.origin().ascii_serialization() == *api,
                _ => false,
            },
        }
    }

    /// Download `url` to `dest`, unless `validators` show the copy on disk is current
    ///
    /// The parent directory is created when missing. The body is streamed to `<dest>.part`, then
    /// renamed to `dest`; on any failure the `.part` file is removed and `dest` is left as it was.
    ///
    /// # Errors
    ///
    /// - [`ResoError::InvalidQuery`] when `url` is not an absolute http(s) URL
    /// - the status errors of [`ResoClient::execute`] (`Forbidden` for an expired signed URL,
    ///   `NotFound`, ...) once retries are used up
    /// - [`ResoError::Network`] when the connection fails or breaks off mid-file
    /// - [`ResoError::Io`] when the file cannot be written
    pub async fn download_to(
        &self,
        url: &str,
        dest: &Path,
        validators: &Validators,
    ) -> Result<DownloadOutcome> {
        check_url(url)?;
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_error(parent, &e))?;
        }
        let mut target = FileTarget {
            dest,
            part: part_path(dest),
            file: None,
        };
        self.download_into(url, validators, &mut target).await
    }

    /// Download `url` into `target`, unless `validators` show the stored copy is current
    ///
    /// The same retries, pacing, token rule and counters as [`download_to`](Self::download_to);
    /// the body goes to `target` chunk by chunk and is never held whole in memory here. On
    /// success the returned [`SavedFile`] holds the headers and the byte count.
    ///
    /// # Errors
    ///
    /// Those of [`download_to`](Self::download_to); errors `target` returns end the download
    /// (they are not retried).
    pub async fn download_into<T: DownloadTarget>(
        &self,
        url: &str,
        validators: &Validators,
        target: &mut T,
    ) -> Result<DownloadOutcome> {
        let parsed = check_url(url)?;
        let with_token = self.sends_token_to(url);

        let policy = &self.config.retry;
        let mut retries = 0u32;
        loop {
            let outcome = self
                .attempt(parsed.as_str(), target, validators, with_token)
                .await;
            let (error, retry_after) = match outcome {
                Ok(Attempt::Done(done)) => return Ok(done),
                Ok(Attempt::Retry { error, retry_after }) => (error, retry_after),
                Err(e @ ResoError::Network(_)) => (e, None),
                Err(e) => return Err(e),
            };
            let delay = (retries < policy.max_retries)
                .then(|| policy.delay_for(retries, retry_after))
                .flatten();
            let Some(delay) = delay else {
                return Err(error);
            };
            debug!(
                "Download retry {} of {} in {:?} ({})",
                retries + 1,
                policy.max_retries,
                delay,
                error
            );
            self.counters.retry();
            tokio::time::sleep(delay).await;
            retries += 1;
        }
    }

    /// One request, with the one-time token renewal after a 401, streamed to `part`
    async fn attempt<T: DownloadTarget>(
        &self,
        url: &str,
        target: &mut T,
        validators: &Validators,
        with_token: bool,
    ) -> Result<Attempt> {
        let token = match with_token {
            true => Some(self.client.tokens().token(self.client.http()).await?),
            false => None,
        };
        let mut response = self.send(url, validators, token.as_deref()).await?;
        if let (StatusCode::UNAUTHORIZED, Some(rejected)) = (response.status(), token.as_deref()) {
            if let Some(renewed) = self
                .client
                .tokens()
                .renew_after_401(self.client.http(), rejected)
                .await?
            {
                response = self.send(url, validators, Some(&renewed)).await?;
            }
        }

        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(Attempt::Done(DownloadOutcome::NotModified));
        }
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, std::time::SystemTime::now()));
            let body = response.bytes().await.unwrap_or_default();
            self.count_bytes(body.len() as u64).await;
            let error = ResoError::from_status(status.as_u16(), &String::from_utf8_lossy(&body));
            if is_retryable_status(status.as_u16()) {
                return Ok(Attempt::Retry { error, retry_after });
            }
            return Err(error);
        }

        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let mut saved = SavedFile {
            bytes: 0,
            etag: header(ETAG),
            last_modified: header(LAST_MODIFIED),
            content_type: header(CONTENT_TYPE),
        };
        target.start(&saved).await?;
        let streamed = loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    saved.bytes += chunk.len() as u64;
                    if let Err(e) = target.write(&chunk).await {
                        break Err(e);
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => {
                    let e = e.without_url();
                    break Err(ResoError::Network(format!("download broke off: {e}")));
                }
            }
        };
        self.count_bytes(saved.bytes).await;
        if let Err(e) = streamed {
            target.abort().await;
            return Err(e);
        }
        if let Err(e) = target.finish().await {
            target.abort().await;
            return Err(e);
        }
        Ok(Attempt::Done(DownloadOutcome::Saved(saved)))
    }

    async fn send(
        &self,
        url: &str,
        validators: &Validators,
        token: Option<&str>,
    ) -> Result<reqwest::Response> {
        self.limiter.acquire().await;
        self.counters.request();
        let mut request = self.http.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(etag) = &validators.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(date) = &validators.last_modified {
            request = request.header(IF_MODIFIED_SINCE, date);
        }
        let response = request
            .send()
            .await
            // Without the URL: media URLs are often signed, and errors end up in logs and manifests
            .map_err(|e| ResoError::Network(e.without_url().to_string()))?;
        self.counters.status(response.status().as_u16());
        Ok(response)
    }

    async fn count_bytes(&self, n: u64) {
        self.counters.bytes(n);
        self.limiter.record_bytes(n).await;
    }
}

/// `url` parsed, when it is an absolute http(s) URL
fn check_url(url: &str) -> Result<Url> {
    Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "http" || u.scheme() == "https")
        .ok_or_else(|| ResoError::InvalidQuery("not an absolute http(s) URL".into()))
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name: OsString = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

fn io_error(path: &Path, e: &std::io::Error) -> ResoError {
    ResoError::Io(format!("{}: {}", path.display(), e.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;

    fn downloader_for(base: &str, send: SendToken) -> bool {
        let client = ResoClient::with_config(ClientConfig::new(base, "t")).expect("client builds");
        let d = client
            .downloader(DownloadConfig::default().with_send_token(send))
            .expect("downloader builds");
        d.sends_token_to("https://api.example.com:443/files/1.jpg")
    }

    #[test]
    fn token_goes_only_where_allowed() {
        assert!(downloader_for(
            "https://api.example.com/odata",
            SendToken::SameHost
        ));
        assert!(!downloader_for(
            "https://other.example.com/odata",
            SendToken::SameHost
        ));
        assert!(!downloader_for(
            "http://api.example.com/odata",
            SendToken::SameHost
        ));
        assert!(!downloader_for(
            "https://api.example.com:8443/odata",
            SendToken::SameHost
        ));
        assert!(!downloader_for(
            "https://api.example.com/odata",
            SendToken::Never
        ));
        assert!(downloader_for(
            "https://other.example.com/odata",
            SendToken::Always
        ));
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(
            part_path(Path::new("a/b.jpg")),
            PathBuf::from("a/b.jpg.part")
        );
    }
}
