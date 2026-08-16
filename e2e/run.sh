#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build -p obscura-cli --bins --no-default-features
export OBSCURA_BIN="${OBSCURA_BIN:-$ROOT/target/debug/obscura}"
exec python3 "$ROOT/e2e/obstacle-course/run.py" --runs 1 --warmup 0 "$@"
