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
    assert_eq!(one.rate_limit.per_day, Some(40_000));
    assert_eq!(one.rate_limit.bytes_per_hour, Some(4_000_000_000));
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

#[test]
fn media_settings_build_a_download_config() {
    use reso_client::SendToken;
    let config = Config::parse(EXAMPLE).unwrap();
    let two = config.servers[1].media.download_config();
    assert_eq!(two.rate_limit.per_second, Some(4));
    let media = "[server.media]\nper_day = 9\nbytes_per_hour = 7\n";
    let text = EXAMPLE.replace("[server.media]\n", media);
    let again = Config::parse(&text).unwrap();
    assert_eq!(
        again.servers[1].media.download_config().rate_limit.per_day,
        Some(9)
    );
    assert_eq!(two.timeout, std::time::Duration::from_secs(300));
    assert_eq!(two.send_token, SendToken::SameHost);
    assert_eq!(two.retry.max_retries, 5, "the ingest default applies");
    // A server without [server.media] gets the defaults
    let one = config.servers[0].media.download_config();
    assert_eq!(one.rate_limit, reso_client::RateLimit::default());
    assert_eq!(one.timeout, std::time::Duration::from_secs(120));
}

#[test]
fn bad_send_token_is_rejected() {
    let text = "[[server]]\nname = \"x\"\nbase_url = \"https://a\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n[server.media]\nsend_token = \"sometimes\"\n";
    let (server, message) = server_error(text);
    assert_eq!(server, "x");
    assert!(message.contains("media.send_token"), "{message}");
}

#[test]
fn more_bad_values_are_rejected() {
    let bearer = "[[server]]\nname = \"a\"\nbase_url = \"https://a.example\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n";
    let (_, message) = server_error(&bearer.replace("name = \"a\"", "name = \" \""));
    assert!(message.contains("name"), "{message}");
    let (_, message) = server_error(&format!("{bearer}[server.media]\ntimeout_secs = 0\n"));
    assert!(message.contains("media.timeout_secs"), "{message}");

    let oauth = "[[server]]\nname = \"b\"\nbase_url = \"https://b.example\"\n[server.auth]\ntype = \"client_credentials\"\ntoken_url = \"https://b.example/token\"\nclient_id_env = \"I\"\nclient_secret_env = \"S\"\n";
    Config::parse(oauth).unwrap();
    let (_, message) = server_error(&format!("{oauth}transport = \"pigeon\"\n"));
    assert!(message.contains("auth.transport"), "{message}");
    let (_, message) = server_error(&oauth.replace("https://b.example/token", "b.example/token"));
    assert!(message.contains("auth.token_url"), "{message}");
}

#[test]
fn server_must_be_an_array_of_tables() {
    let err = Config::parse("server = 1\n").unwrap_err();
    assert!(
        matches!(err, IngestError::Parse(ref m) if m.contains("[[server]]")),
        "{err}"
    );
}

#[test]
fn unreadable_config_file_is_a_read_error() {
    let path =
        std::env::temp_dir().join(format!("reso-ingest-no-such-{}.toml", std::process::id()));
    let err = Config::from_file(&path).unwrap_err();
    assert!(matches!(err, IngestError::Read { .. }), "{err}");
}

#[test]
fn credential_transport_and_scope_reach_the_client_config() {
    let text = "[[server]]\nname = \"b\"\nbase_url = \"https://b.example\"\n[server.auth]\ntype = \"client_credentials\"\ntoken_url = \"https://b.example/token\"\nclient_id_env = \"I\"\nclient_secret_env = \"S\"\nscope = \"api\"\ntransport = \"header\"\n";
    let config = Config::parse(text).unwrap();
    let client = config.servers[0]
        .client_config_with(|_| Some("x".into()))
        .unwrap();
    let Auth::ClientCredentials(c) = client.auth else {
        panic!("expected client credentials");
    };
    assert_eq!(c.scope.as_deref(), Some("api"));
    assert_eq!(c.transport, reso_client::CredentialTransport::BasicHeader);
}

const FILTERED: &str = r#"
[[server]]
name = "grid"
base_url = "https://api.grid.example/v2"
page_size = 5000
filter = "OriginatingSystemName eq 'mls'"
[server.resources.Property]
expand = ["Media", "Rooms"]
page_size = 1000
filter = "PropertyType eq 'Residential'"
[server.resources.Member]
expand = ["Media"]
[server.auth]
type = "bearer"
token_env = "GRID_TOKEN"
"#;

#[test]
fn server_and_resource_filters_combine_and_resource_settings_win() {
    let config = Config::parse(FILTERED).unwrap();
    let s = &config.servers[0];
    assert_eq!(
        s.filter_for("Property").as_deref(),
        Some("(OriginatingSystemName eq 'mls') and (PropertyType eq 'Residential')")
    );
    assert_eq!(
        s.filter_for("Office").as_deref(),
        Some("OriginatingSystemName eq 'mls'")
    );
    assert_eq!(s.expand_for("Property"), ["Media", "Rooms"]);
    assert_eq!(s.expand_for("Member"), ["Media"]);
    assert!(s.expand_for("Office").is_empty());
    assert_eq!(s.page_size_for("Property"), Some(1000));
    assert_eq!(s.page_size_for("Member"), Some(5000));

    let only_resource = FILTERED.replace("filter = \"OriginatingSystemName eq 'mls'\"\n", "");
    let config = Config::parse(&only_resource).unwrap();
    assert_eq!(
        config.servers[0].filter_for("Property").as_deref(),
        Some("PropertyType eq 'Residential'")
    );
    assert_eq!(config.servers[0].filter_for("Office"), None);
}

#[test]
fn bad_filter_and_resource_settings_name_the_server() {
    for (from, to, expected) in [
        (
            "page_size = 1000",
            "page_size = 0",
            "resources.Property.page_size must be at least 1",
        ),
        (
            "filter = \"PropertyType eq 'Residential'\"",
            "filter = \" \"",
            "resources.Property.filter must not be empty",
        ),
        (
            "filter = \"OriginatingSystemName eq 'mls'\"",
            "filter = \"\"",
            "filter must not be empty",
        ),
        (
            "expand = [\"Media\"]\n",
            "expand = [\"\"]\n",
            "resources.Member.expand has an empty name",
        ),
        (
            "expand = [\"Media\"]\n",
            "expandd = [\"Media\"]\n",
            "expandd",
        ),
    ] {
        let text = FILTERED.replace(from, to);
        assert_ne!(text, FILTERED, "{from}");
        let (server, message) = server_error(&text);
        assert_eq!(server, "grid");
        assert!(message.contains(expected), "{message}");
    }
}

#[test]
fn flag_field_deletes_parse_and_an_empty_one_is_rejected() {
    let one = "[[server]]\nname = \"a\"\nbase_url = \"https://a.example\"\nDELETES\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n";
    let config =
        Config::parse(&one.replace("DELETES", "deletes = { flag_field = \"MlgCanView\" }"))
            .unwrap();
    assert_eq!(
        config.servers[0].deletes.flag_field.as_deref(),
        Some("MlgCanView")
    );
    assert_eq!(
        Config::parse(&one.replace("DELETES", "")).unwrap().servers[0]
            .deletes
            .flag_field,
        None
    );

    let (server, message) =
        server_error(&one.replace("DELETES", "deletes = { flag_field = \" \" }"));
    assert_eq!(server, "a");
    assert!(message.contains("deletes.flag_field"), "{message}");
    // A misspelt key is refused rather than ignored
    let (_, message) = server_error(&one.replace("DELETES", "deletes = { flag = \"MlgCanView\" }"));
    assert!(message.contains("flag"), "{message}");
}
