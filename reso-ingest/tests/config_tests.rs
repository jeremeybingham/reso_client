use reso_client::Auth;
use reso_ingest::{Config, IngestError};

const EXAMPLE: &str = include_str!("../servers.example.toml");

fn env(name: &str) -> Option<String> {
    match name {
        "MLS_ONE_CLIENT_ID" => Some("the-id".into()),
        "MLS_ONE_CLIENT_SECRET" => Some("the-secret".into()),
        "MLS_TWO_TOKEN" => Some("the-token".into()),
        _ => None,
    }
}

fn server_error(text: &str) -> (String, String) {
    match Config::parse(text) {
        Err(IngestError::Server { server, message }) => (server, message),
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[test]
fn example_config_with_two_servers_parses() {
    let config = Config::parse(EXAMPLE).unwrap();
    assert_eq!(config.max_concurrent_servers, 2);
    assert_eq!(config.servers.len(), 2);
    assert_eq!(config.servers[0].name, "mls-one");
    assert_eq!(config.servers[1].dataset_id.as_deref(), Some("ref_data"));
    assert!(config.servers[0].includes("Property"));
    assert!(!config.servers[0].includes("Media"));
    assert!(!config.servers[0].includes("Other"));
    assert!(config.servers[1].includes("Anything"));
}

#[test]
fn secrets_resolve_from_named_env_variables() {
    let config = Config::parse(EXAMPLE).unwrap();
    let one = config.servers[0].client_config_with(env).unwrap();
    assert!(matches!(one.auth, Auth::ClientCredentials(ref c) if c.client_secret == "the-secret"));
    assert_eq!(one.retry.max_retries, 4);
    assert_eq!(one.rate_limit.per_second, Some(2));
    let two = config.servers[1].client_config_with(env).unwrap();
    assert!(matches!(two.auth, Auth::Bearer(ref t) if t == "the-token"));
    assert_eq!(two.dataset_id.as_deref(), Some("ref_data"));
}

#[test]
fn missing_env_variable_names_server_and_variable() {
    let config = Config::parse(EXAMPLE).unwrap();
    let err = config.servers[1].client_config_with(|_| None).unwrap_err();
    assert_eq!(
        err.to_string(),
        "server `mls-two`: environment variable `MLS_TWO_TOKEN` is not set"
    );
}

#[test]
fn unknown_key_names_the_server() {
    let (server, message) = server_error(
        r#"
[[server]]
name = "a"
base_url = "https://a.example"
colour = "red"
[server.auth]
type = "bearer"
token_env = "T"
"#,
    );
    assert_eq!(server, "a");
    assert!(message.contains("colour"), "{message}");
}

#[test]
fn unknown_key_in_auth_names_the_server() {
    let (server, message) = server_error(
        r#"
[[server]]
name = "a"
base_url = "https://a.example"
[server.auth]
type = "bearer"
token_env = "T"
extra = 1
"#,
    );
    assert_eq!(server, "a");
    assert!(message.contains("extra"), "{message}");
}

#[test]
fn missing_required_field_names_the_server() {
    let (server, message) = server_error(
        r#"
[[server]]
name = "a"
[server.auth]
type = "bearer"
token_env = "T"
"#,
    );
    assert_eq!(server, "a");
    assert!(message.contains("base_url"), "{message}");

    // No name at all: identified by position
    let (server, message) = server_error("[[server]]\nbase_url = \"https://a.example\"\n");
    assert_eq!(server, "#1");
    assert!(
        message.contains("name") || message.contains("auth"),
        "{message}"
    );
}

#[test]
fn literal_secret_is_rejected_without_echoing_it() {
    for (key, value) in [("token", "abc123SECRET"), ("client_secret", "abc123SECRET")] {
        let text = format!(
            "[[server]]\nname = \"a\"\nbase_url = \"https://a.example\"\n[server.auth]\ntype = \"bearer\"\n{key} = \"{value}\"\n"
        );
        let (server, message) = server_error(&text);
        assert_eq!(server, "a");
        assert!(message.contains(&format!("{key}_env")), "{message}");
        assert!(!message.contains(value), "{message}");
    }
}

#[test]
fn secret_pasted_into_an_env_field_is_rejected_without_echoing_it() {
    let (_, message) = server_error(
        r#"
[[server]]
name = "a"
base_url = "https://a.example"
[server.auth]
type = "bearer"
token_env = "sk-live/abc 123"
"#,
    );
    assert!(message.contains("token_env"), "{message}");
    assert!(!message.contains("sk-live"), "{message}");
}

#[test]
fn duplicate_names_and_bad_values_are_rejected() {
    let one = "[[server]]\nname = \"a\"\nbase_url = \"https://a.example\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n";
    let (server, message) = server_error(&format!("{one}{one}"));
    assert_eq!(server, "a");
    assert!(message.contains("duplicate"));

    let (_, message) = server_error(&one.replace("https://a.example", "ftp://a"));
    assert!(message.contains("base_url"));
    let (_, message) = server_error(&one.replace("base_url", "page_size = 0\nbase_url"));
    assert!(message.contains("page_size"));
}

#[test]
fn top_level_problems_are_parse_errors() {
    assert!(matches!(
        Config::parse("[[server]"),
        Err(IngestError::Parse(_))
    ));
    assert!(matches!(Config::parse(""), Err(IngestError::Parse(_))));
    assert!(matches!(
        Config::parse("colour = 1\n"),
        Err(IngestError::Parse(_))
    ));
    assert!(matches!(
        Config::parse("max_concurrent_servers = 0\n"),
        Err(IngestError::Parse(_))
    ));
}

#[test]
fn retries_default_to_five_when_not_set() {
    let config = Config::parse(
        r#"
[[server]]
name = "a"
base_url = "https://a.example/odata"
[server.auth]
type = "bearer"
token_env = "SOME_TOKEN"
"#,
    )
    .unwrap();
    std::env::set_var("SOME_TOKEN", "t");
    let client = config.servers[0].client_config().unwrap();
    assert_eq!(client.retry.max_retries, 5);
}
