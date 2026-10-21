//! TPC-H queries at a small scale factor, cross-checked against SQLite.

use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};
use sqlengine::catalog::MemTable;
use sqlengine::types::{ScalarValue, format_date};
use sqlengine::{Session, SessionConfig, tpch};

/// The same queries with date arithmetic pre-computed (SQLite has no DATE type;
/// ISO date strings compare correctly as text).
const SQLITE_QUERIES: [(&str, &str); 4] = [
    (
        "Q1",
        "SELECT l_returnflag, l_linestatus, sum(l_quantity), sum(l_extendedprice), sum(l_extendedprice * (1 - l_discount)),
                sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)), avg(l_quantity), avg(l_extendedprice), avg(l_discount), count(*)
         FROM lineitem WHERE l_shipdate <= '1998-09-02'
         GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus",
    ),
    (
        "Q3",
        "SELECT l_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority
         FROM customer, orders, lineitem
         WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey
           AND o_orderdate < '1995-03-15' AND l_shipdate > '1995-03-15'
         GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10",
    ),
    (
        "Q6",
        "SELECT sum(l_extendedprice * l_discount) FROM lineitem
         WHERE l_shipdate >= '1994-01-01' AND l_shipdate < '1995-01-01' AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity < 24",
    ),
    (
        "Q12",
        "SELECT l_shipmode,
                sum(CASE WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH' THEN 1 ELSE 0 END),
                sum(CASE WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH' THEN 1 ELSE 0 END)
         FROM orders, lineitem
         WHERE o_orderkey = l_orderkey AND l_shipmode IN ('MAIL', 'SHIP') AND l_commitdate < l_receiptdate
           AND l_shipdate < l_commitdate AND l_receiptdate >= '1994-01-01' AND l_receiptdate < '1995-01-01'
         GROUP BY l_shipmode ORDER BY l_shipmode",
    ),
];

fn to_sqlite_value(v: &ScalarValue) -> Value {
    match v {
        ScalarValue::Null(_) => Value::Null,
        ScalarValue::Boolean(b) => Value::Integer(*b as i64),
        ScalarValue::Int64(x) => Value::Integer(*x),
        ScalarValue::Float64(x) => Value::Real(*x),
        ScalarValue::Utf8(s) => Value::Text(s.clone()),
        ScalarValue::Date(d) => Value::Text(format_date(*d)),
        ScalarValue::Interval { .. } => Value::Null,
    }
}

fn cell_eq(ours: &ScalarValue, theirs: ValueRef<'_>) -> bool {
    match (ours, theirs) {
        (ScalarValue::Null(_), ValueRef::Null) => true,
        (ScalarValue::Int64(a), ValueRef::Integer(b)) => *a == b,
        (ScalarValue::Float64(a), ValueRef::Real(b)) => (a - b).abs() <= 1e-6 * a.abs().max(1.0),
        (ScalarValue::Float64(a), ValueRef::Integer(b)) => (a - b as f64).abs() < 1e-6,
        (ScalarValue::Utf8(a), ValueRef::Text(b)) => a.as_bytes() == b,
        (ScalarValue::Date(d), ValueRef::Text(b)) => format_date(*d).as_bytes() == b,
        _ => false,
    }
}

#[test]
fn tpch_queries_match_sqlite() {
    let tables = tpch::generate(0.01, 7);
    let conn = Connection::open_in_memory().unwrap();
    let mut session = Session::with_config(SessionConfig {
        target_partitions: 4,
        optimize: true,
    });
    for t in tables {
        let cols: Vec<String> = t.schema.fields.iter().map(|f| f.name.clone()).collect();
        conn.execute_batch(&format!("CREATE TABLE {} ({})", t.name, cols.join(", ")))
            .unwrap();
        let placeholders = vec!["?"; cols.len()].join(", ");
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(&format!("INSERT INTO {} VALUES ({placeholders})", t.name))
                .unwrap();
            for b in &t.batches {
                for r in 0..b.num_rows {
                    let row: Vec<Value> = b.row(r).iter().map(to_sqlite_value).collect();
                    stmt.execute(rusqlite::params_from_iter(row)).unwrap();
                }
            }
        }
        tx.commit().unwrap();
        session.register_table(t.name, MemTable::new(t.schema, t.batches));
    }

    for ((name, ours_sql), (_, sqlite_sql)) in tpch::queries().into_iter().zip(SQLITE_QUERIES) {
        let ours = session.sql(ours_sql).unwrap_or_else(|e| panic!("{name}: {e}")).rows();
        let mut stmt = conn.prepare(sqlite_sql).unwrap();
        let ncols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        let mut i = 0;
        while let Some(r) = rows.next().unwrap() {
            assert!(
                i < ours.len(),
                "{name}: sqlengine returned too few rows ({})",
                ours.len()
            );
            assert_eq!(ours[i].len(), ncols, "{name}: column count differs");
            for (c, value) in ours[i].iter().enumerate() {
                let theirs = r.get_ref(c).unwrap();
                assert!(
                    cell_eq(value, theirs),
                    "{name} row {i} col {c}: sqlengine={value:?} sqlite={theirs:?}"
                );
            }
            i += 1;
        }
        assert_eq!(i, ours.len(), "{name}: row count differs");
        assert!(i > 0, "{name}: empty result");
    }
}
