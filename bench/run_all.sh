#!/usr/bin/env bash
# Reproduce every benchmark number in the README.
#   bench/run_all.sh            # writes bench/results/latest.md
# Requires: Rust toolchain; optional: DuckDB CLI on PATH (for the comparison).
set -euo pipefail
cd "$(dirname "$0")/.."
RUNS=${RUNS:-10}
OUT=bench/results/latest.md
mkdir -p bench/results

cargo build --release --bins

for sf in 0.1 1 3; do
  [ -f "data/sf$sf/lineitem.parquet" ] || ./target/release/tpchgen --sf "$sf" --out "data/sf$sf" --format parquet
done
[ -f data/sf1/lineitem.csv ] || ./target/release/tpchgen --sf 1 --out data/sf1 --format both

{
  echo "# Benchmark run $(date '+%Y-%m-%d %H:%M')"
  echo
  echo "- machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu 2>/dev/null || nproc) cores, $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 )) GiB"
  echo "- load average at start: $(uptime | sed 's/.*load averages*: //')"
  echo "- runs per query: $RUNS (after one warm-up run)"
  for sf in 0.1 1 3; do
    echo; echo "## sqlengine, SF $sf (Parquet -> memory)"; echo
    ./target/release/bench --data "data/sf$sf" --format parquet --runs "$RUNS"
    if command -v duckdb >/dev/null; then
      echo; echo "## DuckDB, SF $sf (Parquet -> in-memory tables)"; echo
      python3 bench/duckdb_compare.py "data/sf$sf" "$RUNS"
    fi
  done
  echo; echo "## Thread scaling, SF 1"; echo
  ./target/release/bench --data data/sf1 --format parquet --runs 5 --threads 1,2,4,8,"$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
  echo; echo "## CSV load (SF 1)"; echo
  ./target/release/bench --data data/sf1 --format csv --runs 3 --queries Q6
  echo; echo "- load average at end: $(uptime | sed 's/.*load averages*: //')"
} | tee "$OUT"
