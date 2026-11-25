//! The TOML configuration: many servers, each with auth, limits and resource filters

use std::path::Path;
use std::time::Duration;

use reso_client::{
    Auth, ClientConfig, ClientCredentials, CredentialTransport, DownloadConfig, RateLimit,
    RetryPolicy, SendToken,
};
use serde::Deserialize;

use crate::error::IngestError;

/// Keys that would hold a secret in the file itself; they are rejected with a pointer to the `_env` form
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
///
/// [[server]]
/// name = "bridge"
/// base_url = "https://api.bridgedataoutput.com/api/v2/OData"
/// dataset_id = "actris_ref"           # optional
/// page_size = 1000                    # optional
/// include = ["Property", "Member"]    # optional; empty = every resource
/// exclude = ["Media"]                 # optional; wins over include
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
/// ```
///
/// See `reso-ingest/servers.example.toml` for a complete file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Most servers running at the same time (default 4)
    pub max_concurrent_servers: usize,
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
    /// How to authenticate
    pub auth: AuthConfig,
    /// Retry and rate limits
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Media file downloads: their own limits, and where the token goes
    #[serde(default)]
    pub media: MediaConfig,
}

/// Authentication settings; secrets are named by environment variable, never written in the file
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// A static bearer token read from `token_env`
    Bearer {
        /// Variable holding the token
        token_env: String,
    },
    /// OAuth2 client credentials
    ClientCredentials {
        /// Token endpoint URL
        token_url: String,
        /// Variable holding the client id
        client_id_env: String,
        /// Variable holding the client secret
        client_secret_env: String,
        /// Optional scope
        #[serde(default)]
        scope: Option<String>,
        /// `body` (default), `header` or `query`
        #[serde(default)]
        transport: Option<String>,
    },
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
    /// File bytes per day
    #[serde(default)]
    pub bytes_per_day: Option<u64>,
    /// Timeout for one file in seconds (default 120)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// `never`, `same_host` (default) or `always`
    #[serde(default)]
    pub send_token: Option<String>,
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

fn rate_limit(
    per_second: Option<u32>,
    per_hour: Option<u32>,
    bytes_per_day: Option<u64>,
) -> RateLimit {
    let mut rate = RateLimit::default();
    if let Some(n) = per_second {
        rate = rate.per_second(n);
    }
    if let Some(n) = per_hour {
        rate = rate.per_hour(n);
    }
    if let Some(n) = bytes_per_day {
        rate = rate.bytes_per_day(n);
    }
    rate
}

impl Config {
    /// Read and parse a configuration file
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, IngestError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| IngestError::Read {
            path: path.display().to_string(),
            message: e.kind().to_string(),
        })?;
        Self::parse(&text)
    }

    /// Parse configuration text
    ///
    /// Unknown keys, missing required fields, duplicate names and literal secrets are errors that
    /// name the server.
    pub fn parse(text: &str) -> Result<Self, IngestError> {
        let mut table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| IngestError::Parse(e.to_string()))?;

        let max_concurrent_servers = match table.remove("max_concurrent_servers") {
            None => DEFAULT_MAX_CONCURRENT,
            Some(toml::Value::Integer(n)) if n >= 1 => n as usize,
            Some(_) => {
                return Err(IngestError::Parse(
                    "max_concurrent_servers must be an integer of at least 1".into(),
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
        if entries.is_empty() {
            return Err(IngestError::Parse("no [[server]] entries".into()));
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
        Ok(Config {
            max_concurrent_servers,
            servers,
        })
    }
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
                "auth.{key}: secrets must not be written in the config file; use `{key}_env` to name the environment variable that holds it"
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
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err("base_url must start with http:// or https://".into());
        }
        if self.page_size == Some(0) {
            return Err("page_size must be at least 1".into());
        }
        if let Some(t) = &self.media.send_token {
            if SendToken::parse(t).is_none() {
                return Err(format!(
                    "media.send_token `{t}` is not one of never, same_host, always"
                ));
            }
        }
        if self.media.timeout_secs == Some(0) {
            return Err("media.timeout_secs must be at least 1".into());
        }
        let env_names: Vec<(&str, &str)> = match &self.auth {
            AuthConfig::Bearer { token_env } => vec![("token_env", token_env)],
            AuthConfig::ClientCredentials {
                client_id_env,
                client_secret_env,
                transport,
                token_url,
                ..
            } => {
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
                vec![
                    ("client_id_env", client_id_env),
                    ("client_secret_env", client_secret_env),
                ]
            }
        };
        for (key, value) in env_names {
            // A pasted secret in an `_env` field would not look like a variable name
            if !is_env_name(value) {
                return Err(format!(
                    "auth.{key} must be an environment variable name (letters, digits, underscore); the value is not echoed because it may be a secret"
                ));
            }
        }
        Ok(())
    }

    /// Whether `resource` should be pulled: not excluded, and included (or `include` is empty)
    pub fn includes(&self, resource: &str) -> bool {
        !self.exclude.iter().any(|r| r == resource)
            && (self.include.is_empty() || self.include.iter().any(|r| r == resource))
    }

    /// Build the [`ClientConfig`], reading secrets from the process environment
    pub fn client_config(&self) -> Result<ClientConfig, IngestError> {
        self.client_config_with(|name| std::env::var(name).ok())
    }

    /// Like [`client_config`](Self::client_config) with the variable lookup supplied by the caller
    pub fn client_config_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<ClientConfig, IngestError> {
        let get = |variable: &str| {
            lookup(variable)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| IngestError::MissingEnv {
                    server: self.name.clone(),
                    variable: variable.to_owned(),
                })
        };
        let auth = match &self.auth {
            AuthConfig::Bearer { token_env } => Auth::Bearer(get(token_env)?),
            AuthConfig::ClientCredentials {
                token_url,
                client_id_env,
                client_secret_env,
                scope,
                transport,
            } => {
                let mut c =
                    ClientCredentials::new(token_url, get(client_id_env)?, get(client_secret_env)?);
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

        Ok(config.with_rate_limit(rate_limit(l.per_second, l.per_hour, l.bytes_per_day)))
    }
}
