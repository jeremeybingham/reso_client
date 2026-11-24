// Secrets never appear in tracing output (#21).
//
// This test has its own binary: tracing caches per-callsite interest process-wide, so a scoped
// subscriber set in one test races with other tests running on other threads.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use reso_client::{ClientConfig, ClientCredentials, CredentialTransport, QueryBuilder, ResoClient};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SECRET: &str = "the-secret";

/// Answers the token endpoint with token-1, token-2, ... in turn.
struct CountingTokens(Mutex<u32>);

impl Respond for CountingTokens {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": format!("token-{}", n),
            "token_type": "Bearer",
            "expires_in": 3600,
        }))
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Writer<'a>(&'a mut String);

impl Visit for Writer<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = writeln!(self.0, "{}={:?}", field.name(), value);
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        span.record(&mut Writer(&mut self.0.lock().unwrap()));
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        values.record(&mut Writer(&mut self.0.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        event.record(&mut Writer(&mut self.0.lock().unwrap()));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[tokio::test]
async fn secrets_never_appear_in_logs() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(CountingTokens(Mutex::new(0)))
        .mount(&server)
        .await;
    // token-1 is rejected, so the run also covers the 401 renewal path.
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Property"))
        .and(header("authorization", "Bearer token-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "value": [] })))
        .mount(&server)
        .await;

    let creds = ClientCredentials::new(format!("{}/oauth/token", server.uri()), "the-id", SECRET)
        .with_transport(CredentialTransport::Query);
    let client = ResoClient::with_config(ClientConfig::with_auth(server.uri(), creds)).unwrap();
    client
        .execute(&QueryBuilder::new("Property").build().unwrap())
        .await
        .unwrap();

    let logged = capture.0.lock().unwrap().clone();
    assert!(!logged.is_empty(), "expected some log output");
    for secret in [SECRET, "token-1", "token-2"] {
        assert!(!logged.contains(secret), "{} appeared in logs", secret);
    }
}
