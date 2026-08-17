#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd $(dirname $0) && pwd)
cd $ROOT

usage() {
  cat <<'EOF'
Usage:
  ./provider.sh list
  ./provider.sh set <public_model> <provider> <upstream_model>
  ./provider.sh unset <public_model>
  ./provider.sh -h|--help

Dynamic model routing for local-llm-proxy. Thin wrapper around llpx CLI.
EOF
}

bin=$ROOT/target/debug/llpx
if [ ! -x $bin ]; then
  cargo build --bin llpx -q
fi

base=${PROXY_BASE:-}
if [ -n "$base" ]; then
  set -- --base "$base" "$@"
fi

cmd=${1:-}
case $cmd in
  -h|--help|help)
    usage
    exit 0
    ;;
  list|set|unset)
    $bin "$@"
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
