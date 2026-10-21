//! Partitioned (multi-threaded) execution must produce exactly the same
//! results as single-partition execution. Tables here span many batches so
//! scans really are split across threads and partial aggregates are merged.

use sqlengine::catalog::MemTable;
use sqlengine::column::{BATCH_SIZE, Batch, ColumnBuilder, Field, Schema};
use sqlengine::types::{DataType, ScalarValue};
use sqlengine::util::Rng;
use sqlengine::{Session, SessionConfig};

fn table(rows: usize, seed: u64) -> MemTable {
    let mut rng = Rng::new(seed);
    let schema = Schema::new(vec![
        Field::new("k", DataType::Int64, true),
        Field::new("s", DataType::Utf8, true),
        Field::new("v", DataType::Float64, true),
        Field::new("d", DataType::Date, true),
    ]);
    let mut batches = vec![];
    let mut done = 0;
    while done < rows {
        let n = BATCH_SIZE.min(rows - done);
        let mut k = ColumnBuilder::new(DataType::Int64);
        let mut s = ColumnBuilder::new(DataType::Utf8);
        let mut v = ColumnBuilder::new(DataType::Float64);
        let mut d = ColumnBuilder::new(DataType::Date);
        for _ in 0..n {
            if rng.bool(0.05) {
                k.push_null()
            } else {
                k.push_i64(rng.range(0, 5000))
            }
            if rng.bool(0.05) {
                s.push_null()
            } else {
                s.push_str(["red", "green", "blue", "cyan"][rng.range(0, 3) as usize])
            }
            if rng.bool(0.05) {
                v.push_null()
            } else {
                v.push_f64(rng.range(0, 1000) as f64 / 8.0)
            }
            d.push_date(rng.range(9000, 9400) as i32);
        }
        batches.push(Batch::from_columns(vec![
            k.finish(),
            s.finish(),
            v.finish(),
            d.finish(),
        ]));
        done += n;
    }
    MemTable::new(schema, batches)
}

fn sorted_rows(s: &mut Session, sql: &str) -> Vec<String> {
    let mut rows: Vec<String> = s
        .sql(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    // Float sums may differ in the last bits with a different summation order.
                    ScalarValue::Float64(f) => format!("{f:.6}"),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn partitioned_execution_matches_serial() {
    let queries = [
        "SELECT k, count(*), sum(v), min(s), max(d) FROM a GROUP BY k",
        "SELECT s, count(DISTINCT k), avg(v), count(v) FROM a GROUP BY s",
        "SELECT s, d, count(*) FROM a WHERE v > 10 GROUP BY s, d",
        "SELECT count(*), sum(v), min(k), max(k) FROM a",
        "SELECT a.s, count(*), sum(b.v) FROM a JOIN b ON a.k = b.k GROUP BY a.s",
        "SELECT a.k, b.s FROM a LEFT JOIN b ON a.k = b.k AND b.v > 100 WHERE a.v < 3",
        "SELECT k, v FROM a ORDER BY v DESC, k LIMIT 25",
        "SELECT s, sum(v) AS t FROM a GROUP BY s HAVING sum(v) > 0 ORDER BY t",
    ];
    let a = table(60_000, 1);
    let b = table(20_000, 2);
    let mut serial = Session::with_config(SessionConfig {
        target_partitions: 1,
        optimize: true,
    });
    let mut parallel = Session::with_config(SessionConfig {
        target_partitions: 8,
        optimize: true,
    });
    for s in [&mut serial, &mut parallel] {
        s.register_table("a", a.clone());
        s.register_table("b", b.clone());
    }
    for q in queries {
        let x = sorted_rows(&mut serial, q);
        let y = sorted_rows(&mut parallel, q);
        assert!(!x.is_empty(), "{q} returned nothing");
        assert_eq!(x, y, "partitioned result differs for {q}");
    }
}
