#!/usr/bin/env bash
# Run the local editor regression and print the path/latency table.
# Usage: bash scripts/e2e.sh
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test --test e2e_local -- --nocapture "$@"
