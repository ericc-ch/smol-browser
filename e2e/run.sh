#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build -p tinybrowser-cli --bins --no-default-features
export TINYBROWSER_BIN="${TINYBROWSER_BIN:-$ROOT/target/debug/tinybrowser}"
DRIVER="$ROOT/e2e/obstacle-course/run.py"

if command -v python3 >/dev/null 2>&1; then
  exec python3 "$DRIVER" --runs 1 --warmup 0 "$@"
fi

# Fall back to uv's managed Python when no system python3 exists.
exec uv run --python 3.12 python3 "$DRIVER" --runs 1 --warmup 0 "$@"
