#!/usr/bin/env bash
# Start, stop, or status one local ehgraph instance.
# Usage: scripts/ehgraph-local.sh <start|stop|status> <network|noetl-cluster>
set -euo pipefail
action="${1:?start, stop, or status}"
domain="${2:?domain: network or noetl-cluster}"
root="$(cd "$(dirname "$0")/.." && pwd)"
home_dir="${EHGRAPH_HOME:-$root/.local}"
instance="ehgraph-${domain}"
case "$domain" in
  network) port=8081 ;;
  noetl-cluster) port=8082 ;;
  *) echo "unknown domain $domain" >&2; exit 1 ;;
esac
bin="${EHGRAPH_BIN:-$root/target/debug/ehgraph}"
dir="$home_dir/$instance"
cfg="$dir/config.toml"
pidfile="$dir/ehgraph.pid"
log="$dir/ehgraph.log"

write_config() {
  mkdir -p "$dir/data" "$dir/backup"
  if [[ -f "$cfg" ]]; then
    return
  fi
  cat > "$cfg" <<EOF
instance_id = "$instance"
domain = "$domain"
data_dir = "$dir/data"
backup_prefix = "$dir/backup"
listen = "127.0.0.1:$port"
role = "primary"
build = "local"
backup_interval_s = 0

[auth]
mode = "disabled"
EOF
}

case "$action" in
  start)
    write_config
    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
      echo "$instance already running (pid $(cat "$pidfile"))"
      exit 0
    fi
    if [[ ! -x "$bin" ]]; then
      echo "building $bin"
      (cd "$root" && cargo build -p ehgraph-server)
    fi
    nohup "$bin" serve --config "$cfg" > "$log" 2>&1 &
    echo $! > "$pidfile"
    echo "$instance listening on 127.0.0.1:$port (pid $(cat "$pidfile"))"
    ;;
  stop)
    if [[ -f "$pidfile" ]]; then
      kill "$(cat "$pidfile")" 2>/dev/null || true
      rm -f "$pidfile"
    fi
    echo "$instance stopped"
    ;;
  status)
    curl -fsS "http://127.0.0.1:$port/healthz" || {
      echo "$instance is not answering on $port" >&2
      exit 1
    }
    echo
    ;;
  *)
    echo "usage: $0 <start|stop|status> <network|noetl-cluster>" >&2
    exit 1
    ;;
esac
