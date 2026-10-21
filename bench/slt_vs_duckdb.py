#!/usr/bin/env python3
"""Cross-check the expected results in tests/slt/*.slt against DuckDB.

Every `query` record (except EXPLAIN) is executed in DuckDB after replaying the
file's `statement ok` records; results are normalised the same way the Rust
runner prints them and compared. Usage: python3 bench/slt_vs_duckdb.py
"""
import csv, glob, io, os, subprocess, sys

def parse(path):
    lines = open(path).read().splitlines()
    i, recs = 0, []
    while i < len(lines):
        l = lines[i].strip()
        if not l or l.startswith('#'):
            i += 1; continue
        header = l.split(); i += 1
        sql = []
        while i < len(lines) and lines[i].strip() and lines[i].strip() != '----':
            sql.append(lines[i]); i += 1
        exp = []
        if header[0] == 'query' and i < len(lines) and lines[i].strip() == '----':
            i += 1
            while i < len(lines) and lines[i].strip():
                exp.append(lines[i].rstrip()); i += 1
        recs.append((header, '\n'.join(sql), exp))
    return recs

# Differences that are expected and documented (DuckDB returns DOUBLE for
# ceil(BIGINT); sqlengine keeps both USING columns in SELECT *).
KNOWN = {"SELECT abs(-2.5), round(2.567, 2), floor(2.7), ceil(2)", "SELECT * FROM l LEFT JOIN r USING (k)"}

def norm(v, ty):
    if v.endswith(' 00:00:00'): v = v[:-9]  # DuckDB: DATE + INTERVAL -> TIMESTAMP
    if v == 'NULL': return 'NULL'
    if v == '': return '(empty)'
    if ty == 'R':
        return '%.3f' % float(v)
    if ty == 'I':
        if v in ('true', 'false'): return v
        try:
            f = float(v)
            return str(int(f)) if f == int(f) and '.' not in v else '%.3f' % f
        except ValueError:
            return v
    return v

def run(setup, sql):
    script = "SET integer_division = true;\n" + ''.join(s + ';\n' for s in setup) + ".mode csv\n.nullvalue NULL\n.headers off\n" + sql + ';\n'
    p = subprocess.run(['duckdb', '-batch'], input=script, capture_output=True, text=True)
    if p.returncode != 0 or 'Error' in p.stderr:
        return None, p.stderr.strip().splitlines()[-1] if p.stderr.strip() else 'error'
    return list(csv.reader(io.StringIO(p.stdout))), None

total = mism = skipped = known = 0
for path in sorted(glob.glob(os.path.join(os.path.dirname(__file__), '..', 'tests', 'slt', '*.slt'))):
    setup = []
    for header, sql, exp in parse(path):
        if header[0] == 'statement':
            if header[1] == 'ok': setup.append(sql)
            continue
        if sql.lstrip().upper().startswith('EXPLAIN'):
            skipped += 1; continue
        total += 1
        types = header[1]; rowsort = len(header) > 2 and header[2] == 'rowsort'
        rows, err = run(setup, sql)
        if rows is None:
            mism += 1; print(f'--- {os.path.basename(path)}: DuckDB error: {err}\n{sql}\n'); continue
        got = [' '.join(norm(v, types[i] if i < len(types) else 'T') for i, v in enumerate(r)) for r in rows]
        if rowsort: got.sort()
        if got != exp and sql in KNOWN:
            known += 1; continue
        if got != exp:
            mism += 1
            print(f'--- {os.path.basename(path)}: mismatch\n{sql}\nexpected (sqlengine): {exp}\nduckdb:               {got}\n')
print(f'{total} queries compared against DuckDB: {mism} unexpected differences, {known} known/documented differences, {skipped} EXPLAIN skipped')
sys.exit(1 if mism else 0)
