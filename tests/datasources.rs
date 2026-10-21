//! CSV and Parquet data source tests.

use sqlengine::datasource::{csv, parquet};
use sqlengine::types::{DataType, ScalarValue, parse_date};
use sqlengine::{Session, SessionConfig};
use std::path::PathBuf;

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlengine-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn session() -> Session {
    Session::with_config(SessionConfig {
        target_partitions: 3,
        optimize: true,
    })
}

/// A Parquet file written by DuckDB (dictionary pages, DECIMAL, DATE, nulls,
/// three row groups) must decode to the same aggregates DuckDB computes.
#[test]
fn reads_duckdb_written_parquet() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/duckdb_types.parquet");
    let t = parquet::read_parquet(path).unwrap();
    let types: Vec<(String, DataType)> = t.schema.fields.iter().map(|f| (f.name.clone(), f.data_type)).collect();
    assert_eq!(
        types,
        vec![
            ("i32".into(), DataType::Int64),
            ("i64".into(), DataType::Int64),
            ("dbl".into(), DataType::Float64),
            ("dec".into(), DataType::Float64),
            ("dt".into(), DataType::Date),
            ("name".into(), DataType::Utf8),
            ("flag".into(), DataType::Boolean),
            ("maybe".into(), DataType::Int64),
        ]
    );
    let mut s = session();
    s.register_table("p", t);
    let r = s
        .sql(
            "SELECT count(*), sum(i32), sum(i64), round(sum(dbl), 4), sum(dec), min(dt), max(dt), count(name),
                    count(DISTINCT name), sum(CASE WHEN flag THEN 1 ELSE 0 END), count(maybe), sum(maybe) FROM p",
        )
        .unwrap();
    // Expected values computed by DuckDB 1.5 on the same file.
    assert_eq!(
        r.rows()[0],
        vec![
            ScalarValue::Int64(3000),
            ScalarValue::Int64(4_498_500),
            ScalarValue::Int64(4_498_500_031_489_500),
            ScalarValue::Float64(642642.8571),
            ScalarValue::Float64(5_623_125.0),
            ScalarValue::Date(parse_date("2020-01-01").unwrap()),
            ScalarValue::Date(parse_date("2028-03-18").unwrap()),
            ScalarValue::Int64(2400),
            ScalarValue::Int64(13),
            ScalarValue::Int64(1500),
            ScalarValue::Int64(2000),
            ScalarValue::Int64(3_000_000),
        ]
    );
    // Direct file queries work too.
    let sql = format!("SELECT name, count(*) FROM '{path}' WHERE name IS NOT NULL GROUP BY name ORDER BY name LIMIT 2");
    let r = s.sql(&sql).unwrap();
    assert_eq!(r.num_rows(), 2);
}

#[test]
fn parquet_roundtrip_with_nulls() {
    let mut s = session();
    s.sql("CREATE TABLE t (a INTEGER, b DOUBLE, c VARCHAR, d DATE, e BOOLEAN)")
        .unwrap();
    s.sql("INSERT INTO t VALUES (1, 1.5, 'x', DATE '2020-01-01', true), (NULL, NULL, NULL, NULL, NULL), (3, -2.25, '', DATE '1969-07-20', false)")
        .unwrap();
    let before = s.sql("SELECT * FROM t").unwrap();
    let path = temp_path("roundtrip.parquet");
    parquet::write_parquet(&before.schema, &before.batches, path.to_str().unwrap(), 2).unwrap();
    let t = parquet::read_parquet(path.to_str().unwrap()).unwrap();
    s.register_table("t2", t);
    let after = s.sql("SELECT * FROM t2").unwrap();
    assert_eq!(before.rows(), after.rows());
}

#[test]
fn csv_roundtrip_and_direct_query() {
    let mut s = session();
    s.sql("CREATE TABLE t (id INTEGER, name VARCHAR, price DOUBLE, day DATE)")
        .unwrap();
    s.sql("INSERT INTO t VALUES (1, 'plain', 9.5, DATE '2021-05-01'), (2, 'has, comma', NULL, DATE '2021-05-02'), (3, 'has \"quote\"', 3.0, NULL)")
        .unwrap();
    let before = s.sql("SELECT * FROM t ORDER BY id").unwrap();
    let path = temp_path("roundtrip.csv");
    csv::write_csv(&before.schema, &before.batches, path.to_str().unwrap()).unwrap();
    let t = csv::read_csv(path.to_str().unwrap(), &csv::CsvOptions::default()).unwrap();
    let types: Vec<DataType> = t.schema.fields.iter().map(|f| f.data_type).collect();
    assert_eq!(
        types,
        vec![DataType::Int64, DataType::Utf8, DataType::Float64, DataType::Date]
    );
    let r = s
        .sql(&format!("SELECT * FROM '{}' ORDER BY id", path.display()))
        .unwrap();
    assert_eq!(before.rows(), r.rows());
}

#[test]
fn large_csv_parallel_parse() {
    // > 4 MiB without quotes triggers the parallel chunked parser.
    let path = temp_path("large.csv");
    let mut text = String::from("k,v,s\n");
    for i in 0..300_000 {
        text.push_str(&format!("{i},{},{}\n", i % 7, if i % 11 == 0 { "" } else { "abc" }));
    }
    std::fs::write(&path, text).unwrap();
    let mut s = session();
    s.register_csv("big", path.to_str().unwrap()).unwrap();
    let r = s.sql("SELECT count(*), sum(k), sum(v), count(s) FROM big").unwrap();
    let expected_nulls = (0..300_000).filter(|i| i % 11 == 0).count() as i64;
    assert_eq!(
        r.rows()[0],
        vec![
            ScalarValue::Int64(300_000),
            ScalarValue::Int64((0..300_000i64).sum()),
            ScalarValue::Int64((0..300_000i64).map(|i| i % 7).sum()),
            ScalarValue::Int64(300_000 - expected_nulls),
        ]
    );
}

#[test]
fn missing_file_reports_error() {
    let mut s = session();
    let e = s.sql("SELECT * FROM 'does/not/exist.csv'").unwrap_err();
    assert!(e.to_string().contains("does/not/exist.csv"), "{e}");
    assert!(e.span.is_some());
}
