#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
RUN_DIR="$ROOT/.run"
PID_FILE="$RUN_DIR/local-llm-proxy.pid"
LOG_FILE="$RUN_DIR/local-llm-proxy.log"

cd "$ROOT"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

: "${ADA_API_KEY:?ADA_API_KEY must be set (export it or put it in .env)}"
export ADA_API_KEY
export BIND_ADDR="${BIND_ADDR:-127.0.0.1:8787}"
export ADA_BASE_URL="${ADA_BASE_URL:-http://ada-cli-golang.ctripcorp.com/coding-plan/openai/v1}"

if [[ -f "$PID_FILE" ]]; then
  old_pid="$(cat "$PID_FILE")"
  if kill -0 "$old_pid" 2>/dev/null; then
    echo "already running pid=$old_pid bind=$BIND_ADDR"
    exit 0
  fi
  rm -f "$PID_FILE"
fi

mkdir -p "$RUN_DIR"
rm -rf "$RUN_DIR/exchanges"
mkdir -p "$RUN_DIR/exchanges"
: >"$LOG_FILE"
cargo build -q

export EXCHANGE_LOG_DIR="${EXCHANGE_LOG_DIR:-$RUN_DIR/exchanges}"
nohup "$ROOT/target/debug/local-llm-proxy" >"$LOG_FILE" 2>&1 &
echo $! >"$PID_FILE"

host="${BIND_ADDR%:*}"
port="${BIND_ADDR##*:}"
for _ in $(seq 1 40); do
  if curl -sS -m 1 "http://${host}:${port}/v1/models" >/dev/null 2>&1; then
    echo "started pid=$(cat "$PID_FILE") bind=$BIND_ADDR log=$LOG_FILE exchanges=$EXCHANGE_LOG_DIR"
    exit 0
  fi
  sleep 0.15
done

echo "started but /v1/models not ready yet; pid=$(cat "$PID_FILE") log=$LOG_FILE" >&2
exit 1
