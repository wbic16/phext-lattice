#!/usr/bin/env bash
# Offline-Wikipedia benchmark: phext (libphext / phext-lattice / SQ) vs files vs SQLite vs ZIM.
#
#   bench/wiki/run_all.sh <work_dir> [scales...]      (default scales: 1 10)
#
# Needs: python3 + `pip install libzim zstandard`, a built SQ (`SQ_BIN`, default ../SQ/target/release/sq),
# and root if you want cold-cache numbers (drop_caches); otherwise "cold_cache": false is recorded.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
work="$(mkdir -p "$1" && cd "$1" && pwd)"; shift
scales="${*:-1 10}"
sq_bin="${SQ_BIN:-$repo/../SQ/target/release/sq}"
zim="$work/zim-testing-suite/data/nons/wikipedia_en_climate_change_mini_2024-06.zim"

[ -f "$zim" ] || git clone --depth 1 https://github.com/openzim/zim-testing-suite "$work/zim-testing-suite"
python3 -I "$here/extract_zim.py" "$zim" "$work/corpus"
python3 -I "$here/make_queries.py" "$work/corpus"
(cd "$repo" && cargo build --release --example wikibench)
bench="$repo/target/release/examples/wikibench"

for s in $scales; do
  out="$work/s$s"
  "$bench" build "$work/corpus" "$out" "$s"
  python3 -I "$here/zim_bench.py" "$work/corpus" "$out" "$s" --original "$zim" > /dev/null
  "$bench" run "$work/corpus" "$out" "$s" --sq "$sq_bin" > /dev/null
  "$bench" build "$work/corpus" "$out" "$s" > /dev/null     # run mutates the stores; measure pristine ones
  python3 -I "$here/storage.py" "$out" > /dev/null
done
python3 -I "$here/report.py" "$work" $scales > "$here/RESULTS.md"
echo "wrote $here/RESULTS.md"
