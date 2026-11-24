use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reso_ingest::{run_servers, Config, ServerConfig};

fn servers(n: usize) -> Vec<ServerConfig> {
    let mut text = String::new();
    for i in 0..n {
        text.push_str(&format!(
            "[[server]]\nname = \"s{i}\"\nbase_url = \"https://s{i}.example\"\n[server.auth]\ntype = \"bearer\"\ntoken_env = \"T\"\n"
        ));
    }
    Config::parse(&text).unwrap().servers
}

#[tokio::test]
async fn failing_server_does_not_stop_the_healthy_one() {
    let reports = run_servers(servers(2), 2, |server| async move {
        if server.name == "s0" {
            Err("boom".to_string())
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(42)
        }
    })
    .await;

    assert_eq!(reports[0].name, "s0");
    assert_eq!(reports[0].result.as_ref().unwrap_err(), "boom");
    assert_eq!(reports[1].name, "s1");
    assert_eq!(reports[1].result.as_ref().unwrap(), &42);
}

#[tokio::test]
async fn panicking_server_is_reported_and_others_complete() {
    let reports = run_servers(servers(3), 3, |server| async move {
        if server.name == "s1" {
            panic!("bad");
        }
        Ok::<_, String>(server.name)
    })
    .await;

    assert_eq!(reports[0].result.as_ref().unwrap(), "s0");
    assert!(reports[1].result.is_err());
    assert_eq!(reports[2].result.as_ref().unwrap(), "s2");
}

#[tokio::test]
async fn concurrency_cap_is_respected() {
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (r, p) = (running.clone(), peak.clone());

    let reports = run_servers(servers(6), 2, move |_| {
        let (running, peak) = (r.clone(), p.clone());
        async move {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            running.fetch_sub(1, Ordering::SeqCst);
            Ok::<_, String>(())
        }
    })
    .await;

    assert!(reports.iter().all(|r| r.result.is_ok()));
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}
