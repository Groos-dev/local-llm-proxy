#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
RUN_DIR="$ROOT/.run"
PID_FILE="$RUN_DIR/local-llm-proxy.pid"
BACKUP_FILE="${LLPX_CODEX_BACKUP:-$RUN_DIR/codex-live-backup.json}"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

bind_addr="${BIND_ADDR:-}"
if [[ -z "$bind_addr" && -f "${CONFIG_PATH:-$ROOT/config.toml}" ]]; then
  bind_addr="$(sed -n 's/^[[:space:]]*bind_addr[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "${CONFIG_PATH:-$ROOT/config.toml}" | head -n 1)"
fi
bind_addr="${bind_addr:-127.0.0.1:8787}"
port="${bind_addr##*:}"

stopped=0

if [[ -f "$PID_FILE" ]]; then
  pid="$(cat "$PID_FILE")"
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null || true
    fi
    echo "stopped pid=$pid"
    stopped=1
  fi
  rm -f "$PID_FILE"
fi

if command -v lsof >/dev/null 2>&1; then
  for pid in $(lsof -t -iTCP:"$port" -sTCP:LISTEN 2>/dev/null || true); do
    kill "$pid" 2>/dev/null || true
    echo "stopped listener pid=$pid port=$port"
    stopped=1
  done
fi

if [[ -f "$BACKUP_FILE" ]]; then
  if [[ ! -x "$ROOT/target/debug/local-llm-proxy" ]]; then
    cargo build -q --bin local-llm-proxy
  fi
  LLPX_RESTORE_CODEX_LIVE=1 LLPX_CODEX_BACKUP="$BACKUP_FILE" \
    "$ROOT/target/debug/local-llm-proxy"
fi

if [[ "$stopped" -eq 0 ]]; then
  echo "not running"
fi

rm -rf "$RUN_DIR/exchanges"
echo "cleared $RUN_DIR/exchanges"
