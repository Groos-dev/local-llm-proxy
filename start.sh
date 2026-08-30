#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
RUN_DIR="$ROOT/.run"
PID_FILE="$RUN_DIR/local-llm-proxy.pid"
LOG_FILE="$RUN_DIR/local-llm-proxy.log"
BACKUP_FILE="${LLPX_CODEX_BACKUP:-$RUN_DIR/codex-live-backup.json}"

cd "$ROOT"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

export CONFIG_PATH="${CONFIG_PATH:-$ROOT/config.toml}"
# Prefer JSON store (llpx configure / provider upsert). Falls back to migrating TOML.
if [[ -z "${LLPX_STORE:-}" ]]; then
  if [[ -f "$HOME/.llpx/store.json" ]]; then
    export LLPX_STORE="$HOME/.llpx/store.json"
  elif [[ -f "$RUN_DIR/llpx-store.json" ]]; then
    export LLPX_STORE="$RUN_DIR/llpx-store.json"
  fi
fi
if [[ -n "${BIND_ADDR:-}" ]]; then
  export BIND_ADDR
fi

bind_addr="${BIND_ADDR:-}"
if [[ -z "$bind_addr" && -f "$CONFIG_PATH" ]]; then
  bind_addr="$(sed -n 's/^[[:space:]]*bind_addr[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$CONFIG_PATH" | head -n 1)"
fi
bind_addr="${bind_addr:-127.0.0.1:8787}"

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE")"
  if kill -0 "$old_pid" 2>/dev/null; then
    echo "already running pid=$old_pid bind=$bind_addr"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

mkdir -p "$RUN_DIR"
rm -rf "$RUN_DIR/exchanges"
mkdir -p "$RUN_DIR/exchanges"
: >"$LOG_FILE"
cargo build -q
cargo build -q --bin llpx
cp "$ROOT/target/debug/llpx" "$ROOT/llpx"
chmod +x "$ROOT/llpx"

export EXCHANGE_LOG_DIR="${EXCHANGE_LOG_DIR:-$RUN_DIR/exchanges}"
proxy_base="http://${bind_addr}/v1"

if [[ "${LLPX_SKIP_CODEX_LIVE:-}" != "1" ]]; then
  export LLPX_APPLY_CODEX_LIVE=1
  export LLPX_CODEX_BACKUP="$BACKUP_FILE"
  export LLPX_PROXY_BASE="$proxy_base"
else
  unset LLPX_APPLY_CODEX_LIVE LLPX_CODEX_BACKUP LLPX_PROXY_BASE || true
fi

nohup "$ROOT/target/debug/local-llm-proxy" >"$LOG_FILE" 2>&1 &
echo $! >"$PID_FILE"

host="${bind_addr%:*}"
port="${bind_addr##*:}"
for _ in $(seq 1 40); do
  if curl -sS -m 1 "http://${host}:${port}/health" >/dev/null 2>&1; then
    echo "started pid=$(cat "$PID_FILE") bind=$bind_addr log=$LOG_FILE exchanges=$EXCHANGE_LOG_DIR"
    exit 0
  fi
  sleep 0.15
done

echo "started but /health not ready yet; pid=$(cat "$PID_FILE") log=$LOG_FILE" >&2
exit 1
