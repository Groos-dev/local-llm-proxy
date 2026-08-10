#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

usage() {
  cat <<'EOF'
Usage:
  ./provider.sh list
  ./provider.sh use <provider>
  ./provider.sh -h|--help

Show or switch the runtime active provider via /v1/admin/active-provider.

Base URL resolution (first match wins):
  1. PROXY_BASE
  2. http://$bind_addr from CONFIG_PATH / BIND_ADDR (same as start.sh)
  3. http://127.0.0.1:8787
EOF
}

resolve_base() {
  if [[ -n "${PROXY_BASE:-}" ]]; then
    printf '%s\n' "${PROXY_BASE%/}"
    return
  fi

  export CONFIG_PATH="${CONFIG_PATH:-$ROOT/config.toml}"
  local bind_addr="${BIND_ADDR:-}"
  if [[ -z "$bind_addr" && -f "$CONFIG_PATH" ]]; then
    bind_addr="$(sed -n 's/^[[:space:]]*bind_addr[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$CONFIG_PATH" | head -n 1)"
  fi
  bind_addr="${bind_addr:-127.0.0.1:8787}"
  printf 'http://%s\n' "$bind_addr"
}

print_status() {
  python3 -c '
import json, sys
data = json.load(sys.stdin)
active = data.get("provider") or ""
providers = data.get("providers") or []
for name in providers:
    mark = "*" if name == active else " "
    print(f"{mark} {name}")
if not providers:
    print(f"* {active}" if active else "(no providers)")
'
}

api_get() {
  local url="$1"
  local tmp status
  tmp="$(mktemp)"
  status="$(curl -sS -o "$tmp" -w '%{http_code}' "$url")" || {
    rm -f "$tmp"
    echo "failed to reach $url" >&2
    exit 1
  }
  if [[ "$status" != "200" ]]; then
    echo "GET $url -> HTTP $status" >&2
    cat "$tmp" >&2 || true
    rm -f "$tmp"
    exit 1
  fi
  cat "$tmp"
  rm -f "$tmp"
}

api_put() {
  local url="$1"
  local body="$2"
  local tmp status
  tmp="$(mktemp)"
  status="$(curl -sS -o "$tmp" -w '%{http_code}' \
    -X PUT \
    -H 'Content-Type: application/json' \
    -d "$body" \
    "$url")" || {
    rm -f "$tmp"
    echo "failed to reach $url" >&2
    exit 1
  }
  if [[ "$status" != "200" ]]; then
    echo "PUT $url -> HTTP $status" >&2
    cat "$tmp" >&2 || true
    rm -f "$tmp"
    exit 1
  fi
  cat "$tmp"
  rm -f "$tmp"
}

cmd="${1:-}"
case "$cmd" in
  -h|--help|help)
    usage
    exit 0
    ;;
  list)
    base="$(resolve_base)"
    resp="$(api_get "$base/v1/admin/active-provider")"
    printf '%s\n' "$resp" | print_status
    ;;
  use)
    name="${2:-}"
    if [[ -z "$name" ]]; then
      echo "missing provider name" >&2
      usage >&2
      exit 1
    fi
    base="$(resolve_base)"
    body="$(PROVIDER_NAME="$name" python3 -c 'import json,os; print(json.dumps({"provider": os.environ["PROVIDER_NAME"]}))')"
    resp="$(api_put "$base/v1/admin/active-provider" "$body")"
    printf '%s\n' "$resp" | print_status
    ;;
  "")
    usage >&2
    exit 1
    ;;
  *)
    echo "unknown command: $cmd" >&2
    usage >&2
    exit 1
    ;;
esac
