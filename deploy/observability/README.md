# Prometheus + Grafana for `reso-ingest serve`

A small kit for one person to watch one `serve` daemon on one machine. It keeps 30 days of history, alerts on failing
or stuck feeds, and shows a dashboard per feed. These are config files only; nothing here is compiled into
`reso-ingest`. For "what is happening right now", `serve --ui` serves status pages on the daemon itself. This kit is
for history and alerts.

| File | What |
|---|---|
| `prometheus.yml` | Scrapes `serve` on `127.0.0.1:9090` every 30 s and loads the two rule files |
| `alerts.yml` | Rules for `serve` feeds: `ResoServeDown`, `ResoFeedFailing` (3 failures in a row), `ResoFeedNoSuccess` (no success in 3 intervals + 10 min), `ResoRateLimitHigh` (over 90% for 15 min) |
| `../../reso-ingest/monitoring/alert-rules.yml` | The rules for every run's results, shared with `sync --metrics-file`: stale feeds (freshness lag over 2 h), failed resources, held deletes, schema changes and others. It is copied in by `setup.sh` and mounted by `compose.yaml` |
| `grafana/provisioning/` | The Prometheus data source (uid `reso-prometheus`) and the dashboard folder |
| `grafana/dashboards/reso-ingest-serve.json` | The dashboard (uid `reso-ingest-serve`), filtered by `Server` and `Resource`. Panels: feeds now (failures in a row, paused, time since the last success, freshness lag), syncs and failed syncs per hour, records per sync, freshness lag, failures in a row, sync duration, rate-limit use, responses by class, media files |
| `setup.sh` | Native install, no Docker |
| `compose.yaml` | The same with Docker Compose |

Each alert's runbook is in [`docs/OPERATIONS.md`](../../docs/OPERATIONS.md#runbooks).

## Ports

Both setups listen on loopback only:

- `serve`: `127.0.0.1:9090`, the port used in the docs (`serve --listen 127.0.0.1:9090`)
- Prometheus: `127.0.0.1:9190`, not its usual 9090, which `serve` already uses
- Grafana: `127.0.0.1:3000`

Grafana lets anyone who can reach it view the dashboards without signing in. Sign in as `admin` (password `admin`,
changed at the first sign-in) to edit them. Keep it on loopback, or put it behind something that authenticates.

## Native (systemd user units)

```sh
deploy/observability/setup.sh install                       # serve on 127.0.0.1:9090
deploy/observability/setup.sh install --target 10.0.0.5:9090   # serve on another host
```

The script:

1. Downloads pinned Prometheus and Grafana releases for amd64 or arm64.
2. Checks them against the SHA-256 sums written in the script.
3. Unpacks them into `~/.local/share/reso-observability` (`RESO_OBS_DIR` overrides).
4. Writes the configuration there.
5. Starts two systemd `--user` services, `reso-prometheus` and `reso-grafana`.

Running it again re-copies the configuration and restarts both services. This is also how to pick up an edited rule or
dashboard. The downloads are kept, so a re-run doesn't fetch them again.

The install takes about 2.3 GB of disk (amd64, Prometheus 3.16.0 and Grafana 13.3.2), including the kept downloads.
Prometheus data grows from there over its 30 days. `setup.sh remove` leaves all of it in place; delete the directory to
free the space.

```sh
systemctl --user status reso-prometheus reso-grafana
journalctl --user -u reso-grafana
loginctl enable-linger "$USER"        # keep them running when you are logged out
deploy/observability/setup.sh remove  # stop and remove the units; data stays in ~/.local/share/reso-observability
```

To upgrade, change `PROMETHEUS_VERSION` / `GRAFANA_VERSION` and their sums in `setup.sh`. The sums are in
Prometheus's `sha256sums.txt` on its GitHub release and in Grafana's `.tar.gz.sha256` files on `dl.grafana.com`.

## Docker Compose

```sh
docker compose -f deploy/observability/compose.yaml up -d
docker compose -f deploy/observability/compose.yaml down        # add -v to delete the data
```

Both containers use host networking, so their addresses are the same as in the native setup. This works on Linux.
On Docker Desktop (macOS, Windows), replace `network_mode: host` with port mappings and scrape
`host.docker.internal:9090`. To scrape another host, edit the target in `prometheus.yml`.

## Check it works

1. Prometheus → Status → Targets: `reso-ingest` is `UP`.
2. Open `http://127.0.0.1:3000/d/reso-ingest-serve`. Every feed has a row under "Feeds now", and the hourly panels fill
   in after the feeds' first syncs.
3. Prometheus → Alerts lists the rules from both files. Break a feed (for example, point a resource at a name the
   server doesn't have) and `ResoFeedFailing` fires after its third failure.

## Lighter option: VictoriaMetrics

VictoriaMetrics single-node is one binary that reads the same `prometheus.yml`
(`victoria-metrics -promscrape.config=prometheus.yml -retentionPeriod=30d`) and uses less memory. Alert rules need
its separate `vmalert`. In Grafana, point the data source at it as a Prometheus type. It is not set up here.
