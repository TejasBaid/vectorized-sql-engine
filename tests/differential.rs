//! Differential testing against SQLite.
//!
//! Random tables (with NULLs and duplicate keys) are loaded into both engines,
//! then hundreds of randomly generated queries (filters, expressions,
//! three-valued logic, joins, aggregates, HAVING, subqueries, ORDER BY/LIMIT)
//! are executed on both and the results compared as multisets (or in order,
//! when the query has a total ORDER BY).

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use sqlengine::types::ScalarValue;
use sqlengine::util::Rng;
use sqlengine::{Session, SessionConfig};

#[derive(Debug, Clone, PartialEq, PartialOrd)]
enum V {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

fn from_ours(v: &ScalarValue) -> V {
    match v {
        ScalarValue::Null(_) => V::Null,
        ScalarValue::Boolean(b) => V::Int(*b as i64),
        ScalarValue::Int64(x) => V::Int(*x),
        ScalarValue::Float64(x) => V::Real(*x),
        ScalarValue::Utf8(s) => V::Text(s.clone()),
        other => V::Text(other.to_string()),
    }
}

fn same(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Real(x), V::Real(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        (V::Real(x), V::Int(y)) | (V::Int(y), V::Real(x)) => (x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        _ => a == b,
    }
}

fn sort_key(row: &[V]) -> String {
    row.iter()
        .map(|v| match v {
            V::Null => "\u{0}NULL".to_string(),
            V::Int(i) => format!("{:020.6}", *i as f64 + 1e12),
            V::Real(r) => format!("{:020.6}", r + 1e12),
            V::Text(s) => s.clone(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

const WORDS: [&str; 6] = ["apple", "banana", "cherry", "date", "Apple", "fig"];

struct Gen {
    rng: Rng,
}

impl Gen {
    fn int_col(&mut self, t: &str) -> String {
        let cols: &[&str] = if t == "t1" { &["a", "b"] } else { &["a", "e"] };
        format!("{t}.{}", self.rng.choose(cols))
    }

    fn int_lit(&mut self) -> String {
        self.rng.range(-3, 12).to_string()
    }

    fn int_expr(&mut self, t: &str, depth: u32) -> String {
        match self.rng.range(0, if depth == 0 { 1 } else { 4 }) {
            0 => self.int_col(t),
            1 => self.int_lit(),
            _ => {
                let op = *self.rng.choose(&["+", "-", "*", "/", "%"]);
                let l = self.int_expr(t, depth - 1);
                let r = self.int_expr(t, depth - 1);
                format!("({l} {op} {r})")
            }
        }
    }

    fn pred(&mut self, t: &str, depth: u32) -> String {
        let choice = self.rng.range(0, if depth == 0 { 4 } else { 7 });
        match choice {
            0 | 1 => {
                let op = *self.rng.choose(&["=", "<>", "<", "<=", ">", ">="]);
                let l = self.int_expr(t, 1);
                let r = self.int_expr(t, 1);
                format!("{l} {op} {r}")
            }
            2 => {
                let c = self.int_col(t);
                format!("{c} IS {}NULL", if self.rng.bool(0.5) { "NOT " } else { "" })
            }
            3 => {
                let c = self.int_col(t);
                let lo = self.rng.range(-2, 6);
                let hi = lo + self.rng.range(0, 6);
                format!(
                    "{c} {}BETWEEN {lo} AND {hi}",
                    if self.rng.bool(0.3) { "NOT " } else { "" }
                )
            }
            4 => {
                if t == "t1" && self.rng.bool(0.5) {
                    let p = *self.rng.choose(&["a%", "%e%", "_pple", "%a", "b%a"]);
                    format!("t1.c {}LIKE '{p}'", if self.rng.bool(0.3) { "NOT " } else { "" })
                } else {
                    let c = self.int_col(t);
                    let n = self.rng.range(1, 4);
                    let mut items: Vec<String> = (0..n).map(|_| self.int_lit()).collect();
                    if self.rng.bool(0.2) {
                        items.push("NULL".into());
                    }
                    format!(
                        "{c} {}IN ({})",
                        if self.rng.bool(0.3) { "NOT " } else { "" },
                        items.join(", ")
                    )
                }
            }
            5 => format!("({} AND {})", self.pred(t, depth - 1), self.pred(t, depth - 1)),
            6 => format!("({} OR {})", self.pred(t, depth - 1), self.pred(t, depth - 1)),
            _ => format!("NOT ({})", self.pred(t, depth - 1)),
        }
    }

    fn agg(&mut self, t: &str) -> String {
        let col = self.int_col(t);
        match self.rng.range(0, 7) {
            0 => "count(*)".into(),
            1 => format!("count({col})"),
            2 => format!("sum({col})"),
            3 => format!("min({col})"),
            4 => format!("max({col})"),
            5 => format!("avg({col})"),
            _ => format!("count(DISTINCT {col})"),
        }
    }

    /// Returns (sql, ordered) where `ordered` means rows must match in order.
    fn query(&mut self) -> (String, bool) {
        match self.rng.range(0, 8) {
            0 => {
                let e1 = self.int_expr("t1", 2);
                let e2 = self.int_expr("t1", 1);
                let p = self.pred("t1", 2);
                (format!("SELECT {e1}, {e2}, t1.c FROM t1 WHERE {p}"), false)
            }
            1 => {
                let p = self.pred("t1", 2);
                let a1 = self.agg("t1");
                let a2 = self.agg("t1");
                (format!("SELECT {a1}, {a2} FROM t1 WHERE {p}"), false)
            }
            2 => {
                let g = self.int_col("t1");
                let a1 = self.agg("t1");
                let a2 = self.agg("t1");
                let p = self.pred("t1", 1);
                let having = if self.rng.bool(0.5) {
                    format!(" HAVING count(*) > {}", self.rng.range(0, 3))
                } else {
                    String::new()
                };
                (
                    format!("SELECT {g}, {a1}, {a2} FROM t1 WHERE {p} GROUP BY {g}{having}"),
                    false,
                )
            }
            3 => {
                let p = self.pred("t1", 1);
                let q = self.pred("t2", 1);
                (
                    format!("SELECT t1.a, t1.b, t2.e, t2.f FROM t1 JOIN t2 ON t1.a = t2.a WHERE {p} AND {q}"),
                    false,
                )
            }
            4 => {
                let p = self.pred("t1", 1);
                let extra = if self.rng.bool(0.5) {
                    format!(" AND t2.e > {}", self.int_lit())
                } else {
                    String::new()
                };
                (
                    format!("SELECT t1.a, t1.c, t2.e FROM t1 LEFT JOIN t2 ON t1.a = t2.a{extra} WHERE {p}"),
                    false,
                )
            }
            5 => {
                let g = self.int_col("t1");
                let a = self.agg("t1");
                let lim = self.rng.range(1, 8);
                let dir = if self.rng.bool(0.5) { "ASC" } else { "DESC" };
                let nulls = if self.rng.bool(0.5) { "FIRST" } else { "LAST" };
                // Ordering by every output column gives a total order.
                (
                    format!(
                        "SELECT {g} AS g, {a} AS x FROM t1 GROUP BY {g} ORDER BY g {dir} NULLS {nulls}, x LIMIT {lim}"
                    ),
                    true,
                )
            }
            6 => {
                let a = self.agg("t1");
                let p = self.pred("t1", 1);
                let k = self.rng.range(0, 4);
                (
                    format!(
                        "SELECT s.k, s.v FROM (SELECT t1.b AS k, {a} AS v FROM t1 WHERE {p} GROUP BY t1.b) s WHERE s.v >= {k}"
                    ),
                    false,
                )
            }
            _ => {
                let p = self.pred("t1", 1);
                let a = self.agg("t2");
                (
                    format!("SELECT t1.c, {a} FROM t1 JOIN t2 ON t1.b = t2.e WHERE {p} GROUP BY t1.c"),
                    false,
                )
            }
        }
    }
}

fn value_sql(rng: &mut Rng, kind: char) -> String {
    if rng.bool(0.12) {
        return "NULL".into();
    }
    match kind {
        'i' => rng.range(-2, 10).to_string(),
        'r' => format!("{:.2}", rng.range(-20, 40) as f64 / 4.0),
        _ => format!("'{}'", rng.choose(&WORDS)),
    }
}

fn setup(seed: u64) -> (Session, Connection) {
    let mut rng = Rng::new(seed);
    let mut ours = Session::with_config(SessionConfig {
        target_partitions: 3,
        optimize: true,
    });
    let sqlite = Connection::open_in_memory().unwrap();
    sqlite.execute_batch("PRAGMA case_sensitive_like = ON;").unwrap();
    let ddl = [
        (
            "CREATE TABLE t1 (a INTEGER, b INTEGER, c VARCHAR, d DOUBLE)",
            "CREATE TABLE t1 (a INTEGER, b INTEGER, c TEXT, d REAL)",
            "t1",
            "iisr",
            300,
        ),
        (
            "CREATE TABLE t2 (a INTEGER, e INTEGER, f VARCHAR)",
            "CREATE TABLE t2 (a INTEGER, e INTEGER, f TEXT)",
            "t2",
            "iis",
            120,
        ),
    ];
    for (ours_ddl, sqlite_ddl, name, kinds, rows) in ddl {
        ours.sql(ours_ddl).unwrap();
        sqlite.execute_batch(sqlite_ddl).unwrap();
        let values: Vec<String> = (0..rows)
            .map(|_| {
                format!(
                    "({})",
                    kinds
                        .chars()
                        .map(|k| value_sql(&mut rng, k))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        let insert = format!("INSERT INTO {name} VALUES {}", values.join(", "));
        ours.sql(&insert).unwrap();
        sqlite.execute_batch(&insert).unwrap();
    }
    (ours, sqlite)
}

fn run_sqlite(conn: &Connection, sql: &str) -> Vec<Vec<V>> {
    let mut stmt = conn
        .prepare(sql)
        .unwrap_or_else(|e| panic!("sqlite rejected {sql}: {e}"));
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = vec![];
    while let Some(r) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match r.get_ref(i).unwrap() {
                    ValueRef::Null => V::Null,
                    ValueRef::Integer(x) => V::Int(x),
                    ValueRef::Real(x) => V::Real(x),
                    ValueRef::Text(t) => V::Text(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => V::Text("<blob>".into()),
                })
                .collect(),
        );
    }
    out
}

fn compare(seed: u64, queries: usize, non_empty: &mut usize) -> Vec<String> {
    let (mut ours, sqlite) = setup(seed);
    let mut g = Gen {
        rng: Rng::new(seed ^ 0xABCDEF),
    };
    let mut failures = vec![];
    for _ in 0..queries {
        let (sql, ordered) = g.query();
        let mut expected = run_sqlite(&sqlite, &sql);
        let mut actual: Vec<Vec<V>> = match ours.sql(&sql) {
            Ok(r) => r.rows().iter().map(|row| row.iter().map(from_ours).collect()).collect(),
            Err(e) => {
                failures.push(format!("{sql}\n  sqlengine error: {e}"));
                continue;
            }
        };
        *non_empty += !expected.is_empty() as usize;
        if !ordered {
            expected.sort_by_key(|r| sort_key(r));
            actual.sort_by_key(|r| sort_key(r));
        }
        let ok = expected.len() == actual.len()
            && expected
                .iter()
                .zip(&actual)
                .all(|(a, b)| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)));
        if !ok {
            failures.push(format!(
                "{sql}\n  sqlite ({} rows): {:?}\n  ours   ({} rows): {:?}",
                expected.len(),
                &expected[..expected.len().min(5)],
                actual.len(),
                &actual[..actual.len().min(5)]
            ));
        }
    }
    failures
}

#[test]
fn differential_vs_sqlite() {
    let mut failures = vec![];
    let seeds = [1u64, 2, 3, 4];
    let per_seed = 250;
    let mut non_empty = 0;
    for seed in seeds {
        failures.extend(compare(seed, per_seed, &mut non_empty));
    }
    let total = seeds.len() * per_seed;
    // Guard against a degenerate generator that only produces empty results.
    assert!(
        non_empty * 10 > total * 6,
        "only {non_empty} of {total} queries returned rows"
    );
    if !failures.is_empty() {
        panic!(
            "{} of {total} random queries differ from SQLite:\n\n{}",
            failures.len(),
            failures[..failures.len().min(10)].join("\n\n")
        );
    }
    println!("{total} random queries matched SQLite ({non_empty} with non-empty results)");
}
