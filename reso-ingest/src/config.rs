//! The TOML configuration: many servers, each with auth, limits and resource filters

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reso_client::{
    Auth, ClientConfig, ClientCredentials, CredentialTransport, DownloadConfig, NetworkOptions,
    RateLimit, RetryPolicy, SendToken,
};
use serde::Deserialize;

use crate::error::IngestError;

/// Keys that would hold a secret in the file itself; they are rejected with a pointer to the `_env` and `_file` forms
const SECRET_KEYS: &[&str] = &[
    "token",
    "access_token",
    "bearer_token",
    "client_secret",
    "secret",
    "password",
];

const DEFAULT_MAX_CONCURRENT: usize = 4;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A parsed configuration file
///
/// ```toml
/// max_concurrent_servers = 2          # optional, default 4
/// webhook_url_env = "RESO_WEBHOOK"    # optional; variable holding the URL that gets the run summary
///
/// [[server]]
/// name = "bridge"
/// base_url = "https://api.bridgedataoutput.com/api/v2/OData"
/// dataset_id = "actris_ref"           # optional
/// page_size = 1000                    # optional
/// include = ["Property", "Member"]    # optional; empty = every resource
/// exclude = ["Media"]                 # optional; wins over include
/// filter = "OriginatingSystemName eq 'mls'"   # optional; added to every query
///
/// [server.resources.Property]         # optional; settings for one resource
/// filter = "PropertyType eq 'Residential'"    # ANDed with the server filter
/// expand = ["Media"]                  # `$expand` on sync pulls
/// page_size = 1000
///
/// [server.auth]
/// type = "client_credentials"
/// token_url = "https://auth.example/token"
/// client_id_env = "BRIDGE_CLIENT_ID"
/// client_secret_env = "BRIDGE_CLIENT_SECRET"
///
/// [server.limits]                     # optional
/// max_retries = 4
/// per_second = 2
///
/// [server.media]                      # optional; file downloads (`reso-ingest media`)
/// per_second = 4
/// send_token = "same_host"            # never, same_host (default) or always
///
/// [server.network]                    # optional; proxy, CA, User-Agent, pool
/// proxy = "http://proxy.corp.example:3128"   # or proxy_env / proxy_file when it holds a password
/// ca_files = ["/etc/ssl/corp-ca.pem"]
/// user_agent = "acme-ingest/1.0 (data@acme.example)"
/// ```
///
/// See `reso-ingest/servers.example.toml` for a complete file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Most servers running at the same time (default 4)
    pub max_concurrent_servers: usize,
    /// Name of the environment variable that holds the webhook URL (`webhook_url_env`); the URL
    /// is read when the run ends, and the run summary is POSTed to it as JSON
    pub webhook_url_env: Option<String>,
    /// The servers, in file order; names are unique
    pub servers: Vec<ServerConfig>,
}

/// One server's settings
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Unique name; tags records and names checkpoints
    pub name: String,
    /// Base URL of the RESO Web API
    pub base_url: String,
    /// Dataset id inserted between base URL and resource (vendor mode)
    #[serde(default)]
    pub dataset_id: Option<String>,
    /// `$top` used when paging
    #[serde(default)]
    pub page_size: Option<u32>,
    /// HTTP timeout in seconds (default 30)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Resources to pull; empty means every resource the server offers
    #[serde(default)]
    pub include: Vec<String>,
    /// Resources never to pull; wins over `include`
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Filter added (by `and`) to every query sent to this server: sync pulls, the key pulls and
    /// lookups that find deletes. Some servers require one (MLS Grid: `OriginatingSystemName`).
    #[serde(default)]
    pub filter: Option<String>,
    /// How this server signals deletes, when not by `EntityEvent` or a key comparison
    #[serde(default)]
    pub deletes: DeletesConfig,
    /// Settings for single resources, by resource name
    #[serde(default)]
    pub resources: BTreeMap<String, ResourceConfig>,
    /// `serve` only: how often to sync the resources in `include` that have no `interval` of their
    /// own (`"2m"`, `"30s"`, `"1h"`)
    #[serde(default)]
    pub interval: Option<String>,
    /// `serve` only: most feeds of this server syncing at once (default 2)
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Leave the server alone: `sync` skips it and `serve` schedules none of its feeds, until
    /// it is set back to `false` (a running `serve` picks the change up on reload)
    #[serde(default)]
    pub paused: bool,
    /// How to authenticate
    pub auth: AuthConfig,
    /// Retry and rate limits
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Media file downloads: their own limits, and where the token goes
    #[serde(default)]
    pub media: MediaConfig,
    /// `sync --backfill`: the backfill's share of the rate limits
    #[serde(default)]
    pub backfill: BackfillConfig,
    /// Proxy, extra CA certificates, User-Agent and connection pool, for API and media requests
    #[serde(default)]
    pub network: NetworkConfig,
}

/// The User-Agent `reso-ingest` sends when `network.user_agent` is not set
pub const DEFAULT_USER_AGENT: &str = concat!("reso-ingest/", env!("CARGO_PKG_VERSION"));

/// Network settings of one server (all optional); see [`NetworkOptions`]
///
/// Without a proxy setting the `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` variables
/// are honoured. A proxy URL with a user name or password is a secret: name it with `proxy_env`
/// or `proxy_file`, never `proxy`; it is read again with the auth secrets, so a rotated file is
/// picked up without a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Proxy URL for every request of this server (no credentials in it)
    #[serde(default)]
    pub proxy: Option<String>,
    /// Variable holding the proxy URL (may hold `user:password@`)
    #[serde(default)]
    pub proxy_env: Option<String>,
    /// File holding the proxy URL (may hold `user:password@`)
    #[serde(default)]
    pub proxy_file: Option<String>,
    /// Hosts that bypass the proxy, in the `NO_PROXY` format (`localhost,.example.com`)
    #[serde(default)]
    pub no_proxy: Option<String>,
    /// Honour the proxy environment variables when no proxy is set (default true)
    #[serde(default)]
    pub system_proxy: Option<bool>,
    /// PEM files of extra trusted CA certificates
    #[serde(default)]
    pub ca_files: Vec<String>,
    /// The `User-Agent` header (default `reso-ingest/<version>`)
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Most idle connections kept per host
    #[serde(default)]
    pub pool_max_idle_per_host: Option<usize>,
    /// Seconds an idle connection is kept (default 90)
    #[serde(default)]
    pub pool_idle_timeout_secs: Option<u64>,
}

impl NetworkConfig {
    /// Where the proxy URL is read from, when `proxy_env` or `proxy_file` is set
    fn proxy_source(&self) -> Result<Option<SecretSource>, String> {
        if self.proxy_env.is_none() && self.proxy_file.is_none() {
            return Ok(None);
        }
        SecretSource::pick("network", "proxy", &self.proxy_env, &self.proxy_file).map(Some)
    }

    fn validate(&self) -> Result<(), String> {
        let sources = [&self.proxy, &self.proxy_env, &self.proxy_file]
            .iter()
            .filter(|s| s.is_some())
            .count();
        if sources > 1 {
            return Err("network: set only one of proxy, proxy_env and proxy_file".into());
        }
        if let Some(proxy) = &self.proxy {
            check_proxy_url(proxy).map_err(|e| format!("network.proxy {e}"))?;
            let url = reqwest::Url::parse(proxy)
                .map_err(|_| "network.proxy is not a valid URL".to_owned())?;
            if !url.username().is_empty() || url.password().is_some() {
                return Err("network.proxy must not hold a user name or password; put the URL in a variable or file and name it with proxy_env or proxy_file".into());
            }
        }
        self.proxy_source()?;
        if self
            .no_proxy
            .as_deref()
            .is_some_and(|h| h.trim().is_empty())
        {
            return Err("network.no_proxy must not be empty".into());
        }
        if self.ca_files.iter().any(|f| f.trim().is_empty()) {
            return Err("network.ca_files has an empty path".into());
        }
        if let Some(agent) = &self.user_agent {
            if agent.trim().is_empty() || agent.chars().any(|c| c.is_control()) {
                return Err("network.user_agent must be non-empty text on one line".into());
            }
        }
        if self.pool_idle_timeout_secs == Some(0) {
            return Err("network.pool_idle_timeout_secs must be at least 1".into());
        }
        Ok(())
    }

    /// The [`NetworkOptions`] these settings describe, with `proxy` read from its source if any
    ///
    /// CA files are read now.
    fn options(&self, proxy: Option<String>) -> Result<NetworkOptions, String> {
        let mut options = NetworkOptions::default()
            .with_user_agent(self.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT));
        if let Some(url) = proxy.or_else(|| self.proxy.clone()) {
            options = options.with_proxy(url);
        }
        if let Some(hosts) = &self.no_proxy {
            options = options.with_no_proxy(hosts);
        }
        if self.system_proxy == Some(false) {
            options = options.without_system_proxy();
        }
        for path in &self.ca_files {
            options = options
                .with_ca_file(path)
                .map_err(|e| format!("network.ca_files: {e}"))?;
        }
        if let Some(max) = self.pool_max_idle_per_host {
            options = options.with_pool_max_idle_per_host(max);
        }
        if let Some(secs) = self.pool_idle_timeout_secs {
            options = options.with_pool_idle_timeout(Duration::from_secs(secs));
        }
        Ok(options)
    }
}

/// A proxy URL must be `http://` or `https://`
fn check_proxy_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err("must start with http:// or https://".into())
    }
}

/// The share of a server's `[server.limits]` rate limits a backfill runs within (`sync --backfill`)
///
/// While a backfill of the server runs, it gets `rate_percent` of each request and byte limit and
/// every other sync of the server (another process, or `serve`) gets the rest, so the two together
/// stay within the limits and incremental syncs keep their freshness. Without a backfill running,
/// other syncs use the whole limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillConfig {
    /// Percent of each rate limit for the backfill, 1 to 99 (default 50)
    #[serde(default)]
    pub rate_percent: Option<u8>,
}

/// The backfill's share of the rate limits when `rate_percent` is not set
pub const DEFAULT_BACKFILL_RATE_PERCENT: u8 = 50;

impl BackfillConfig {
    /// The backfill's percent of each rate limit
    pub fn rate_percent(&self) -> u8 {
        self.rate_percent.unwrap_or(DEFAULT_BACKFILL_RATE_PERCENT)
    }
}

/// Vendor delete signalling (`deletes = { flag_field = "MlgCanView" }`)
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletesConfig {
    /// A field that is `false` on a removed record (MLS Grid: `MlgCanView`). The record is still
    /// written raw, plus a delete event for its key; `--deletes` does no key comparison for this
    /// server.
    #[serde(default)]
    pub flag_field: Option<String>,
    /// Hold back a delete pass that finds more than this many deletes (`--deletes`; not for
    /// `flag_field`). See [`DeleteLimits`](crate::DeleteLimits).
    #[serde(default)]
    pub max_deletes: Option<u64>,
    /// Hold back a key-comparison pass that deletes more than this percentage (1 to 100) of the
    /// keys known from the previous pass
    #[serde(default)]
    pub max_percent: Option<u32>,
}

impl DeletesConfig {
    /// The limits these settings describe; `allow` skips them (`--allow-mass-deletes`)
    pub fn limits(&self, allow: bool) -> crate::DeleteLimits {
        crate::DeleteLimits {
            max_deletes: self.max_deletes,
            max_percent: self.max_percent.map(f64::from),
            allow,
        }
    }
}

/// Settings for one resource of a server (`[server.resources.<Name>]`)
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    /// Filter for this resource, combined by `and` with the server's `filter`
    #[serde(default)]
    pub filter: Option<String>,
    /// Navigation properties to expand on sync pulls (`$expand`), for example `["Media"]`
    #[serde(default)]
    pub expand: Vec<String>,
    /// `$top` for this resource; overrides the server's `page_size` (servers may allow fewer
    /// records per page with `$expand`)
    #[serde(default)]
    pub page_size: Option<u32>,
    /// `serve` only: how often to sync this resource (`"2m"`, `"30s"`, `"1h"`, `"500ms"`)
    #[serde(default)]
    pub interval: Option<String>,
    /// Leave this resource alone: `sync` skips it and `serve` does not schedule it, while its
    /// checkpoint is kept for when it is resumed
    #[serde(default)]
    pub paused: bool,
}

/// The longest wait the daemon puts between two syncs of a failing feed, unless the feed's own
/// interval is longer
pub const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Parse an interval such as `500ms`, `30s`, `2m`, `1h` or `1d` (a whole number and a unit)
///
/// ```
/// # use reso_ingest::parse_interval;
/// assert_eq!(parse_interval("2m").unwrap().as_secs(), 120);
/// assert!(parse_interval("0s").is_err());
/// ```
pub fn parse_interval(text: &str) -> Result<std::time::Duration, String> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, unit) = t.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{text}` is not an interval (examples: 30s, 2m, 1h)"))?;
    let millis = match unit.trim() {
        "ms" => n,
        "s" => n.saturating_mul(1000),
        "m" => n.saturating_mul(60_000),
        "h" => n.saturating_mul(3_600_000),
        "d" => n.saturating_mul(86_400_000),
        _ => {
            return Err(format!(
                "`{text}` is not an interval (examples: 30s, 2m, 1h)"
            ))
        }
    };
    if millis == 0 {
        return Err(format!("interval `{text}` must be more than zero"));
    }
    Ok(std::time::Duration::from_millis(millis))
}

/// Authentication settings; secrets are named by environment variable or file, never written in the file
///
/// Each secret takes one source: `<name>_env` (an environment variable) or `<name>_file` (a file,
/// for example a Kubernetes secret mount). Sources are read again before each resource is synced,
/// so a rotated file is picked up without a restart; see [`ClientHandle`](crate::ClientHandle).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// A static bearer token read from `token_env` or `token_file`
    Bearer {
        /// Variable holding the token
        #[serde(default)]
        token_env: Option<String>,
        /// File holding the token
        #[serde(default)]
        token_file: Option<String>,
    },
    /// OAuth2 client credentials
    ClientCredentials {
        /// Token endpoint URL
        token_url: String,
        /// Variable holding the client id
        #[serde(default)]
        client_id_env: Option<String>,
        /// File holding the client id
        #[serde(default)]
        client_id_file: Option<String>,
        /// Variable holding the client secret
        #[serde(default)]
        client_secret_env: Option<String>,
        /// File holding the client secret
        #[serde(default)]
        client_secret_file: Option<String>,
        /// Optional scope
        #[serde(default)]
        scope: Option<String>,
        /// `body` (default), `header` or `query`
        #[serde(default)]
        transport: Option<String>,
    },
}

impl AuthConfig {
    /// The sources of the secrets, in order: `token` for a bearer token; `client_id` then
    /// `client_secret` for client credentials
    ///
    /// # Errors
    ///
    /// A message when a secret has both sources or neither.
    pub fn sources(&self) -> Result<Vec<SecretSource>, String> {
        match self {
            AuthConfig::Bearer {
                token_env,
                token_file,
            } => Ok(vec![SecretSource::pick(
                "auth", "token", token_env, token_file,
            )?]),
            AuthConfig::ClientCredentials {
                client_id_env,
                client_id_file,
                client_secret_env,
                client_secret_file,
                ..
            } => Ok(vec![
                SecretSource::pick("auth", "client_id", client_id_env, client_id_file)?,
                SecretSource::pick(
                    "auth",
                    "client_secret",
                    client_secret_env,
                    client_secret_file,
                )?,
            ]),
        }
    }
}

/// Where one secret is read from
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    /// An environment variable, by name (`*_env`)
    Env(String),
    /// A file (`*_file`); surrounding whitespace, such as a final newline, is removed
    File(PathBuf),
}

impl SecretSource {
    fn pick(
        section: &str,
        field: &str,
        env: &Option<String>,
        file: &Option<String>,
    ) -> Result<Self, String> {
        match (env, file) {
            (Some(e), None) => {
                // A pasted secret in an `_env` field would not look like a variable name
                if is_env_name(e) {
                    Ok(Self::Env(e.clone()))
                } else {
                    Err(format!(
                        "{section}.{field}_env must be an environment variable name (letters, digits, underscore); the value is not echoed because it may be a secret"
                    ))
                }
            }
            (None, Some(f)) if f.trim().is_empty() => {
                Err(format!("{section}.{field}_file must not be empty"))
            }
            (None, Some(f)) => Ok(Self::File(PathBuf::from(f))),
            (Some(_), Some(_)) => Err(format!(
                "{section}: set only one of {field}_env and {field}_file"
            )),
            (None, None) => Err(format!(
                "{section}: {field}_env or {field}_file is required"
            )),
        }
    }

    /// Read the secret now; `lookup` reads environment variables
    ///
    /// # Errors
    ///
    /// [`IngestError::MissingEnv`] for an unset or empty variable, [`IngestError::SecretFile`] for
    /// a file that cannot be read or is empty. Neither holds the secret.
    pub fn read(
        &self,
        server: &str,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, IngestError> {
        match self {
            SecretSource::Env(variable) => {
                lookup(variable)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| IngestError::MissingEnv {
                        server: server.to_owned(),
                        variable: variable.clone(),
                    })
            }
            SecretSource::File(path) => {
                let fail = |message: String| IngestError::SecretFile {
                    server: server.to_owned(),
                    path: path.display().to_string(),
                    message,
                };
                // The error is the I/O error kind only: its text never holds file contents
                let text = std::fs::read_to_string(path).map_err(|e| fail(e.kind().to_string()))?;
                let value = text.trim();
                if value.is_empty() {
                    return Err(fail("the file is empty".into()));
                }
                Ok(value.to_owned())
            }
        }
    }
}

/// The values of a server's secrets, in the order of [`AuthConfig::sources`], then the proxy URL
/// when it is read from `network.proxy_env` or `network.proxy_file`; `Debug` redacts them
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secrets(Vec<String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secrets(<{} redacted>)", self.0.len())
    }
}

/// Retries per request when a server does not set `max_retries` (the library itself defaults to 0)
pub const DEFAULT_MAX_RETRIES: u32 = 5;

/// Retry and rate-limit settings (all optional; default is 5 retries and no rate limit)
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Retries after the first attempt
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// First backoff in milliseconds
    #[serde(default)]
    pub initial_backoff_ms: Option<u64>,
    /// Longest backoff in milliseconds
    #[serde(default)]
    pub max_backoff_ms: Option<u64>,
    /// Requests per second
    #[serde(default)]
    pub per_second: Option<u32>,
    /// Requests per hour
    #[serde(default)]
    pub per_hour: Option<u32>,
    /// Requests per day
    #[serde(default)]
    pub per_day: Option<u32>,
    /// Response bytes per hour
    #[serde(default)]
    pub bytes_per_hour: Option<u64>,
    /// Response bytes per day
    #[serde(default)]
    pub bytes_per_day: Option<u64>,
}

/// Timeout for one media file when `timeout_secs` is not set
pub const DEFAULT_MEDIA_TIMEOUT_SECS: u64 = 120;

/// Media download settings, separate from the API's (all optional)
///
/// Defaults: 5 retries, no rate limit, a 120-second timeout per file, and the API token sent
/// only to the API's own host (`send_token = "same_host"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    /// Retries after the first attempt
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// First backoff in milliseconds
    #[serde(default)]
    pub initial_backoff_ms: Option<u64>,
    /// Longest backoff in milliseconds
    #[serde(default)]
    pub max_backoff_ms: Option<u64>,
    /// File requests per second
    #[serde(default)]
    pub per_second: Option<u32>,
    /// File requests per hour
    #[serde(default)]
    pub per_hour: Option<u32>,
    /// File requests per day
    #[serde(default)]
    pub per_day: Option<u32>,
    /// File bytes per hour
    #[serde(default)]
    pub bytes_per_hour: Option<u64>,
    /// File bytes per day
    #[serde(default)]
    pub bytes_per_day: Option<u64>,
    /// Timeout for one file in seconds (default 120)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// `never`, `same_host` (default) or `always`
    #[serde(default)]
    pub send_token: Option<String>,
    /// Field of a record that holds its nested media records, for `sync --media` (default `Media`)
    #[serde(default)]
    pub nested_field: Option<String>,
    /// Download workers for this server's files (default 4); they share the limits above
    #[serde(default)]
    pub workers: Option<usize>,
}

impl MediaConfig {
    /// The [`DownloadConfig`] these settings describe
    pub fn download_config(&self) -> DownloadConfig {
        let defaults = RetryPolicy::default();
        let retry = RetryPolicy::new(self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES)).with_backoff(
            self.initial_backoff_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.initial_backoff),
            self.max_backoff_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.max_backoff),
        );
        DownloadConfig::default()
            .with_timeout(Duration::from_secs(
                self.timeout_secs.unwrap_or(DEFAULT_MEDIA_TIMEOUT_SECS),
            ))
            .with_retry(retry)
            .with_rate_limit(rate_limit(
                self.per_second,
                self.per_hour,
                self.per_day,
                self.bytes_per_hour,
                self.bytes_per_day,
            ))
            .with_send_token(
                self.send_token
                    .as_deref()
                    .and_then(SendToken::parse)
                    .unwrap_or_default(),
            )
    }
}

/// `percent` of each limit in `limit`, rounded down but at least 1; 100 or more leaves it whole
pub(crate) fn share_of(limit: &RateLimit, percent: u8) -> RateLimit {
    if percent >= 100 {
        return limit.clone();
    }
    let p = u128::from(percent);
    // Below `n`, so it fits back into the type it came from
    let cut = |n: u64| ((u128::from(n) * p / 100) as u64).max(1);
    let cut32 = |n: u32| cut(u64::from(n)) as u32;
    RateLimit {
        per_second: limit.per_second.map(cut32),
        per_hour: limit.per_hour.map(cut32),
        per_day: limit.per_day.map(cut32),
        bytes_per_hour: limit.bytes_per_hour.map(cut),
        bytes_per_day: limit.bytes_per_day.map(cut),
    }
}

fn rate_limit(
    per_second: Option<u32>,
    per_hour: Option<u32>,
    per_day: Option<u32>,
    bytes_per_hour: Option<u64>,
    bytes_per_day: Option<u64>,
) -> RateLimit {
    let mut rate = RateLimit::default();
    if let Some(n) = per_second {
        rate = rate.per_second(n);
    }
    if let Some(n) = per_hour {
        rate = rate.per_hour(n);
    }
    if let Some(n) = per_day {
        rate = rate.per_day(n);
    }
    if let Some(n) = bytes_per_hour {
        rate = rate.bytes_per_hour(n);
    }
    if let Some(n) = bytes_per_day {
        rate = rate.bytes_per_day(n);
    }
    rate
}

/// What one configuration file holds: the top-level settings it sets and its servers
struct Part {
    max_concurrent_servers: Option<usize>,
    webhook_url_env: Option<String>,
    servers: Vec<ServerConfig>,
}

impl Config {
    /// Read and parse a configuration file
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, IngestError> {
        Self::parse(&read_text(path.as_ref())?)
    }

    /// Read a configuration file, or every `*.toml` file of a directory as one configuration
    ///
    /// A directory lets each server live in its own file (`servers.d/mls-a.toml`, ...), so
    /// feeds are added and removed by adding and removing files. Files are read in name order;
    /// other files (and `*.toml` files in subdirectories) are ignored. The top-level settings
    /// (`max_concurrent_servers`, `webhook_url_env`) may be set in one file only, server names
    /// must be unique across files, and errors name the file. The directory needs at least one
    /// server in all.
    ///
    /// # Errors
    ///
    /// [`IngestError`] when a file cannot be read or parsed, a setting is repeated across files,
    /// or no server is configured.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, IngestError> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Self::from_file(path);
        }
        let entries = std::fs::read_dir(path).map_err(|e| IngestError::Read {
            path: path.display().to_string(),
            message: e.kind().to_string(),
        })?;
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort();
        let mut config = Config {
            max_concurrent_servers: DEFAULT_MAX_CONCURRENT,
            webhook_url_env: None,
            servers: Vec::new(),
        };
        let mut set_in: [Option<String>; 2] = [None, None];
        for file in &files {
            let name = file.display().to_string();
            let in_file = |e: IngestError| IngestError::Parse(format!("{name}: {e}"));
            let part = Self::parse_part(&read_text(file)?).map_err(in_file)?;
            for (slot, (key, present)) in set_in.iter_mut().zip([
                (
                    "max_concurrent_servers",
                    part.max_concurrent_servers.is_some(),
                ),
                ("webhook_url_env", part.webhook_url_env.is_some()),
            ]) {
                if !present {
                    continue;
                }
                if let Some(first) = slot {
                    return Err(IngestError::Parse(format!(
                        "{name}: `{key}` is already set in {first}; set it in one file only"
                    )));
                }
                *slot = Some(name.clone());
            }
            if let Some(n) = part.max_concurrent_servers {
                config.max_concurrent_servers = n;
            }
            if part.webhook_url_env.is_some() {
                config.webhook_url_env = part.webhook_url_env;
            }
            for server in part.servers {
                if config.servers.iter().any(|s| s.name == server.name) {
                    return Err(IngestError::Parse(format!(
                        "{name}: server `{}` is also defined in another file",
                        server.name
                    )));
                }
                config.servers.push(server);
            }
        }
        if config.servers.is_empty() {
            return Err(IngestError::Parse(format!(
                "no [[server]] entries in the *.toml files of {}",
                path.display()
            )));
        }
        Ok(config)
    }

    /// Parse configuration text
    ///
    /// Unknown keys, missing required fields, duplicate names and literal secrets are errors that
    /// name the server.
    pub fn parse(text: &str) -> Result<Self, IngestError> {
        let part = Self::parse_part(text)?;
        if part.servers.is_empty() {
            return Err(IngestError::Parse("no [[server]] entries".into()));
        }
        Ok(Config {
            max_concurrent_servers: part
                .max_concurrent_servers
                .unwrap_or(DEFAULT_MAX_CONCURRENT),
            webhook_url_env: part.webhook_url_env,
            servers: part.servers,
        })
    }

    /// Parse one file's text; it may hold no server (a file of top-level settings in a directory)
    fn parse_part(text: &str) -> Result<Part, IngestError> {
        let mut table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| IngestError::Parse(e.to_string()))?;

        let max_concurrent_servers = match table.remove("max_concurrent_servers") {
            None => None,
            Some(toml::Value::Integer(n)) if n >= 1 => Some(n as usize),
            Some(_) => {
                return Err(IngestError::Parse(
                    "max_concurrent_servers must be an integer of at least 1".into(),
                ))
            }
        };
        let webhook_url_env = match table.remove("webhook_url_env") {
            None => None,
            Some(toml::Value::String(name)) if is_env_name(&name) => Some(name),
            Some(_) => {
                return Err(IngestError::Parse(
                    "webhook_url_env must name an environment variable (the URL itself is a secret and is not written in the file)"
                        .into(),
                ))
            }
        };
        let entries = match table.remove("server") {
            Some(toml::Value::Array(a)) => a,
            Some(_) => {
                return Err(IngestError::Parse(
                    "`server` must be an array of tables: use [[server]]".into(),
                ))
            }
            None => Vec::new(),
        };
        if let Some(key) = table.keys().next() {
            return Err(IngestError::Parse(format!("unknown top-level key `{key}`")));
        }
        let mut servers: Vec<ServerConfig> = Vec::new();
        for (i, entry) in entries.into_iter().enumerate() {
            let label = entry
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("#{}", i + 1));
            let fail = |message: String| IngestError::Server {
                server: label.clone(),
                message,
            };

            check_no_literal_secrets(&entry).map_err(&fail)?;
            let server: ServerConfig = entry
                .try_into()
                .map_err(|e: toml::de::Error| fail(clean(e)))?;
            server.validate().map_err(&fail)?;
            if servers.iter().any(|s| s.name == server.name) {
                return Err(fail("duplicate server name".into()));
            }
            servers.push(server);
        }
        Ok(Part {
            max_concurrent_servers,
            webhook_url_env,
            servers,
        })
    }
}

fn read_text(path: &Path) -> Result<String, IngestError> {
    std::fs::read_to_string(path).map_err(|e| IngestError::Read {
        path: path.display().to_string(),
        message: e.kind().to_string(),
    })
}

/// toml error text without the source-snippet decoration, which is noisy once the server is named
fn clean(e: toml::de::Error) -> String {
    e.message().to_owned()
}

fn check_no_literal_secrets(entry: &toml::Value) -> Result<(), String> {
    let Some(auth) = entry.get("auth").and_then(|a| a.as_table()) else {
        return Ok(());
    };
    for key in auth.keys() {
        if SECRET_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "auth.{key}: secrets must not be written in the config file; use `{key}_env` to name the environment variable that holds it, or `{key}_file` for a file"
            ));
        }
    }
    Ok(())
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ServerConfig {
    fn validate(&self) -> Result<(), String> {
        if self
            .deletes
            .flag_field
            .as_deref()
            .is_some_and(|f| f.trim().is_empty())
        {
            return Err("deletes.flag_field must not be empty".into());
        }
        if self
            .deletes
            .max_percent
            .is_some_and(|p| !(1..=100).contains(&p))
        {
            return Err("deletes.max_percent must be between 1 and 100".into());
        }
        if self.deletes.max_deletes == Some(0) {
            return Err("deletes.max_deletes must be at least 1".into());
        }
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err("base_url must start with http:// or https://".into());
        }
        if self.page_size == Some(0) {
            return Err("page_size must be at least 1".into());
        }
        if self.filter.as_deref().is_some_and(|f| f.trim().is_empty()) {
            return Err("filter must not be empty".into());
        }
        if let Some(i) = &self.interval {
            parse_interval(i).map_err(|e| format!("interval: {e}"))?;
        }
        if self.concurrency == Some(0) {
            return Err("concurrency must be at least 1".into());
        }
        for (name, resource) in &self.resources {
            if let Some(i) = &resource.interval {
                parse_interval(i).map_err(|e| format!("resources.{name}.interval: {e}"))?;
            }
            if resource.page_size == Some(0) {
                return Err(format!("resources.{name}.page_size must be at least 1"));
            }
            if resource
                .filter
                .as_deref()
                .is_some_and(|f| f.trim().is_empty())
            {
                return Err(format!("resources.{name}.filter must not be empty"));
            }
            if resource.expand.iter().any(|e| e.trim().is_empty()) {
                return Err(format!("resources.{name}.expand has an empty name"));
            }
        }
        if let Some(t) = &self.media.send_token {
            if SendToken::parse(t).is_none() {
                return Err(format!(
                    "media.send_token `{t}` is not one of never, same_host, always"
                ));
            }
        }
        if self
            .backfill
            .rate_percent
            .is_some_and(|p| !(1..=99).contains(&p))
        {
            return Err("backfill.rate_percent must be between 1 and 99".into());
        }
        if self.media.workers == Some(0) {
            return Err("media.workers must be at least 1".into());
        }
        if self.media.timeout_secs == Some(0) {
            return Err("media.timeout_secs must be at least 1".into());
        }
        if let AuthConfig::ClientCredentials {
            transport,
            token_url,
            ..
        } = &self.auth
        {
            if let Some(t) = transport {
                if CredentialTransport::parse(t).is_none() {
                    return Err(format!(
                        "auth.transport `{t}` is not one of body, header, query"
                    ));
                }
            }
            if !token_url.starts_with("http://") && !token_url.starts_with("https://") {
                return Err("auth.token_url must start with http:// or https://".into());
            }
        }
        self.auth.sources()?;
        self.network.validate()?;
        Ok(())
    }

    /// Whether `resource` should be pulled: not excluded, and included (or `include` is empty)
    pub fn includes(&self, resource: &str) -> bool {
        !self.exclude.iter().any(|r| r == resource)
            && (self.include.is_empty() || self.include.iter().any(|r| r == resource))
    }

    /// The filter for every query on `resource`: the server's and the resource's, joined by `and`
    pub fn filter_for(&self, resource: &str) -> Option<String> {
        let own = self
            .resources
            .get(resource)
            .and_then(|r| r.filter.as_deref());
        match (self.filter.as_deref(), own) {
            (Some(s), Some(r)) => Some(format!("({s}) and ({r})")),
            (Some(f), None) | (None, Some(f)) => Some(f.to_owned()),
            (None, None) => None,
        }
    }

    /// The navigation properties to expand when syncing `resource`
    pub fn expand_for(&self, resource: &str) -> &[String] {
        self.resources
            .get(resource)
            .map_or(&[], |r| r.expand.as_slice())
    }

    /// The page size for `resource`: its own, else the server's
    pub fn page_size_for(&self, resource: &str) -> Option<u32> {
        self.resources
            .get(resource)
            .and_then(|r| r.page_size)
            .or(self.page_size)
    }

    /// The feeds `serve` schedules for this server, with their intervals, in name order
    ///
    /// A resource with its own `interval` is scheduled; so is each resource in `include` without
    /// one, at the server's `interval`. A resource the server does not [`include`](Self::includes)
    /// is never scheduled, nor is a [paused](Self::is_paused) one (see
    /// [`paused_feeds`](Self::paused_feeds)). The result is empty when nothing has an interval.
    /// Intervals were checked when the file was parsed.
    pub fn feeds(&self) -> Vec<(String, std::time::Duration)> {
        let mut feeds = self.all_feeds();
        feeds.retain(|(name, _)| !self.is_paused(name));
        feeds
    }

    /// The feeds [`feeds`](Self::feeds) leaves out because they are paused, with their intervals
    pub fn paused_feeds(&self) -> Vec<(String, std::time::Duration)> {
        let mut feeds = self.all_feeds();
        feeds.retain(|(name, _)| self.is_paused(name));
        feeds
    }

    /// Whether `resource` is paused: the server is, or the resource's own `paused` is set
    pub fn is_paused(&self, resource: &str) -> bool {
        self.paused || self.resources.get(resource).is_some_and(|r| r.paused)
    }

    fn all_feeds(&self) -> Vec<(String, std::time::Duration)> {
        let default = self
            .interval
            .as_deref()
            .and_then(|i| parse_interval(i).ok());
        let mut feeds: BTreeMap<String, std::time::Duration> = BTreeMap::new();
        for name in &self.include {
            if let Some(d) = default {
                feeds.insert(name.clone(), d);
            }
        }
        for (name, r) in &self.resources {
            if let Some(d) = r.interval.as_deref().and_then(|i| parse_interval(i).ok()) {
                feeds.insert(name.clone(), d);
            }
        }
        feeds.retain(|name, _| self.includes(name));
        feeds.into_iter().collect()
    }

    /// Build the [`ClientConfig`], reading secrets from the process environment and secret files
    pub fn client_config(&self) -> Result<ClientConfig, IngestError> {
        self.client_config_with(|name| std::env::var(name).ok())
    }

    /// Like [`client_config`](Self::client_config) with the variable lookup supplied by the caller
    ///
    /// Secrets named by `*_file` are read from their files.
    pub fn client_config_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<ClientConfig, IngestError> {
        self.client_config_from(&self.read_secrets(&lookup)?)
    }

    /// Read every secret of this server from its source now
    pub(crate) fn read_secrets(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Secrets, IngestError> {
        let fail = |message| IngestError::Server {
            server: self.name.clone(),
            message,
        };
        let mut sources = self.auth.sources().map_err(fail)?;
        sources.extend(self.network.proxy_source().map_err(fail)?);
        sources
            .iter()
            .map(|s| s.read(&self.name, lookup))
            .collect::<Result<Vec<_>, _>>()
            .map(Secrets)
    }

    /// Build the [`ClientConfig`] with secrets already read by [`read_secrets`](Self::read_secrets)
    pub(crate) fn client_config_from(
        &self,
        secrets: &Secrets,
    ) -> Result<ClientConfig, IngestError> {
        let secret = |i: usize| {
            secrets
                .0
                .get(i)
                .cloned()
                .ok_or_else(|| IngestError::Server {
                    server: self.name.clone(),
                    message: "a secret is missing".into(),
                })
        };
        let auth_count = self.auth.sources().map(|s| s.len()).unwrap_or(0);
        let proxy = match self.network.proxy_source() {
            Ok(Some(_)) => Some(secret(auth_count)?),
            _ => None,
        };
        let auth = match &self.auth {
            AuthConfig::Bearer { .. } => Auth::Bearer(secret(0)?),
            AuthConfig::ClientCredentials {
                token_url,
                scope,
                transport,
                ..
            } => {
                let mut c = ClientCredentials::new(token_url, secret(0)?, secret(1)?);
                if let Some(scope) = scope {
                    c = c.with_scope(scope);
                }
                if let Some(t) = transport.as_deref().and_then(CredentialTransport::parse) {
                    c = c.with_transport(t);
                }
                Auth::ClientCredentials(c)
            }
        };

        let mut config = ClientConfig::with_auth(&self.base_url, auth).with_timeout(
            Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        );
        if let Some(id) = &self.dataset_id {
            config = config.with_dataset_id(id);
        }
        config = config.with_network(self.network.options(proxy).map_err(|message| {
            IngestError::Server {
                server: self.name.clone(),
                message,
            }
        })?);

        let l = &self.limits;
        let mut retry = RetryPolicy::new(l.max_retries.unwrap_or(DEFAULT_MAX_RETRIES));
        let defaults = RetryPolicy::default();
        retry = retry.with_backoff(
            l.initial_backoff_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.initial_backoff),
            l.max_backoff_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.max_backoff),
        );
        config = config.with_retry(retry);

        Ok(config.with_rate_limit(rate_limit(
            l.per_second,
            l.per_hour,
            l.per_day,
            l.bytes_per_hour,
            l.bytes_per_day,
        )))
    }

    /// Like [`client_config_from`](Self::client_config_from), with each rate limit cut to
    /// `percent` of the configured one (at least 1); 100 leaves them as they are
    pub(crate) fn client_config_share(
        &self,
        secrets: &Secrets,
        percent: u8,
    ) -> Result<ClientConfig, IngestError> {
        let config = self.client_config_from(secrets)?;
        let limit = share_of(&config.rate_limit, percent);
        Ok(config.with_rate_limit(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::{share_of, RateLimit};

    #[test]
    fn shares_round_down_but_keep_one() {
        let limit = RateLimit {
            per_second: Some(3),
            per_hour: Some(7200),
            per_day: None,
            bytes_per_hour: Some(1),
            bytes_per_day: Some(u64::MAX),
        };
        let half = share_of(&limit, 50);
        assert_eq!(half.per_second, Some(1));
        assert_eq!(half.per_hour, Some(3600));
        assert_eq!(half.per_day, None);
        assert_eq!(half.bytes_per_hour, Some(1));
        assert_eq!(half.bytes_per_day, Some(u64::MAX / 2));
        assert_eq!(share_of(&limit, 10).per_second, Some(1));
        assert_eq!(share_of(&limit, 100), limit);
    }
}
