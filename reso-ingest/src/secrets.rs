//! A server's client, rebuilt when its secrets change (rotation without a restart, #92)
//!
//! `reso-client` takes credentials by value, so a rotated secret needs a new client. A
//! [`ClientHandle`] reads the server's secret sources (`*_env`, `*_file`) again each time a sync
//! asks it for the client, before each resource, and builds a new client only when a value
//! changed. After an authentication failure (a 401, or a failed token request) the sync asks
//! [`ClientHandle::renew`] once: when the secrets changed (or another task sharing the handle
//! already built a newer client) it retries the resource with the new client, otherwise the
//! failure stands.
//!
//! A new client starts with an empty rate-limit window and fresh counters; rotation is rare, so
//! this is accepted. Secret values are compared in memory and never logged.

use std::sync::{Arc, Mutex, PoisonError};

use reso_client::ResoClient;

use crate::config::{Secrets, ServerConfig};
use crate::error::IngestError;

type Lookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// One server's client, re-read from its secret sources before use
///
/// # Examples
///
/// ```
/// # use reso_ingest::{ClientHandle, Config};
/// let file = std::env::temp_dir().join(format!("reso-ingest-doc-token-{}", std::process::id()));
/// std::fs::write(&file, "token-1\n")?;
/// let config = Config::parse(&format!(r#"
/// [[server]]
/// name = "demo"
/// base_url = "https://api.mls.example/odata"
///
/// [server.auth]
/// type = "bearer"
/// token_file = "{}"
/// "#, file.display()))?;
///
/// let handle = ClientHandle::new(&config.servers[0])?;
/// let first = handle.client();
/// std::fs::write(&file, "token-2\n")?;   // rotated
/// assert!(!std::sync::Arc::ptr_eq(&first, &handle.client()));
/// # std::fs::remove_file(&file)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ClientHandle {
    server: ServerConfig,
    lookup: Arc<Lookup>,
    current: Mutex<Current>,
}

struct Current {
    client: Arc<ResoClient>,
    secrets: Secrets,
}

impl std::fmt::Debug for ClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandle")
            .field("server", &self.server.name)
            .finish_non_exhaustive()
    }
}

impl ClientHandle {
    /// Read the secrets from their sources (environment variables from the process) and build the client
    ///
    /// # Errors
    ///
    /// [`IngestError`] when a secret cannot be read or the client cannot be built.
    pub fn new(server: &ServerConfig) -> Result<Self, IngestError> {
        Self::with_lookup(server, |name| std::env::var(name).ok())
    }

    /// Like [`new`](Self::new) with the environment variable lookup supplied by the caller
    pub fn with_lookup(
        server: &ServerConfig,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self, IngestError> {
        let lookup: Arc<Lookup> = Arc::new(lookup);
        let secrets = server.read_secrets(lookup.as_ref())?;
        let client = build(server, &secrets)?;
        Ok(Self {
            server: server.clone(),
            lookup,
            current: Mutex::new(Current { client, secrets }),
        })
    }

    /// The client to use now: the secrets are read again, and a new client is built when one changed
    ///
    /// When a source cannot be read (a file being replaced, say), or the new client cannot be
    /// built, the current client is kept and a warning is logged.
    pub fn client(&self) -> Arc<ResoClient> {
        self.refresh().0
    }

    /// After `failed` hit an authentication failure: a newer client when the secrets changed (now,
    /// or since `failed` was handed out, by another task sharing this handle), else `None`
    ///
    /// Callers retry once with the client returned.
    pub fn renew(&self, failed: &Arc<ResoClient>) -> Option<Arc<ResoClient>> {
        match self.refresh() {
            (client, changed) if changed || !Arc::ptr_eq(&client, failed) => Some(client),
            _ => {
                tracing::warn!(
                    server = %self.server.name,
                    "authentication failed and the secrets have not changed"
                );
                None
            }
        }
    }

    /// Read the secrets; rebuild when they changed. Returns the client and whether it is new.
    fn refresh(&self) -> (Arc<ResoClient>, bool) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        let secrets = match self.server.read_secrets(self.lookup.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(server = %self.server.name, error = %e, "cannot read the secrets again; keeping the current client");
                return (Arc::clone(&current.client), false);
            }
        };
        if secrets == current.secrets {
            return (Arc::clone(&current.client), false);
        }
        match build(&self.server, &secrets) {
            Ok(client) => {
                tracing::info!(server = %self.server.name, "secrets changed: using a new client");
                *current = Current {
                    client: Arc::clone(&client),
                    secrets,
                };
                (client, true)
            }
            Err(e) => {
                tracing::warn!(server = %self.server.name, error = %e, "cannot build a client with the changed secrets; keeping the current one");
                (Arc::clone(&current.client), false)
            }
        }
    }
}

fn build(server: &ServerConfig, secrets: &Secrets) -> Result<Arc<ResoClient>, IngestError> {
    let config = server.client_config_from(secrets)?;
    ResoClient::with_config(config)
        .map(Arc::new)
        .map_err(|e| IngestError::Server {
            server: server.name.clone(),
            message: e.to_string(),
        })
}

/// Whether an error message from a sync is an authentication failure: a 401 from the server, or a
/// token request that failed
pub(crate) fn is_auth_failure(message: &str) -> bool {
    message.contains("Unauthorized (401)") || message.contains("Token request failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failures_are_recognised() {
        let unauthorized = reso_client::ResoError::Unauthorized {
            message: "bad token".into(),
            status_code: 401,
        }
        .to_string();
        assert!(is_auth_failure(&unauthorized));
        let token = reso_client::ResoError::TokenRequest("token endpoint returned 401".into());
        assert!(is_auth_failure(&format!("cannot read $metadata: {token}")));
        let other = reso_client::ResoError::Parse("oops".into()).to_string();
        assert!(!is_auth_failure(&other));
    }
}
