#!/usr/bin/env python3
"""Benchmark the same TPC-H queries in DuckDB and cross-check results.

Usage: python3 bench/duckdb_compare.py data/sf1 [runs]

1. Runs `target/release/bench --csv-out` to get sqlengine's results.
2. Loads the Parquet files into DuckDB *in-memory tables* (so both engines
   query memory-resident data), runs every query once to warm up and then
   `runs` times, and reports min/median of DuckDB's `.timer` real time.
3. Compares DuckDB's result rows with sqlengine's (numeric tolerance 1e-6 rel).
"""
import csv, io, os, re, statistics, subprocess, sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..')
data = sys.argv[1] if len(sys.argv) > 1 else 'data/sf1'
runs = int(sys.argv[2]) if len(sys.argv) > 2 else 5
queries = ['q1', 'q3', 'q6', 'q12']
out_dir = os.path.join(root, 'target', 'bench-results')

subprocess.run([os.path.join(root, 'target/release/bench'), '--data', data, '--runs', '1', '--csv-out', out_dir], check=True, capture_output=True)

load = ''.join(f"CREATE TABLE {t} AS SELECT * FROM read_parquet('{data}/{t}.parquet');\n" for t in ['customer', 'orders', 'lineitem'])

def duck(script):
    p = subprocess.run(['duckdb', '-batch'], input=script, capture_output=True, text=True, cwd=root)
    if p.returncode != 0:
        sys.exit(p.stderr)
    return p.stdout

def close(a, b):
    if a == b:
        return True
    try:
        x, y = float(a), float(b)
        return abs(x - y) <= 1e-6 * max(abs(x), abs(y), 1.0)
    except ValueError:
        return False

version = subprocess.run(['duckdb', '-version'], capture_output=True, text=True).stdout.strip()
print(f'DuckDB {version} — {data}, {runs} runs after warm-up\n')
print('| query | duckdb min (ms) | duckdb median (ms) | results match |')
print('|-------|----------------:|-------------------:|:-------------:|')
for q in queries:
    sql = open(os.path.join(root, 'bench', 'queries', q + '.sql')).read().strip()
    timed = duck(load + sql + '\n.timer on\n' + '\n'.join([sql] * (runs + 1)) + '\n')
    times = [float(t) * 1e3 for t in re.findall(r'Run Time \(s\): real ([0-9.]+)', timed)][1:]
    rows = list(csv.reader(io.StringIO(duck(load + '.mode csv\n.headers off\n' + sql + '\n'))))
    ours = list(csv.reader(open(os.path.join(out_dir, q + '.csv'))))[1:]
    match = len(rows) == len(ours) and all(len(r) == len(o) and all(close(a, b) for a, b in zip(r, o)) for r, o in zip(rows, ours))
    print(f'| {q.upper()} | {min(times):.1f} | {statistics.median(times):.1f} | {"yes" if match else "NO"} |')
    if not match:
        print('  duckdb:', rows[:3], '\n  ours:  ', ours[:3])
