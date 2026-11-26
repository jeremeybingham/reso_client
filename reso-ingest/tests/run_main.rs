//! `run_main`: the log format from `--log-format` or `RESO_LOG_FORMAT`, and the global log
//! subscriber it installs
//!
//! Own binary: `run_main` installs a global subscriber and the test sets an environment variable.

use reso_ingest::cli::run_main;

async fn main_with(args: &[&str]) -> (i32, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_main(&args, &mut out, &mut err).await;
    (code, String::from_utf8(err).unwrap())
}

// One test, in order: the steps share the process's environment and global subscriber
#[tokio::test]
async fn run_main_picks_the_log_format_and_installs_the_subscriber_once() {
    let dir = std::env::temp_dir().join(format!("reso-ingest-run-main-{}", std::process::id()));
    let out = dir.to_str().unwrap();

    // Bad usage stops before any subscriber is installed
    let (code, _) = main_with(&["no-such-command"]).await;
    assert_eq!(code, 2);
    std::env::set_var("RESO_LOG_FORMAT", "xml");
    let (code, err) = main_with(&["status", "--out", out]).await;
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("RESO_LOG_FORMAT") && err.contains("xml"),
        "{err}"
    );

    // A valid variable: the subscriber is installed and the command runs
    std::env::set_var("RESO_LOG_FORMAT", "json");
    let (code, err) = main_with(&["status", "--out", out]).await;
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("already installed"), "{err}");

    // --log-format wins over the variable; a second install is only a warning
    std::env::set_var("RESO_LOG_FORMAT", "xml");
    let (code, err) = main_with(&["--log-format", "text", "status", "--out", out]).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("a log subscriber is already installed"),
        "{err}"
    );

    std::env::remove_var("RESO_LOG_FORMAT");
    let _ = std::fs::remove_dir_all(&dir);
}
