# Benchmark run 2026-10-15 13:07

- machine: Apple M4, 10 cores, 16 GiB
- load average at start: 1.52 2.42 2.93
- runs per query: 10 (after one warm-up run)

## sqlengine, SF 0.1 (Parquet -> memory)

loaded customer       15000 rows from parquet in     8.4 ms
loaded orders        150000 rows from parquet in    33.9 ms
loaded lineitem      600402 rows from parquet in   104.2 ms

| query | threads | rows | min (ms) | median (ms) |
|-------|--------:|-----:|---------:|------------:|
| Q1 | 10 | 4 | 2.5 | 2.5 |
| Q3 | 10 | 10 | 1.0 | 1.1 |
| Q6 | 10 | 1 | 0.2 | 0.2 |
| Q12 | 10 | 2 | 1.4 | 1.4 |

## DuckDB, SF 0.1 (Parquet -> in-memory tables)

DuckDB v1.5.6 (Variegata) 069cc9f9b5 — data/sf0.1, 10 runs after warm-up

| query | duckdb min (ms) | duckdb median (ms) | results match |
|-------|----------------:|-------------------:|:-------------:|
| Q1 | 5.0 | 6.0 | yes |
| Q3 | 3.0 | 4.0 | yes |
| Q6 | 0.0 | 1.0 | yes |
| Q12 | 4.0 | 4.0 | yes |

## sqlengine, SF 1 (Parquet -> memory)

loaded customer      150000 rows from parquet in    36.9 ms
loaded orders       1500000 rows from parquet in   161.3 ms
loaded lineitem     6000098 rows from parquet in   343.2 ms

| query | threads | rows | min (ms) | median (ms) |
|-------|--------:|-----:|---------:|------------:|
| Q1 | 10 | 4 | 22.5 | 22.8 |
| Q3 | 10 | 10 | 7.8 | 7.8 |
| Q6 | 10 | 1 | 1.7 | 1.7 |
| Q12 | 10 | 2 | 11.2 | 11.3 |

## DuckDB, SF 1 (Parquet -> in-memory tables)

DuckDB v1.5.6 (Variegata) 069cc9f9b5 — data/sf1, 10 runs after warm-up

| query | duckdb min (ms) | duckdb median (ms) | results match |
|-------|----------------:|-------------------:|:-------------:|
| Q1 | 28.0 | 29.0 | yes |
| Q3 | 11.0 | 12.0 | yes |
| Q6 | 4.0 | 4.0 | yes |
| Q12 | 14.0 | 15.0 | yes |

## sqlengine, SF 3 (Parquet -> memory)

loaded customer      450000 rows from parquet in   108.7 ms
loaded orders       4500000 rows from parquet in   232.0 ms
loaded lineitem    17997454 rows from parquet in  1212.7 ms

| query | threads | rows | min (ms) | median (ms) |
|-------|--------:|-----:|---------:|------------:|
| Q1 | 10 | 4 | 66.7 | 68.0 |
| Q3 | 10 | 10 | 28.2 | 30.5 |
| Q6 | 10 | 1 | 5.2 | 6.0 |
| Q12 | 10 | 2 | 34.2 | 34.3 |

## DuckDB, SF 3 (Parquet -> in-memory tables)

DuckDB v1.5.6 (Variegata) 069cc9f9b5 — data/sf3, 10 runs after warm-up

| query | duckdb min (ms) | duckdb median (ms) | results match |
|-------|----------------:|-------------------:|:-------------:|
| Q1 | 80.0 | 83.0 | yes |
| Q3 | 27.0 | 28.0 | yes |
| Q6 | 11.0 | 11.5 | yes |
| Q12 | 39.0 | 40.5 | yes |

## Thread scaling, SF 1

loaded customer      150000 rows from parquet in    42.2 ms
loaded orders       1500000 rows from parquet in   173.3 ms
loaded lineitem     6000098 rows from parquet in   308.0 ms

| query | threads | rows | min (ms) | median (ms) |
|-------|--------:|-----:|---------:|------------:|
| Q1 | 1 | 4 | 133.8 | 134.2 |
| Q3 | 1 | 10 | 33.3 | 33.5 |
| Q6 | 1 | 1 | 6.6 | 6.6 |
| Q12 | 1 | 2 | 75.2 | 75.6 |
| Q1 | 2 | 4 | 67.8 | 67.9 |
| Q3 | 2 | 10 | 18.3 | 18.5 |
| Q6 | 2 | 1 | 3.7 | 3.7 |
| Q12 | 2 | 2 | 38.7 | 38.9 |
| Q1 | 4 | 4 | 35.9 | 36.5 |
| Q3 | 4 | 10 | 11.0 | 11.1 |
| Q6 | 4 | 1 | 2.5 | 2.5 |
| Q12 | 4 | 2 | 20.3 | 20.3 |
| Q1 | 8 | 4 | 25.5 | 25.6 |
| Q3 | 8 | 10 | 8.5 | 8.6 |
| Q6 | 8 | 1 | 1.8 | 1.8 |
| Q12 | 8 | 2 | 13.0 | 13.1 |
| Q1 | 10 | 4 | 22.4 | 22.5 |
| Q3 | 10 | 10 | 8.1 | 8.2 |
| Q6 | 10 | 1 | 1.7 | 1.8 |
| Q12 | 10 | 2 | 11.2 | 12.1 |

## CSV load (SF 1)

loaded customer      150000 rows from csv in    18.4 ms
loaded orders       1500000 rows from csv in   117.4 ms
loaded lineitem     6000098 rows from csv in   595.7 ms

| query | threads | rows | min (ms) | median (ms) |
|-------|--------:|-----:|---------:|------------:|
| Q6 | 10 | 1 | 1.8 | 1.8 |

- load average at end: 4.29 2.99 3.11

## Load average samples (1/5/15 min, every 10 s, separate sampler)

```
13:06:46 { 1.25 2.51 2.98 }
13:06:56 { 1.28 2.47 2.96 }
13:07:06 { 1.16 2.41 2.93 }
13:07:16 { 1.21 2.38 2.91 }
13:07:26 { 1.48 2.40 2.91 }
13:07:36 { 3.01 2.69 3.01 }
13:07:46 { 3.17 2.74 3.02 }
13:07:56 { 4.29 2.99 3.11 }
13:08:06 { 3.87 2.94 3.09 }
13:08:16 { 3.59 2.91 3.08 }
```
