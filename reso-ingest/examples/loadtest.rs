//! Load-test harness for `reso-ingest sync` (#90); not part of `cargo test`
//!
//! Starts a fake RESO server in this process (invented data, keyset paging, optional rate limit and
//! injected faults), then runs `reso-ingest sync` against it in a child process (this same binary,
//! `loadtest child ...`), so the child's peak memory can be read and the child can be killed.
//! After each scenario it reads every output line back and checks that each record arrived.
//!
//! ```sh
//! cargo run --release -p reso-ingest --example loadtest -- --records 1000000
//! cargo run --release -p reso-ingest --example loadtest -- --records 100000 --scenario kill
//! ```
//!
//! Scenarios (`--scenario`, default `all`):
//! - `clean`: one full sync, no faults. Every record exactly once.
//! - `faults`: a full sync while the server answers some requests with 503, 429 (`Retry-After: 1`),
//!   a slow page, a connection closed before the response, or a body cut short. A failed sync is
//!   run again (as a scheduler would) until one finishes. Every record at least once.
//! - `kill`: the sync is killed (SIGKILL) `--kills` times at even points of the pull, each time
//!   resumed by a new sync. Every record at least once; repeats at most one page per kill
//!   (checkpoints are saved after each page, README "What it writes").
//! - `deletes`: `sync --deletes` key comparison (#87). A checkpoint at the last record is seeded so
//!   the sync pulls no records and the run is the key pull and diff alone: a baseline run, a run
//!   after every `--gone-every`-th record left the server (exactly those keys must come out as
//!   delete events, once each), and a run with nothing new. Not in `all`.
//! - `backfill`: `sync --backfill --resource Property` (#88) with `per_second = --limit-rps` and
//!   `rate_percent = --rate-percent`, killed once halfway and resumed, while `sync --resource
//!   Member` runs every second against a small feed that grows. Checks every record, the
//!   `##progress` lines and the resumed count, that the backfill keeps to its share of the
//!   requests, and that the Member syncs used the rest and kept finishing. Not in `all`.
//! - `media-index`: the media index alone (#149), in this process, no server: `--records` media
//!   records arrive in pages of `--page-size`; after each page the downloads of the page before
//!   finish and the index is saved, as `sync --media` does. Prints the save time and bytes per
//!   page at each tenth of the run, next to a whole rewrite of the index (the cost of every save
//!   before #149), and checks that a fresh load equals the index in memory. Not in `all`.
//!
//! `LOADTEST_WRAP="valgrind --tool=callgrind ..."` runs each sync child under that command (the
//! fake server stays native); `scripts/profile.sh` uses it for callgrind and timed runs (#162).
//!
//! Linux only for the memory figure (`VmHWM` from `/proc/<pid>/status`). Results print as markdown
//! table rows for `reso-ingest/LOADTEST.md`. Output is deleted after each scenario unless `--keep`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{TcpListener, TcpStream};

/// 2026-01-01T00:00:00Z
const BASE_EPOCH: i64 = 1_767_225_600;
const TOKEN_ENV: &str = "LOADTEST_TOKEN";
/// Command line the sync child runs under (profilers, `time`), see `spawn_sync`
const WRAP_ENV: &str = "LOADTEST_WRAP";

#[derive(Parser, Debug, Clone)]
#[command(about = "Load-test harness for reso-ingest sync (#90)")]
struct Args {
    /// Records the fake server holds
    #[arg(long, default_value_t = 1_000_000)]
    records: u64,
    /// `page_size` in the configuration ($top)
    #[arg(long, default_value_t = 1000)]
    page_size: u64,
    /// Most records the server returns per page, whatever $top asks
    #[arg(long, default_value_t = 1000)]
    server_cap: u64,
    /// Records sharing one ModificationTimestamp (ties the keyset must break)
    #[arg(long, default_value_t = 5)]
    ties: u64,
    /// Bytes of invented remarks text per record (record size)
    #[arg(long, default_value_t = 400)]
    remarks_bytes: usize,
    /// clean, faults, kill or all
    #[arg(long, default_value = "all")]
    scenario: String,
    /// Share of /Property requests that get a fault in the `faults` scenario, in percent
    #[arg(long, default_value_t = 5.0)]
    fault_percent: f64,
    /// Requests per second the server allows before answering 429 (0 = no limit)
    #[arg(long, default_value_t = 0)]
    server_rps: u64,
    /// Kills in the `kill` scenario
    #[arg(long, default_value_t = 3)]
    kills: u64,
    /// Most sync runs per scenario before giving up
    #[arg(long, default_value_t = 30)]
    max_runs: u32,
    /// Where output goes (one subdirectory per scenario)
    #[arg(long, default_value = "target/loadtest")]
    workdir: PathBuf,
    /// Keep each scenario's output instead of deleting it after the check
    #[arg(long)]
    keep: bool,
    /// In the `deletes` scenario, every Nth record leaves the server after the baseline
    #[arg(long, default_value_t = 100)]
    gone_every: u64,
    /// `backfill`: `[server.limits] per_second` in the configuration
    #[arg(long, default_value_t = 100)]
    limit_rps: u32,
    /// `backfill`: `[server.backfill] rate_percent`
    #[arg(long, default_value_t = 50)]
    rate_percent: u8,
    /// `backfill`: Member records the server adds each second
    #[arg(long, default_value_t = 50)]
    members_per_second: u64,
    /// Seed of the fault sequence
    #[arg(long, default_value_t = 7)]
    seed: u64,
}

// Same allocator as the binary, so the sync child measures what `reso-ingest` would run
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("child") {
        // The sync under test: the real command line, in its own process
        let rt = tokio::runtime::Runtime::new().unwrap();
        let code = rt.block_on(reso_ingest::cli::run_main(
            &raw[2..],
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        ));
        std::process::exit(code);
    }
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new().unwrap();
    if args.scenario == "media-index" {
        let ok = rt.block_on(media_index(&args));
        std::process::exit(i32::from(!ok));
    }
    let scenarios: Vec<&str> = match args.scenario.as_str() {
        "all" => vec!["clean", "faults", "kill"],
        s @ ("clean" | "faults" | "kill" | "deletes" | "backfill") => vec![s],
        other => {
            eprintln!("unknown scenario {other}");
            std::process::exit(2);
        }
    };
    let mut rows = Vec::new();
    let mut failed = false;
    for s in scenarios {
        let report = rt.block_on(run_scenario(&args, s));
        eprintln!("{}", serde_json::to_string_pretty(&report.json).unwrap());
        failed |= !report.ok;
        rows.push(report.row);
    }
    println!("\n| date | commit | scenario | records | runs | wall s | records/s | peak RSS MiB | requests | faults | repeats | torn lines | result |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        println!("{r}");
    }
    std::process::exit(i32::from(failed));
}

// ---------------------------------------------------------------------------------------------
// The fake server

#[derive(Clone, Copy, Default)]
struct Faults {
    /// Per mille of /Property requests for each kind
    reset: u64,
    truncate: u64,
    unavailable: u64,
    throttle: u64,
    slow: u64,
}

impl Faults {
    fn spread(percent: f64) -> Self {
        let each = ((percent * 10.0) / 5.0).round() as u64;
        Self {
            reset: each,
            truncate: each,
            unavailable: each,
            throttle: each,
            slow: each,
        }
    }
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    served_records: AtomicU64,
    reset: AtomicU64,
    truncate: AtomicU64,
    unavailable: AtomicU64,
    throttle: AtomicU64,
    slow: AtomicU64,
    rate_limited: AtomicU64,
    /// `/Member` requests and records (`backfill`), not in `requests` or `served_records`
    member_requests: AtomicU64,
    member_records: AtomicU64,
}

impl Counters {
    fn faults(&self) -> u64 {
        [
            &self.reset,
            &self.truncate,
            &self.unavailable,
            &self.throttle,
            &self.slow,
            &self.rate_limited,
        ]
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum()
    }

    fn json(&self) -> Value {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        json!({
            "requests": l(&self.requests),
            "served_records": l(&self.served_records),
            "reset_before_response": l(&self.reset),
            "body_cut_short": l(&self.truncate),
            "status_503": l(&self.unavailable),
            "status_429_injected": l(&self.throttle),
            "slow_page": l(&self.slow),
            "status_429_rate_limit": l(&self.rate_limited),
            "member_requests": l(&self.member_requests),
            "member_records": l(&self.member_records),
        })
    }
}

struct Fake {
    n: u64,
    cap: u64,
    ties: u64,
    remarks: String,
    faults: Faults,
    rps: u64,
    rng: AtomicU64,
    window: Mutex<(u64, u64)>,
    counters: Counters,
    /// Records with an index divisible by this are gone (0: none)
    gone_every: AtomicU64,
    /// Member records added per second since `started` (0: no Member resource)
    members_per_second: u64,
    started: Instant,
}

impl Fake {
    fn gone(&self, i: u64) -> bool {
        let every = self.gone_every.load(Ordering::Relaxed);
        every > 0 && i.is_multiple_of(every)
    }
}

impl Fake {
    fn timestamp(&self, i: u64) -> String {
        format_ts(BASE_EPOCH + (i / self.ties) as i64)
    }

    fn record(&self, i: u64) -> Value {
        let status = ["Active", "Pending", "Closed"][(i % 3) as usize];
        let city = ["Springfield", "Riverton", "Lakeside", "Fairview"][(i % 4) as usize];
        json!({
            "ListingKey": format!("L{i:09}"),
            "ListingId": format!("X{}", i * 7 + 3),
            "ModificationTimestamp": self.timestamp(i),
            "StandardStatus": status,
            "ListPrice": 100_000 + (i * 7919) % 900_000,
            "BedroomsTotal": i % 6,
            "BathroomsTotalInteger": i % 4 + 1,
            "City": city,
            "PostalCode": format!("{:05}", 10_000 + i % 89_999),
            "PublicRemarks": self.remarks,
        })
    }

    /// The first index after a resume filter (`(TS gt X or (TS eq X and Key gt 'K'))` or `TS gt X`)
    fn start(&self, filter: &str) -> u64 {
        self.start_in(filter, "ListingKey", 'L', self.ties, self.n)
    }

    /// [`Fake::start`] for a resource keyed `key` (`prefix` + number), `ties` records per second
    fn start_in(&self, filter: &str, key: &str, prefix: char, ties: u64, n: u64) -> u64 {
        if let Some(key) = after(filter, &format!("{key} gt '")) {
            let k: u64 = key
                .trim_start_matches(prefix)
                .split('\'')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            return k + 1;
        }
        if let Some(ts) = after(filter, "ModificationTimestamp gt ") {
            let ts = ts.split([' ', ')']).next().unwrap_or("");
            if let Some(secs) = parse_ts(ts) {
                let offset = (secs - BASE_EPOCH + 1).max(0) as u64;
                return (offset * ties).min(n);
            }
        }
        0
    }

    fn roll(&self) -> u64 {
        // splitmix64 over a shared counter: deterministic for one sequential client
        let mut z = self
            .rng
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % 1000
    }

    fn over_rate(&self) -> bool {
        if self.rps == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut w = self.window.lock().unwrap();
        if w.0 != now {
            *w = (now, 0);
        }
        w.1 += 1;
        w.1 > self.rps
    }
}

fn after<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.find(prefix).map(|p| &s[p + prefix.len()..])
}

enum Reply {
    Send(u16, &'static str, Vec<u8>, Vec<(&'static str, String)>),
    Reset,
    Truncate(Vec<u8>),
}

fn metadata_xml(members: bool) -> Vec<u8> {
    let props = [
        ("ListingKey", "Edm.String"),
        ("ListingId", "Edm.String"),
        ("ModificationTimestamp", "Edm.DateTimeOffset"),
        ("StandardStatus", "Edm.String"),
        ("ListPrice", "Edm.Decimal"),
        ("BedroomsTotal", "Edm.Int64"),
        ("BathroomsTotalInteger", "Edm.Int64"),
        ("City", "Edm.String"),
        ("PostalCode", "Edm.String"),
        ("PublicRemarks", "Edm.String"),
    ];
    let fields: String = props
        .iter()
        .map(|(n, t)| format!(r#"<Property Name="{n}" Type="{t}"/>"#))
        .collect();
    format!(
        r#"<?xml version="1.0"?>
<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx" Version="4.0"><edmx:DataServices>
<Schema xmlns="http://docs.oasis-open.org/odata/ns/edm" Namespace="org.reso.metadata">
<EntityType Name="Property"><Key><PropertyRef Name="ListingKey"/></Key>{fields}</EntityType>
{member_type}<EntityContainer Name="C"><EntitySet Name="Property" EntityType="org.reso.metadata.Property"/>{member_set}</EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#,
        member_type = if members {
            r#"<EntityType Name="Member"><Key><PropertyRef Name="MemberKey"/></Key><Property Name="MemberKey" Type="Edm.String"/><Property Name="ModificationTimestamp" Type="Edm.DateTimeOffset"/></EntityType>"#
        } else {
            ""
        },
        member_set = if members {
            r#"<EntitySet Name="Member" EntityType="org.reso.metadata.Member"/>"#
        } else {
            ""
        },
    )
    .into_bytes()
}

fn respond(fake: &Fake, target: &str, host: &str) -> Reply {
    let json_reply = |status, body: Value| {
        Reply::Send(
            status,
            "application/json",
            body.to_string().into_bytes(),
            vec![],
        )
    };
    let url = match reqwest::Url::parse(&format!("http://{host}{target}")) {
        Ok(u) => u,
        Err(_) => return json_reply(400, json!({"error": {"message": "bad url"}})),
    };
    if url.path().ends_with("/$metadata") {
        return Reply::Send(
            200,
            "application/xml",
            metadata_xml(fake.members_per_second > 0),
            vec![],
        );
    }
    if fake.members_per_second > 0 && url.path().ends_with("/Member") {
        return members(fake, &url, target, host);
    }
    if !url.path().ends_with("/Property") {
        return json_reply(404, json!({"error": {"message": "not found"}}));
    }
    fake.counters.requests.fetch_add(1, Ordering::Relaxed);
    if fake.over_rate() {
        fake.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
        return Reply::Send(
            429,
            "application/json",
            br#"{"error":{"message":"rate limit"}}"#.to_vec(),
            vec![("Retry-After", "1".into())],
        );
    }
    let f = fake.faults;
    let roll = fake.roll();
    let mut bound = 0;
    let mut kind = 0;
    for (k, rate) in [f.reset, f.truncate, f.unavailable, f.throttle, f.slow]
        .into_iter()
        .enumerate()
    {
        bound += rate;
        if roll < bound {
            kind = k + 1;
            break;
        }
    }
    match kind {
        1 => {
            fake.counters.reset.fetch_add(1, Ordering::Relaxed);
            return Reply::Reset;
        }
        3 => {
            fake.counters.unavailable.fetch_add(1, Ordering::Relaxed);
            return json_reply(503, json!({"error": {"message": "unavailable"}}));
        }
        4 => {
            fake.counters.throttle.fetch_add(1, Ordering::Relaxed);
            return Reply::Send(
                429,
                "application/json",
                br#"{"error":{"message":"slow down"}}"#.to_vec(),
                vec![("Retry-After", "1".into())],
            );
        }
        _ => {}
    }
    let q: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    if q.get("$count").map(String::as_str) == Some("true") {
        let start = q.get("$filter").map_or(0, |f| fake.start(f));
        return json_reply(
            200,
            json!({"@odata.count": fake.n.saturating_sub(start), "value": []}),
        );
    }
    let start = q.get("$filter").map_or(0, |f| fake.start(f));
    let skip: u64 = q.get("$skip").and_then(|s| s.parse().ok()).unwrap_or(0);
    let top: u64 = q
        .get("$top")
        .and_then(|s| s.parse().ok())
        .unwrap_or(fake.cap);
    let from = (start + skip).min(fake.n);
    let to = (from + top.min(fake.cap)).min(fake.n);
    let keys_only = q.get("$select").map(String::as_str) == Some("ListingKey");
    let value: Vec<Value> = (from..to)
        .filter(|i| !fake.gone(*i))
        .map(|i| {
            if keys_only {
                json!({"ListingKey": format!("L{i:09}")})
            } else {
                fake.record(i)
            }
        })
        .collect();
    let mut body = json!({ "value": value });
    if to < fake.n {
        let raw = target.split("&$skip=").next().unwrap_or(target);
        body["@odata.nextLink"] = json!(format!("http://{host}{raw}&$skip={}", skip + (to - from)));
    }
    let bytes = body.to_string().into_bytes();
    match kind {
        2 => {
            fake.counters.truncate.fetch_add(1, Ordering::Relaxed);
            Reply::Truncate(bytes)
        }
        _ => {
            if kind == 5 {
                fake.counters.slow.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(300));
            }
            fake.counters
                .served_records
                .fetch_add(to - from, Ordering::Relaxed);
            Reply::Send(200, "application/json", bytes, vec![])
        }
    }
}

/// `/Member`: one record per `1 / members_per_second` seconds since the server started, one per
/// timestamp, so each incremental sync finds the records added since the last
fn members(fake: &Fake, url: &reqwest::Url, target: &str, host: &str) -> Reply {
    fake.counters
        .member_requests
        .fetch_add(1, Ordering::Relaxed);
    let n = (fake.started.elapsed().as_secs_f64() * fake.members_per_second as f64) as u64;
    let q: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    let start = q
        .get("$filter")
        .map_or(0, |f| fake.start_in(f, "MemberKey", 'M', 1, n))
        .min(n);
    let body = if q.get("$count").map(String::as_str) == Some("true") {
        json!({"@odata.count": n - start, "value": []})
    } else {
        let skip: u64 = q.get("$skip").and_then(|s| s.parse().ok()).unwrap_or(0);
        let top: u64 = q.get("$top").and_then(|s| s.parse().ok()).unwrap_or(1000);
        let from = (start + skip).min(n);
        let to = (from + top).min(n);
        fake.counters
            .member_records
            .fetch_add(to - from, Ordering::Relaxed);
        let value: Vec<Value> = (from..to)
            .map(|i| json!({"MemberKey": format!("M{i:09}"), "ModificationTimestamp": format_ts(BASE_EPOCH + i as i64)}))
            .collect();
        let mut body = json!({ "value": value });
        if to < n {
            let raw = target.split("&$skip=").next().unwrap_or(target);
            body["@odata.nextLink"] =
                json!(format!("http://{host}{raw}&$skip={}", skip + (to - from)));
        }
        body
    };
    Reply::Send(
        200,
        "application/json",
        body.to_string().into_bytes(),
        vec![],
    )
}

async fn serve_conn(fake: Arc<Fake>, stream: TcpStream) {
    let (read, mut write) = stream.into_split();
    let mut reader = AsyncBufReader::new(read);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let mut host = String::from("127.0.0.1");
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).await.unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("host") {
                    host = v.trim().to_string();
                }
            }
        }
        let f = fake.clone();
        let t = target.clone();
        let reply = tokio::task::spawn_blocking(move || respond(&f, &t, &host))
            .await
            .unwrap();
        let (status, ctype, body, extra, cut) = match reply {
            Reply::Reset => return, // drops the connection with no response
            Reply::Send(s, c, b, e) => (s, c, b, e, false),
            Reply::Truncate(b) => (200, "application/json", b, vec![], true),
        };
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let send = if cut {
            &body[..body.len() / 2]
        } else {
            &body[..]
        };
        if write.write_all(head.as_bytes()).await.is_err()
            || write.write_all(send).await.is_err()
            || write.flush().await.is_err()
        {
            return;
        }
        if cut {
            return; // the rest of the body never comes
        }
    }
}

async fn start_server(fake: Arc<Fake>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            // Head and body go out in two writes; without this, small replies (key pages) wait
            // about 40 ms each for the client's delayed ACK (Nagle)
            let _ = stream.set_nodelay(true);
            tokio::spawn(serve_conn(fake.clone(), stream));
        }
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------------------------
// Time stamps (whole seconds, UTC)

fn format_ts(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

// ---------------------------------------------------------------------------------------------
// Running the sync

struct RunResult {
    code: Option<i32>,
    peak_kib: u64,
    killed: bool,
}

fn vm_hwm_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn spawn_sync(config: &Path, out: &Path, log: &Path, extra: &[&str]) -> Child {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap();
    // `LOADTEST_WRAP` (#162): a command line the child runs under, split on spaces, e.g.
    // `valgrind --tool=callgrind` or `/usr/bin/time -o FILE -f FMT`; the server is not wrapped
    let wrap = std::env::var(WRAP_ENV).unwrap_or_default();
    let mut wrap = wrap.split_whitespace();
    let mut cmd = match wrap.next() {
        Some(program) => {
            let mut c = Command::new(program);
            c.args(wrap).arg(std::env::current_exe().unwrap());
            c
        }
        None => Command::new(std::env::current_exe().unwrap()),
    };
    cmd.arg("child")
        .args(["sync", "--config"])
        .arg(config)
        .arg("--out")
        .arg(out)
        .args(extra)
        .env(TOKEN_ENV, "loadtest-token")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap()
}

/// Run one sync; kill it once the server has sent `kill_at` records (cumulative), if given
async fn run_sync(dir: &Path, fake: &Fake, kill_at: Option<u64>, extra: &[&str]) -> RunResult {
    run_sync_logged(dir, fake, kill_at, extra, "sync.log").await
}

/// [`run_sync`] with its output appended to `dir/log`
async fn run_sync_logged(
    dir: &Path,
    fake: &Fake,
    kill_at: Option<u64>,
    extra: &[&str],
    log: &str,
) -> RunResult {
    let mut child = spawn_sync(
        &dir.join("servers.toml"),
        &dir.join("out"),
        &dir.join(log),
        extra,
    );
    let pid = child.id();
    let mut peak = 0;
    loop {
        if let Some(k) = vm_hwm_kib(pid) {
            peak = peak.max(k);
        }
        if let Some(status) = child.try_wait().unwrap() {
            return RunResult {
                code: status.code(),
                peak_kib: peak,
                killed: false,
            };
        }
        if kill_at.is_some_and(|k| fake.counters.served_records.load(Ordering::Relaxed) >= k) {
            child.kill().unwrap(); // SIGKILL: no shutdown handling, buffers are lost
            child.wait().unwrap();
            return RunResult {
                code: None,
                peak_kib: peak,
                killed: true,
            };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Remove lease files left by a killed run. The next run would otherwise skip the feed until the
/// lease runs out (at most 5 minutes; README "What it writes"), which is the documented behaviour.
fn clear_stale_leases(state: &Path) -> u64 {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(state) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".lock") && name.contains(".Property") {
                let _ = std::fs::remove_file(e.path());
                n += 1;
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------------------------
// Checking the output

#[derive(Default)]
struct Check {
    lines: u64,
    torn: u64,
    missing: u64,
    repeats: u64,
    bytes: u64,
    files: u64,
}

fn check_output(out: &Path, n: u64) -> Check {
    let mut seen = vec![0u8; n as usize];
    let mut c = Check::default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(out)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".ndjson") {
            continue;
        }
        c.files += 1;
        c.bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
        let reader = BufReader::new(std::fs::File::open(&path).unwrap());
        for line in reader.split(b'\n') {
            let line = line.unwrap();
            let parsed = serde_json::from_slice::<Value>(&line).ok();
            if parsed
                .as_ref()
                .is_some_and(|v| v["record"].get("MemberKey").is_some())
            {
                continue; // `backfill`: the Member feed synced alongside
            }
            c.lines += 1;
            let key = parsed.and_then(|v| {
                v["record"]["ListingKey"]
                    .as_str()
                    .and_then(|k| k.trim_start_matches('L').parse::<u64>().ok())
            });
            match key {
                Some(k) if k < n => seen[k as usize] = seen[k as usize].saturating_add(1),
                _ => c.torn += 1,
            }
        }
    }
    for s in seen {
        match s {
            0 => c.missing += 1,
            k => c.repeats += u64::from(k - 1),
        }
    }
    c
}

// ---------------------------------------------------------------------------------------------
// Scenarios

struct Report {
    ok: bool,
    row: String,
    json: Value,
}

fn write_config(dir: &Path, url: &str, args: &Args) {
    let text = format!(
        "[[server]]\nname = \"load\"\nbase_url = \"{url}\"\npage_size = {}\ntimeout_secs = 30\n\
         [server.limits]\nmax_retries = 6\ninitial_backoff_ms = 100\nmax_backoff_ms = 2000\n\
         [server.auth]\ntype = \"bearer\"\ntoken_env = \"{TOKEN_ENV}\"\n",
        args.page_size
    );
    std::fs::write(dir.join("servers.toml"), text).unwrap();
}

fn commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
}

async fn run_scenario(args: &Args, scenario: &str) -> Report {
    let faults = if scenario == "faults" {
        Faults::spread(args.fault_percent)
    } else {
        Faults::default()
    };
    let fake = Arc::new(Fake {
        n: args.records,
        cap: args.server_cap,
        ties: args.ties.max(1),
        remarks: "Invented remarks for load testing. ".repeat(args.remarks_bytes / 35 + 1)
            [..args.remarks_bytes]
            .to_string(),
        faults,
        rps: args.server_rps,
        rng: AtomicU64::new(args.seed),
        window: Mutex::new((0, 0)),
        counters: Counters::default(),
        gone_every: AtomicU64::new(0),
        members_per_second: if scenario == "backfill" {
            args.members_per_second.max(1)
        } else {
            0
        },
        started: Instant::now(),
    });
    let url = start_server(fake.clone()).await;
    let dir = args.workdir.join(scenario);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_config(&dir, &url, args);
    if scenario == "deletes" {
        return run_deletes(args, &fake, &dir).await;
    }
    if scenario == "backfill" {
        return run_backfill(args, &fake, &dir).await;
    }
    eprintln!(
        "== {scenario}: {} records from {url}, output in {}",
        args.records,
        dir.display()
    );

    let kill_points: Vec<u64> = if scenario == "kill" {
        (1..=args.kills)
            .map(|k| args.records * k / (args.kills + 1))
            .collect()
    } else {
        vec![]
    };
    let started = Instant::now();
    let mut runs = 0u32;
    let mut kills = 0u64;
    let mut failures = 0u32;
    let mut leases_cleared = 0;
    let mut peak = 0;
    let mut finished = false;
    while runs < args.max_runs {
        runs += 1;
        let kill_at = kill_points.get(kills as usize).copied();
        let r = run_sync(&dir, &fake, kill_at, &[]).await;
        peak = peak.max(r.peak_kib);
        if r.killed {
            kills += 1;
            leases_cleared += clear_stale_leases(&dir.join("out").join("state"));
            eprintln!(
                "   run {runs}: killed at {} served records",
                fake.counters.served_records.load(Ordering::Relaxed)
            );
            continue;
        }
        if r.code == Some(0) {
            finished = true;
            eprintln!("   run {runs}: finished");
            break;
        }
        failures += 1;
        eprintln!("   run {runs}: exit {:?}, running again", r.code);
    }
    let wall = started.elapsed().as_secs_f64();
    let check = check_output(&dir.join("out"), args.records);
    let repeat_bound = kills * args.page_size.min(args.server_cap);
    let exact = scenario == "clean";
    let ok = finished
        && check.missing == 0
        && check.torn == 0
        && if exact {
            check.repeats == 0
        } else {
            check.repeats <= repeat_bound
        };
    let rate = args.records as f64 / wall;
    let json = json!({
        "scenario": scenario,
        "records": args.records,
        "page_size": args.page_size,
        "server_cap": args.server_cap,
        "remarks_bytes": args.remarks_bytes,
        "finished": finished,
        "runs": runs,
        "failed_runs": failures,
        "kills": kills,
        "stale_leases_removed": leases_cleared,
        "wall_seconds": (wall * 10.0).round() / 10.0,
        "records_per_second": rate.round(),
        "peak_rss_mib": peak / 1024,
        "output": {"files": check.files, "bytes": check.bytes, "lines": check.lines},
        "missing": check.missing,
        "repeats": check.repeats,
        "repeat_bound": if exact { 0 } else { repeat_bound },
        "torn_lines": check.torn,
        "server": fake.counters.json(),
        "ok": ok,
    });
    let row = format!(
        "| {} | {} | {scenario} | {} | {runs} | {:.1} | {:.0} | {} | {} | {} | {} | {} | {} |",
        format_ts(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        )
        .get(..10)
        .unwrap_or(""),
        commit(),
        args.records,
        wall,
        rate,
        peak / 1024,
        fake.counters.requests.load(Ordering::Relaxed),
        fake.counters.faults(),
        check.repeats,
        check.torn,
        if ok { "ok" } else { "FAILED" }
    );
    if !args.keep {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = std::io::stderr().flush();
    Report { ok, row, json }
}

fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    format_ts(now).get(..10).unwrap_or("").to_string()
}

/// Delete events in the output: (lines, keys that should not be there, repeated keys)
fn check_deletes(out: &Path, fake: &Fake) -> (u64, u64, u64) {
    let mut seen = std::collections::HashSet::new();
    let (mut lines, mut wrong, mut repeats) = (0, 0, 0);
    let paths: Vec<PathBuf> = std::fs::read_dir(out)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    for path in paths
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
    {
        for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
            let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if v["event"] != "delete" {
                continue;
            }
            lines += 1;
            let i: Option<u64> = v["key"]
                .as_str()
                .and_then(|k| k.trim_start_matches('L').parse().ok());
            match i {
                Some(i) if i < fake.n && fake.gone(i) => {
                    if !seen.insert(i) {
                        repeats += 1;
                    }
                }
                _ => wrong += 1,
            }
        }
    }
    (lines, wrong, repeats)
}

/// The `backfill` scenario: a killed and resumed `sync --backfill` of Property while `sync
/// --resource Member` runs every second beside it
async fn run_backfill(args: &Args, fake: &Fake, dir: &Path) -> Report {
    let config = format!(
        "[[server]]\nname = \"load\"\nbase_url = \"{}\"\npage_size = {}\ntimeout_secs = 30\n\
         [server.limits]\nmax_retries = 6\ninitial_backoff_ms = 100\nmax_backoff_ms = 2000\nper_second = {}\n\
         [server.backfill]\nrate_percent = {}\n\
         [server.auth]\ntype = \"bearer\"\ntoken_env = \"{TOKEN_ENV}\"\n",
        std::fs::read_to_string(dir.join("servers.toml"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("base_url = \""))
            .and_then(|l| l.strip_suffix('"'))
            .unwrap(),
        args.page_size,
        args.limit_rps,
        args.rate_percent
    );
    std::fs::write(dir.join("servers.toml"), config).unwrap();
    let share = f64::from(args.limit_rps) * f64::from(args.rate_percent) / 100.0;
    eprintln!(
        "== backfill: {} records, {} requests/s limit, backfill {}%, Member +{}/s, output in {}",
        args.records,
        args.limit_rps,
        args.rate_percent,
        args.members_per_second,
        dir.display()
    );

    let done = std::sync::atomic::AtomicBool::new(false);
    let started = Instant::now();
    let backfill = async {
        let mut runs = 0u32;
        let mut peak = 0;
        let mut killed_at = None;
        let mut finished = false;
        while runs < args.max_runs {
            runs += 1;
            let kill_at = killed_at.is_none().then_some(args.records / 2);
            let r = run_sync(
                dir,
                fake,
                kill_at,
                &["--backfill", "--resource", "Property"],
            )
            .await;
            peak = peak.max(r.peak_kib);
            if r.killed {
                let served = fake.counters.served_records.load(Ordering::Relaxed);
                killed_at = Some(served);
                clear_stale_leases(&dir.join("out").join("state"));
                eprintln!("   backfill run {runs}: killed at {served} served records");
                continue;
            }
            if r.code == Some(0) {
                finished = true;
                eprintln!("   backfill run {runs}: finished");
                break;
            }
            eprintln!("   backfill run {runs}: exit {:?}, running again", r.code);
        }
        done.store(true, Ordering::Relaxed);
        (
            runs,
            peak,
            killed_at,
            finished,
            started.elapsed().as_secs_f64(),
        )
    };
    let alongside = async {
        // (seconds, exit code) of each Member sync started while the backfill ran
        let mut syncs = Vec::new();
        while !done.load(Ordering::Relaxed) {
            let t = Instant::now();
            let r = run_sync_logged(dir, fake, None, &["--resource", "Member"], "member.log").await;
            syncs.push((t.elapsed().as_secs_f64(), r.code));
            while t.elapsed() < Duration::from_secs(1) && !done.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        syncs
    };
    let ((runs, peak, killed_at, finished, backfill_secs), member_syncs) =
        tokio::join!(backfill, alongside);
    let wall = started.elapsed().as_secs_f64();

    // What the backfill said about itself
    let log = std::fs::read_to_string(dir.join("sync.log")).unwrap_or_default();
    let progress: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("##progress") && l.contains("label=load/Property sub="))
        .collect();
    let with_eta = progress.iter().filter(|l| l.contains(" eta=")).count();
    let last_progress = progress.last().copied().unwrap_or("");
    let ended_complete = last_progress
        .split_whitespace()
        .find_map(|w| w.strip_prefix("sub="))
        .and_then(|s| s.split_once('/'))
        .is_some_and(|(a, b)| a == b);
    let left: Vec<u64> = log
        .lines()
        .filter_map(|l| after(l, "backfill: "))
        .filter_map(|l| l.split(' ').next()?.parse().ok())
        .collect();
    let resumed_count_ok = match (left.as_slice(), killed_at) {
        // The resumed count starts after the checkpoint: at most the records not yet served,
        // plus the page in flight when the run was killed
        ([first, second], Some(served)) => {
            *first == args.records
                && *second <= args.records - served + args.page_size
                && *second + served >= args.records
        }
        _ => false,
    };
    let property_requests = fake.counters.requests.load(Ordering::Relaxed);
    let backfill_rps = property_requests as f64 / backfill_secs;
    // A whole second of slack over the run for the limiter's first window
    let within_share = backfill_rps <= share * (1.0 + 1.0 / backfill_secs) + 0.5;

    let members = std::fs::read_to_string(dir.join("member.log")).unwrap_or_default();
    let rest_message = format!(
        "using {}% of the rate limits (a backfill is running)",
        100 - args.rate_percent
    );
    let saw_rest = members.matches(&rest_message).count();
    let member_ok = member_syncs.iter().all(|(_, c)| *c == Some(0));
    let member_max = member_syncs.iter().map(|(t, _)| *t).fold(0.0, f64::max);
    let member_mean =
        member_syncs.iter().map(|(t, _)| *t).sum::<f64>() / member_syncs.len().max(1) as f64;

    let check = check_output(&dir.join("out"), args.records);
    let repeat_bound = args.page_size.min(args.server_cap);
    let ok = finished
        && killed_at.is_some()
        && check.missing == 0
        && check.torn == 0
        && check.repeats <= repeat_bound
        && progress.len() as u64 >= args.records / args.page_size
        && with_eta == progress.len()
        && ended_complete
        && resumed_count_ok
        && within_share
        && member_ok
        && !member_syncs.is_empty()
        && saw_rest > 0;
    let rate = args.records as f64 / backfill_secs;
    let json = json!({
        "scenario": "backfill",
        "records": args.records,
        "page_size": args.page_size,
        "limit_rps": args.limit_rps,
        "rate_percent": args.rate_percent,
        "finished": finished,
        "runs": runs,
        "killed_at_served_records": killed_at,
        "backfill_seconds": (backfill_secs * 10.0).round() / 10.0,
        "records_per_second": rate.round(),
        "property_requests_per_second": (backfill_rps * 10.0).round() / 10.0,
        "share_requests_per_second": share,
        "within_share": within_share,
        "peak_rss_mib": peak / 1024,
        "progress_lines": progress.len(),
        "progress_lines_with_eta": with_eta,
        "last_progress": last_progress,
        "records_left_per_run": left,
        "resumed_count_ok": resumed_count_ok,
        "member_syncs": member_syncs.len(),
        "member_syncs_ok": member_ok,
        "member_syncs_on_the_rest": saw_rest,
        "member_sync_seconds_mean": (member_mean * 100.0).round() / 100.0,
        "member_sync_seconds_max": (member_max * 100.0).round() / 100.0,
        "output": {"files": check.files, "bytes": check.bytes, "lines": check.lines},
        "missing": check.missing,
        "repeats": check.repeats,
        "repeat_bound": repeat_bound,
        "torn_lines": check.torn,
        "wall_seconds": (wall * 10.0).round() / 10.0,
        "server": fake.counters.json(),
        "ok": ok,
    });
    let row = format!(
        "| {} | {} | backfill | {} | {runs} | {backfill_secs:.1} | {rate:.0} | {} | {property_requests} | 0 | {} | {} | {} |",
        today(),
        commit(),
        args.records,
        peak / 1024,
        check.repeats,
        check.torn,
        if ok { "ok" } else { "FAILED" }
    );
    if !args.keep {
        let _ = std::fs::remove_dir_all(dir);
    }
    Report { ok, row, json }
}

/// The `deletes` scenario: baseline, a pass that finds the gone records, a pass with nothing new
async fn run_deletes(args: &Args, fake: &Fake, dir: &Path) -> Report {
    // A checkpoint at the last record, so each sync pulls no records and only the key pull runs
    let state = dir.join("out").join("state");
    std::fs::create_dir_all(&state).unwrap();
    let last = args.records.saturating_sub(1);
    let checkpoint = json!({"server": "load", "resource": "Property",
        "timestamp": fake.timestamp(last), "key": format!("L{last:09}"),
        "updated_at": "2026-01-01T00:00:00Z"});
    std::fs::write(state.join("load.Property.json"), checkpoint.to_string()).unwrap();
    eprintln!(
        "== deletes: {} keys, output in {}",
        args.records,
        dir.display()
    );

    let every = args.gone_every.max(1);
    let expected = args.records.div_ceil(every);
    let mut steps = Vec::new();
    let mut ok = true;
    let started = Instant::now();
    for step in ["baseline", "deletes", "repeat"] {
        if step == "deletes" {
            fake.gone_every.store(every, Ordering::Relaxed);
        }
        let t = Instant::now();
        let r = run_sync(dir, fake, None, &["--deletes"]).await;
        let secs = t.elapsed().as_secs_f64();
        let (lines, wrong, repeats) = check_deletes(&dir.join("out"), fake);
        let want = if step == "baseline" { 0 } else { expected };
        let step_ok = r.code == Some(0) && lines == want && wrong == 0 && repeats == 0;
        ok &= step_ok;
        eprintln!(
            "   {step}: exit {:?}, {secs:.1} s, peak {} MiB, {lines} delete lines in total",
            r.code,
            r.peak_kib / 1024
        );
        steps.push(
            json!({"step": step, "exit": r.code, "seconds": (secs * 10.0).round() / 10.0,
            "peak_rss_mib": r.peak_kib / 1024, "delete_lines_total": lines,
            "wrong_keys": wrong, "repeats": repeats, "ok": step_ok}),
        );
    }
    let wall = started.elapsed().as_secs_f64();
    let peak = steps
        .iter()
        .filter_map(|s| s["peak_rss_mib"].as_u64())
        .max()
        .unwrap_or(0);
    let keyset_bytes = std::fs::metadata(state.join("load.Property.keys.ndjson"))
        .or_else(|_| std::fs::metadata(state.join("load.Property.keys.json")))
        .map_or(0, |m| m.len());
    let json = json!({"scenario": "deletes", "keys": args.records, "gone_every": every,
        "expected_deletes": expected, "steps": steps, "wall_seconds": (wall * 10.0).round() / 10.0,
        "peak_rss_mib": peak, "keyset_bytes": keyset_bytes,
        "server": fake.counters.json(), "ok": ok});
    let row = format!(
        "| {} | {} | deletes | {} | 3 | {wall:.1} | {:.0} | {peak} | {} | 0 | 0 | 0 | {} |",
        today(),
        commit(),
        args.records,
        3.0 * args.records as f64 / wall,
        fake.counters.requests.load(Ordering::Relaxed),
        if ok { "ok" } else { "FAILED" }
    );
    if !args.keep {
        let _ = std::fs::remove_dir_all(dir);
    }
    Report { ok, row, json }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(format_ts(BASE_EPOCH), "2026-01-01T00:00:00Z");
        for t in [BASE_EPOCH, BASE_EPOCH + 86_399, BASE_EPOCH + 5_000_000] {
            assert_eq!(parse_ts(&format_ts(t)), Some(t));
        }
        assert_eq!(format_ts(BASE_EPOCH + 59 * 86_400), "2026-03-01T00:00:00Z");
    }
}

// ---------------------------------------------------------------------------------------------
// The media index (#149)

/// A state store that counts the bytes written through it
#[derive(Debug)]
struct Counting {
    inner: reso_ingest::FileStateStore,
    put_bytes: AtomicU64,
}

impl reso_ingest::StateStore for Counting {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> reso_ingest::StoreFuture<'a, Option<reso_ingest::Versioned>> {
        self.inner.get(key)
    }
    fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> reso_ingest::StoreFuture<'a, ()> {
        self.put_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.inner.put(key, bytes)
    }
    fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> reso_ingest::StoreFuture<'a, bool> {
        self.inner.create(key, bytes)
    }
    fn replace<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> reso_ingest::StoreFuture<'a, bool> {
        self.inner.replace(key, bytes, version)
    }
    fn delete<'a>(
        &'a self,
        key: &'a str,
        version: Option<&'a str>,
    ) -> reso_ingest::StoreFuture<'a, bool> {
        self.inner.delete(key, version)
    }
    fn list(&self) -> reso_ingest::StoreFuture<'_, Vec<String>> {
        self.inner.list()
    }
    fn location(&self, key: &str) -> String {
        self.inner.location(key)
    }
}

fn media_record(i: u64) -> Value {
    json!({
        "MediaKey": format!("M{i:010}"),
        "MediaURL": format!("https://cdn.example.com/photos/{i}.jpg?sig=abcdef0123456789&expires=1767225600"),
        "MediaModificationTimestamp": format_ts(BASE_EPOCH + i as i64),
        "ResourceRecordKey": format!("L{}", i / 30),
    })
}

/// The media-index scenario; true when the reloaded index matched
async fn media_index(args: &Args) -> bool {
    use reso_ingest::{MediaIndex, StateStore as _};
    let dir = args.workdir.join("media-index");
    let _ = std::fs::remove_dir_all(&dir);
    let store = Counting {
        inner: reso_ingest::FileStateStore::new(&dir),
        put_bytes: AtomicU64::new(0),
    };
    let server = "load";
    let page = args.page_size.max(1);
    let pages = args.records.div_ceil(page);
    let marks: Vec<u64> = (1..=10).map(|t| (pages * t / 10).max(1)).collect();
    eprintln!(
        "== media-index: {} records in {pages} pages of {page}, state in {}",
        args.records,
        dir.display()
    );
    let mut index = MediaIndex::default();
    let started = Instant::now();
    let mut window: Vec<(f64, u64)> = Vec::new();
    let mut rows = Vec::new();
    let mut total_save = 0.0;
    for p in 0..pages {
        let first = p * page;
        let last = (first + page).min(args.records);
        for i in first..last {
            index.add_record(&media_record(i));
        }
        // The downloads of the page before finish
        if p > 0 {
            for i in (first - page)..first {
                if let Some(e) = index.entry_mut(&format!("M{i:010}")) {
                    e.pending = false;
                    e.file = Some(format!("{:064x}.jpg", i));
                    e.sha256 = Some(format!("{:064x}", i));
                    e.etag = Some(format!("\"{i:x}\""));
                    e.content_type = Some("image/jpeg".into());
                    e.bytes = Some(250_000);
                }
            }
        }
        let before = store.put_bytes.load(Ordering::Relaxed);
        let t = Instant::now();
        index.save_changes(&store, server).await.unwrap();
        let secs = t.elapsed().as_secs_f64();
        total_save += secs;
        window.push((secs, store.put_bytes.load(Ordering::Relaxed) - before));
        if marks.contains(&(p + 1)) {
            // The cost of a save before #149: the whole index, every page
            let text = serde_json::to_vec(&index).unwrap();
            let t = Instant::now();
            store.inner.put("whole.json", &text).await.unwrap();
            let whole_secs = t.elapsed().as_secs_f64();
            let mut times: Vec<f64> = window.iter().map(|w| w.0).collect();
            times.sort_by(f64::total_cmp);
            let median = times[times.len() / 2];
            let max = times.last().copied().unwrap_or(0.0);
            let mut bytes: Vec<u64> = window.iter().map(|w| w.1).collect();
            bytes.sort_unstable();
            let median_bytes = bytes[bytes.len() / 2];
            let mean_bytes = bytes.iter().sum::<u64>() / bytes.len() as u64;
            rows.push(format!(
                "| {} | {:.1} | {:.1} | {} | {} | {:.1} | {} |",
                index.entries.len(),
                median * 1000.0,
                max * 1000.0,
                median_bytes / 1024,
                mean_bytes / 1024,
                (whole_secs + serialize_secs(&index)) * 1000.0,
                text.len() / 1024,
            ));
            eprintln!("##progress step={}/{pages} label=media-index", p + 1);
            window.clear();
        }
    }
    let _ = store.inner.delete("whole.json", None).await;
    let wall = started.elapsed().as_secs_f64();
    let t = Instant::now();
    let loaded = MediaIndex::load(&store, server).await.unwrap();
    let load_secs = t.elapsed().as_secs_f64();
    let ok = loaded == index;
    let keys = store.list().await.unwrap().len();
    let peak = vm_hwm_kib(std::process::id()).unwrap_or(0);
    let written = store.put_bytes.load(Ordering::Relaxed);
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "scenario": "media-index",
            "records": args.records,
            "page_size": page,
            "wall_seconds": (wall * 10.0).round() / 10.0,
            "save_seconds_total": (total_save * 10.0).round() / 10.0,
            "bytes_written_mib": written / (1 << 20),
            "state_keys_at_end": keys,
            "load_seconds": (load_secs * 100.0).round() / 100.0,
            "reloaded_equal": ok,
            "peak_rss_mib": peak / 1024,
        }))
        .unwrap()
    );
    println!(
        "\nmedia-index: {} records, commit {}, {}",
        args.records,
        commit(),
        today()
    );
    println!("| entries | save ms (median) | save ms (max) | KiB per save (median) | KiB per save (mean) | whole rewrite ms | whole index KiB |");
    println!("|---|---|---|---|---|---|---|");
    for r in rows {
        println!("{r}");
    }
    if !args.keep {
        let _ = std::fs::remove_dir_all(&dir);
    }
    ok
}

/// Time to serialize the whole index (part of every save before #149)
fn serialize_secs(index: &reso_ingest::MediaIndex) -> f64 {
    let t = Instant::now();
    let text = serde_json::to_vec(index).unwrap();
    std::hint::black_box(text.len());
    t.elapsed().as_secs_f64()
}
