// src/network.rs

//! Network settings: proxy, extra CA certificates, User-Agent and connection pool
//!
//! Corporate networks and MLS vendors set rules a client has to meet: traffic through a proxy,
//! a TLS-inspecting proxy whose CA is not in the public roots, a User-Agent the vendor can
//! recognise. [`NetworkOptions`] holds these settings; set them on a
//! [`ClientConfig`](crate::ClientConfig) with
//! [`with_network`](crate::ClientConfig::with_network). They apply to API requests, token
//! requests and the [`Downloader`](crate::Downloader)'s file requests alike.
//!
//! # Proxy
//!
//! By default the `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` environment variables
//! are honoured (lower-case names too). An explicit [`proxy`](NetworkOptions::with_proxy)
//! replaces them; [`without_system_proxy`](NetworkOptions::without_system_proxy) turns them off.
//! A proxy URL may carry `user:password@` for basic proxy authentication; `Debug` output and
//! error messages leave the credentials out.
//!
//! # Fixed egress IP
//!
//! Many MLS vendors only accept requests from IP addresses registered during onboarding. The
//! client has no say in its source address: run it behind a NAT gateway or a forward proxy with a
//! fixed public IP (see the README's deployment notes) and point [`proxy`](NetworkOptions::with_proxy)
//! at the proxy if that is the route.
//!
//! # Examples
//!
//! ```
//! # use reso_client::{ClientConfig, NetworkOptions};
//! # use std::time::Duration;
//! let network = NetworkOptions::default()
//!     .with_proxy("http://proxy.corp.example:3128")
//!     .with_no_proxy("localhost,.corp.example")
//!     .with_user_agent("acme-ingest/1.0 (data@acme.example)")
//!     .with_pool_max_idle_per_host(8)
//!     .with_pool_idle_timeout(Duration::from_secs(60));
//! let config = ClientConfig::new("https://api.mls.com/odata", "token").with_network(network);
//! ```

use std::path::Path;
use std::time::Duration;

use reqwest::{Certificate, ClientBuilder, NoProxy, Proxy, Url};

use crate::error::{ResoError, Result};

/// The User-Agent sent when none is set: `reso-client/<version>`
pub const DEFAULT_USER_AGENT: &str = concat!("reso-client/", env!("CARGO_PKG_VERSION"));

/// Proxy, CA, User-Agent and connection-pool settings; see the [module docs](self)
///
/// The default honours the proxy environment variables, trusts the bundled public roots only,
/// sends [`DEFAULT_USER_AGENT`] and keeps reqwest's pool defaults (no cap on idle connections,
/// 90 seconds idle timeout).
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkOptions {
    /// Proxy for every request (`http://`, `https://`); may hold `user:password@`
    pub proxy: Option<String>,
    /// Hosts that bypass [`proxy`](Self::proxy): a comma-separated list in the `NO_PROXY` format
    /// (`localhost,.example.com,10.0.0.0/8`)
    pub no_proxy: Option<String>,
    /// Honour `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` when no
    /// [`proxy`](Self::proxy) is set (default `true`)
    pub system_proxy: bool,
    /// Extra trusted CA certificates, PEM encoded (each entry may hold several certificates)
    pub ca_certificates: Vec<Vec<u8>>,
    /// The `User-Agent` header (default [`DEFAULT_USER_AGENT`])
    pub user_agent: String,
    /// Most idle connections kept per host (default: no cap)
    pub pool_max_idle_per_host: Option<usize>,
    /// How long an idle connection is kept (default: 90 seconds)
    pub pool_idle_timeout: Option<Duration>,
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            proxy: None,
            no_proxy: None,
            system_proxy: true,
            ca_certificates: Vec::new(),
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            pool_max_idle_per_host: None,
            pool_idle_timeout: None,
        }
    }
}

impl std::fmt::Debug for NetworkOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkOptions")
            .field("proxy", &self.proxy.as_deref().map(redact_proxy))
            .field("no_proxy", &self.no_proxy)
            .field("system_proxy", &self.system_proxy)
            .field("ca_certificates", &self.ca_certificates.len())
            .field("user_agent", &self.user_agent)
            .field("pool_max_idle_per_host", &self.pool_max_idle_per_host)
            .field("pool_idle_timeout", &self.pool_idle_timeout)
            .finish()
    }
}

/// A proxy URL with any user name and password replaced by `***`
///
/// # Examples
///
/// ```
/// # use reso_client::network::redact_proxy;
/// assert_eq!(redact_proxy("http://me:pw@proxy:3128"), "http://***@proxy:3128/");
/// assert_eq!(redact_proxy("http://proxy:3128"), "http://proxy:3128");
/// ```
pub fn redact_proxy(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_password(None);
            let _ = u.set_username("***");
            u.to_string()
        }
        Ok(_) => url.to_owned(),
        // An unparsable URL may still hold credentials: show none of it
        Err(_) => "<invalid proxy URL>".to_owned(),
    }
}

impl NetworkOptions {
    /// Send every request through `url` (`http://host:port`, optionally `http://user:password@host:port`)
    ///
    /// This replaces the proxy environment variables.
    pub fn with_proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    /// Hosts that bypass the explicit proxy, in the `NO_PROXY` format (`localhost,.example.com,10.0.0.0/8`)
    pub fn with_no_proxy(mut self, hosts: impl Into<String>) -> Self {
        self.no_proxy = Some(hosts.into());
        self
    }

    /// Ignore `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`; connect directly unless
    /// [`with_proxy`](Self::with_proxy) is set
    pub fn without_system_proxy(mut self) -> Self {
        self.system_proxy = false;
        self
    }

    /// Trust the CA certificates in `pem` as well as the public roots (for example a corporate
    /// TLS-inspecting proxy's CA, or a vendor's private CA)
    ///
    /// The PEM is checked when the client is built.
    pub fn with_ca_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.ca_certificates.push(pem.into());
        self
    }

    /// Like [`with_ca_pem`](Self::with_ca_pem), reading the PEM from `path`
    ///
    /// # Errors
    ///
    /// [`ResoError::Config`] when the file cannot be read.
    pub fn with_ca_file(self, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let pem = std::fs::read(path).map_err(|e| {
            ResoError::Config(format!(
                "cannot read CA file {}: {}",
                path.display(),
                e.kind()
            ))
        })?;
        Ok(self.with_ca_pem(pem))
    }

    /// Send `agent` as the `User-Agent` header; vendors often ask for one naming the product and a contact
    pub fn with_user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = agent.into();
        self
    }

    /// Keep at most `max` idle connections per host (0 closes each connection after use)
    pub fn with_pool_max_idle_per_host(mut self, max: usize) -> Self {
        self.pool_max_idle_per_host = Some(max);
        self
    }

    /// Close idle connections after `timeout`
    pub fn with_pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.pool_idle_timeout = Some(timeout);
        self
    }

    /// Apply these settings to a reqwest client builder
    ///
    /// # Errors
    ///
    /// [`ResoError::Config`] for a proxy URL that does not parse (the message leaves the URL
    /// out), a `User-Agent` that is not a valid header value, or a CA entry with no valid certificate.
    pub(crate) fn apply(&self, mut builder: ClientBuilder) -> Result<ClientBuilder> {
        if let Some(url) = &self.proxy {
            let mut proxy = Proxy::all(url.as_str()).map_err(|_| {
                ResoError::Config(format!("proxy URL is not valid: {}", redact_proxy(url)))
            })?;
            if let Some(hosts) = &self.no_proxy {
                proxy = proxy.no_proxy(NoProxy::from_string(hosts));
            }
            builder = builder.proxy(proxy);
        } else if !self.system_proxy {
            builder = builder.no_proxy();
        }

        for (i, pem) in self.ca_certificates.iter().enumerate() {
            let certs = Certificate::from_pem_bundle(pem).map_err(|e| {
                ResoError::Config(format!("CA certificate #{} is not valid PEM: {}", i + 1, e))
            })?;
            if certs.is_empty() {
                return Err(ResoError::Config(format!(
                    "CA certificate #{} holds no certificate",
                    i + 1
                )));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }

        let agent = reqwest::header::HeaderValue::from_str(&self.user_agent)
            .map_err(|_| ResoError::Config("user agent is not a valid header value".to_owned()))?;
        builder = builder.user_agent(agent);

        if let Some(max) = self.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(max);
        }
        if let Some(timeout) = self.pool_idle_timeout {
            builder = builder.pool_idle_timeout(timeout);
        }
        Ok(builder)
    }

    /// Build a reqwest client with these settings and `timeout`
    pub(crate) fn client(&self, timeout: Duration) -> Result<reqwest::Client> {
        self.apply(reqwest::Client::builder().timeout(timeout))?
            .build()
            .map_err(|e| ResoError::Config(format!("Failed to create HTTP client: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_proxy_credentials() {
        let options = NetworkOptions::default().with_proxy("http://alice:s3cret@proxy:3128");
        let text = format!("{:?}", options);
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        assert!(text.contains("proxy:3128"), "{text}");
    }

    #[test]
    fn bad_proxy_url_is_a_config_error_without_the_url() {
        let options = NetworkOptions::default().with_proxy("::not a url s3cret");
        let error = options.client(Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, ResoError::Config(_)));
        assert!(!error.to_string().contains("s3cret"), "{error}");
    }

    #[test]
    fn bad_ca_pem_is_a_config_error() {
        let options = NetworkOptions::default().with_ca_pem("not a certificate");
        let error = options.client(Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, ResoError::Config(_)), "{error}");
    }

    #[test]
    fn bad_user_agent_is_a_config_error() {
        let options = NetworkOptions::default().with_user_agent("line\nbreak");
        let error = options.client(Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, ResoError::Config(_)), "{error}");
    }

    #[test]
    fn missing_ca_file_is_a_config_error() {
        let error = NetworkOptions::default()
            .with_ca_file("/nonexistent/ca.pem")
            .unwrap_err();
        assert!(matches!(error, ResoError::Config(_)), "{error}");
    }
}
