#!/usr/bin/env bash
# Prometheus + Grafana for one `reso-ingest serve`, without Docker (see README.md).
#
#   deploy/observability/setup.sh install [--target HOST:PORT]   # download, verify, write units, start
#   deploy/observability/setup.sh remove                         # stop and remove the units (data kept)
#
# Installs pinned releases into ${RESO_OBS_DIR:-~/.local/share/reso-observability} and runs them as
# systemd --user services on loopback: Prometheus on 127.0.0.1:9190, Grafana on 127.0.0.1:3000.
# Downloads are checked against the SHA-256 sums below before they are unpacked.
set -euo pipefail

PROMETHEUS_VERSION=3.16.0
GRAFANA_VERSION=13.3.2
declare -A PROMETHEUS_SHA256=(
  [amd64]=68727009472a2c02e1ff7f062fd5b3e637285bffc8fc2d340c76fb449d6e0f34
  [arm64]=fd8571b8162f4020232d886f17340bcf92e1722985ae3b4c1d096157826c0fbe
)
declare -A GRAFANA_SHA256=(
  [amd64]=e6f06cf4ad96a4809294c2e78d3ebe6bc42a79bff36ffe35eefbe502fc19aa87
  [arm64]=366f33263bf92a615644b129cb1f465eb32d632e48c6157c1831ca0194385a56
)

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DIR=${RESO_OBS_DIR:-$HOME/.local/share/reso-observability}
UNITS=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

case $(uname -m) in
  x86_64) ARCH=amd64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *)
    echo "unsupported machine: $(uname -m)" >&2
    exit 2
    ;;
esac

fetch() { # URL SHA256 DEST
  local tmp="$3.part"
  if [[ -f $3 ]] && echo "$2  $3" | sha256sum -c --quiet - 2>/dev/null; then
    return
  fi
  echo "downloading $1"
  curl -fL --retry 3 -o "$tmp" "$1"
  echo "$2  $tmp" | sha256sum -c --quiet - || {
    echo "checksum mismatch: $1" >&2
    rm -f "$tmp"
    exit 1
  }
  mv "$tmp" "$3"
}

install() {
  local target=127.0.0.1:9090
  while [[ $# -gt 0 ]]; do
    case $1 in
      --target)
        target=$2
        shift 2
        ;;
      *)
        echo "unknown option: $1" >&2
        exit 2
        ;;
    esac
  done
  mkdir -p "$DIR"/{downloads,etc/prometheus,dashboards,data/prometheus,data/grafana,logs} "$UNITS"

  local prom="prometheus-$PROMETHEUS_VERSION.linux-$ARCH"
  fetch "https://github.com/prometheus/prometheus/releases/download/v$PROMETHEUS_VERSION/$prom.tar.gz" \
    "${PROMETHEUS_SHA256[$ARCH]}" "$DIR/downloads/$prom.tar.gz"
  local graf="grafana-$GRAFANA_VERSION.linux-$ARCH"
  fetch "https://dl.grafana.com/oss/release/$graf.tar.gz" "${GRAFANA_SHA256[$ARCH]}" "$DIR/downloads/$graf.tar.gz"

  rm -rf "$DIR/prometheus" "$DIR/grafana"
  mkdir -p "$DIR/prometheus" "$DIR/grafana"
  tar -xzf "$DIR/downloads/$prom.tar.gz" -C "$DIR/prometheus" --strip-components 1
  tar -xzf "$DIR/downloads/$graf.tar.gz" -C "$DIR/grafana" --strip-components 1

  # Configuration: the kit's files, the shared alert rules, and the scrape target
  sed "s|\"127.0.0.1:9090\"|\"$target\"|" "$HERE/prometheus.yml" >"$DIR/etc/prometheus/prometheus.yml"
  cp "$HERE/alerts.yml" "$DIR/etc/prometheus/alerts.yml"
  cp "$HERE/../../reso-ingest/monitoring/alert-rules.yml" "$DIR/etc/prometheus/alert-rules.yml"
  "$DIR/prometheus/promtool" check config "$DIR/etc/prometheus/prometheus.yml" >/dev/null
  rm -rf "$DIR/etc/provisioning"
  cp -r "$HERE/grafana/provisioning" "$DIR/etc/provisioning"
  cp "$HERE"/grafana/dashboards/*.json "$DIR/dashboards/"
  cat >"$DIR/etc/grafana.ini" <<EOF
[server]
http_addr = 127.0.0.1
http_port = 3000
[paths]
data = $DIR/data/grafana
logs = $DIR/logs
plugins = $DIR/data/grafana/plugins
provisioning = $DIR/etc/provisioning
[auth.anonymous]
# Loopback only: anyone on this machine can view; sign in as admin (password admin at first) to edit
enabled = true
org_role = Viewer
[analytics]
reporting_enabled = false
check_for_updates = false
check_for_plugin_updates = false
[news]
news_feed_enabled = false
EOF

  cat >"$UNITS/reso-prometheus.service" <<EOF
[Unit]
Description=Prometheus for reso-ingest serve (127.0.0.1:9190)

[Service]
ExecStart=$DIR/prometheus/prometheus --config.file=$DIR/etc/prometheus/prometheus.yml --storage.tsdb.path=$DIR/data/prometheus --storage.tsdb.retention.time=30d --web.listen-address=127.0.0.1:9190
ExecReload=/bin/kill -HUP \$MAINPID
Restart=on-failure

[Install]
WantedBy=default.target
EOF
  cat >"$UNITS/reso-grafana.service" <<EOF
[Unit]
Description=Grafana for reso-ingest serve (127.0.0.1:3000)
After=reso-prometheus.service

[Service]
Environment=RESO_DASHBOARDS=$DIR/dashboards
ExecStart=$DIR/grafana/bin/grafana server --homepath $DIR/grafana --config $DIR/etc/grafana.ini
Restart=on-failure

[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload
  systemctl --user enable --now reso-prometheus.service reso-grafana.service
  systemctl --user restart reso-prometheus.service reso-grafana.service
  echo "Prometheus: http://127.0.0.1:9190 (scraping $target)"
  echo "Grafana:    http://127.0.0.1:3000/d/reso-ingest-serve"
}

remove() {
  systemctl --user disable --now reso-prometheus.service reso-grafana.service 2>/dev/null || true
  rm -f "$UNITS/reso-prometheus.service" "$UNITS/reso-grafana.service"
  systemctl --user daemon-reload
  echo "units removed; binaries and data are still in $DIR (delete it to free the space)"
}

case ${1:-} in
  install)
    shift
    install "$@"
    ;;
  remove) remove ;;
  *)
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
