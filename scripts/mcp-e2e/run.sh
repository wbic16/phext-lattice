#!/usr/bin/env bash
# MCP end-to-end harness: builds phext-edit, spawns a local instance over
# generated fixtures, and replays the real client dialects (Cursor DCR,
# Claude CIMD, ChatGPT CIMD + private_key_jwt) plus transport-conformance
# checks and per-step latency budgets. See README.md in this directory.
#
# Usage: bash scripts/mcp-e2e/run.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build --release --bin phext-edit

WORK=$(mktemp -d)
SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

python3 - "$WORK" <<'PY'
import sys
d = sys.argv[1]
open(d + "/small.phext", "w").write("home scroll text\n")
# 12000 scrolls: enough to prove phext_toc paging (default page 10000)
# and the large-file cache behavior without a production-size fixture.
scrolls = [f"scroll {i} needle-{i % 50} lorem ipsum phext" for i in range(12000)]
open(d + "/big.phext", "w").write("\x01".join(scrolls))
PY

PORT=18917
PHEXT_PUBLIC_URL=http://127.0.0.1:$PORT \
PHEXT_SESSION_SECRET=mcp-e2e-harness-secret-0123456789 \
  ./target/release/phext-edit --local --dir "$WORK" --home small.phext --port $PORT &
SERVER_PID=$!

for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/.well-known/oauth-authorization-server" >/dev/null && break
  sleep 0.2
done

python3 scripts/mcp-e2e/mcp_e2e.py "http://127.0.0.1:$PORT"
