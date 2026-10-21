//! Optimizer rule tests. Each test checks the plan shape produced by a rule
//! (or the full rule pipeline) and, where relevant, that results are unchanged.

use sqlengine::catalog::MemTable;
use sqlengine::column::{Batch, ColumnBuilder, Field, Schema};
use sqlengine::optimizer::{
    ConstantFolding, JoinReorder, LimitPushdown, Optimizer, OptimizerRule, PredicatePushdown, ProjectionPushdown,
};
use sqlengine::types::DataType;
use sqlengine::{Session, SessionConfig};

/// A table with integer columns `cols`, `rows` rows, column i = (row * (i+1)) % 97.
fn int_table(cols: &[&str], rows: usize) -> MemTable {
    let schema = Schema::new(cols.iter().map(|c| Field::new(*c, DataType::Int64, true)).collect());
    let columns = (0..cols.len())
        .map(|i| {
            let mut b = ColumnBuilder::new(DataType::Int64);
            for r in 0..rows {
                b.push_i64(((r * (i + 1)) % 97) as i64);
            }
            b.finish()
        })
        .collect();
    MemTable::new(schema, vec![Batch::from_columns(columns)])
}

fn session() -> Session {
    let mut s = Session::with_config(SessionConfig {
        target_partitions: 2,
        optimize: true,
    });
    s.register_table("t", int_table(&["a", "b", "c"], 100));
    s.register_table("u", int_table(&["c", "d"], 50));
    s.register_table("small", int_table(&["sx", "sy"], 10));
    s.register_table("medium", int_table(&["mx", "my"], 100));
    s.register_table("big", int_table(&["bx", "by"], 2_000));
    s
}

fn plan_with(s: &Session, sql: &str, rules: Vec<Box<dyn OptimizerRule>>) -> String {
    let plan = s.logical_plan(sql).unwrap();
    Optimizer::with_rules(rules).optimize(plan).unwrap().to_string()
}

fn full_plan(s: &Session, sql: &str) -> String {
    let plan = s.logical_plan(sql).unwrap();
    s.optimize(plan).unwrap().to_string()
}

#[test]
fn constant_folding_simplifies_expressions() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT a + (1 + 2) FROM t WHERE 1 = 1 AND a > 2 * 5",
        vec![Box::new(ConstantFolding)],
    );
    assert!(p.contains("Filter: a > 10"), "{p}");
    assert!(p.contains("a + 3"), "{p}");
    assert!(!p.contains("1 = 1"), "{p}");
}

#[test]
fn constant_folding_dates_and_false_filters() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT DATE '1998-12-01' - INTERVAL '90' DAY AS d",
        vec![Box::new(ConstantFolding)],
    );
    assert!(p.contains("DATE '1998-09-02' AS d"), "{p}");
    let p = plan_with(
        &s,
        "SELECT a FROM t WHERE a > 1 AND 1 > 2",
        vec![Box::new(ConstantFolding)],
    );
    assert!(p.contains("EmptyRelation") && !p.contains("TableScan"), "{p}");
    let p = plan_with(
        &s,
        "SELECT a FROM t WHERE NOT NOT (a = 1 OR false)",
        vec![Box::new(ConstantFolding)],
    );
    assert!(p.contains("Filter: a = 1"), "{p}");
}

#[test]
fn predicate_pushdown_through_projection_and_alias() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT * FROM (SELECT a, b * 2 AS b2 FROM t) s WHERE s.b2 > 4 AND s.a = 1",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(p.contains("TableScan: t filters=[b * 2 > 4, a = 1]"), "{p}");
    assert!(!p.contains("Filter:"), "{p}");
}

#[test]
fn predicate_pushdown_below_aggregate_only_for_group_keys() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT * FROM (SELECT a, sum(b) AS total FROM t GROUP BY a) x WHERE x.a > 1 AND x.total > 10",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(p.contains("TableScan: t filters=[a > 1]"), "{p}");
    let agg_pos = p.find("Aggregate").unwrap();
    let filter_pos = p.find("Filter: sum(b) > 10").expect(&p);
    assert!(
        filter_pos < agg_pos,
        "HAVING-like filter must stay above the aggregate:\n{p}"
    );
}

#[test]
fn cross_join_with_equality_becomes_equi_join() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT t.a, u.d FROM t, u WHERE t.c = u.c AND u.d < 5",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(p.contains("Inner Join: on=[t.c = u.c]"), "{p}");
    assert!(p.contains("TableScan: u filters=[d < 5]"), "{p}");
    assert!(!p.contains("CrossJoin"), "{p}");
}

#[test]
fn left_join_rewrites() {
    let s = session();
    // Null-rejecting WHERE predicate on the right side: LEFT -> INNER, then pushed down.
    let p = plan_with(
        &s,
        "SELECT * FROM t LEFT JOIN u ON t.c = u.c WHERE u.d > 3",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(
        p.contains("Inner Join") && p.contains("TableScan: u filters=[d > 3]"),
        "{p}"
    );
    // IS NULL is not null-rejecting: the join stays LEFT and the filter stays above.
    let p = plan_with(
        &s,
        "SELECT * FROM t LEFT JOIN u ON t.c = u.c WHERE u.d IS NULL",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(p.contains("Left Join") && p.contains("Filter: d IS NULL"), "{p}");
    // ON conditions: right-only conjuncts filter the right input; left-only ones must stay in the join.
    let p = plan_with(
        &s,
        "SELECT * FROM t LEFT JOIN u ON t.c = u.c AND u.d = 1 AND t.a = 2",
        vec![Box::new(PredicatePushdown)],
    );
    assert!(p.contains("TableScan: u filters=[d = 1]"), "{p}");
    assert!(p.contains("filter=a = 2") && !p.contains("TableScan: t filters"), "{p}");
}

#[test]
fn projection_pushdown_prunes_columns() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT a FROM t WHERE b > 1",
        vec![Box::new(PredicatePushdown), Box::new(ProjectionPushdown)],
    );
    assert!(p.contains("TableScan: t projection=[a, b] filters=[b > 1]"), "{p}");
    let p = plan_with(&s, "SELECT count(*) FROM t", vec![Box::new(ProjectionPushdown)]);
    assert!(p.contains("TableScan: t projection=[]"), "{p}");
    let p = plan_with(
        &s,
        "SELECT x.s FROM (SELECT a, sum(b) AS s, max(c) AS m FROM t GROUP BY a) x",
        vec![Box::new(ProjectionPushdown)],
    );
    assert!(
        p.contains("aggr=[sum(b)]") && p.contains("projection=[a, b]"),
        "unused aggregate and column must be pruned:\n{p}"
    );
}

/// Return the table names in the order their scans appear in the plan text.
fn scan_order(plan: &str) -> Vec<String> {
    plan.lines()
        .filter_map(|l| l.trim().strip_prefix("TableScan: "))
        .map(|l| l.split_whitespace().next().unwrap().to_string())
        .collect()
}

#[test]
fn join_reorder_starts_from_smallest_connected_relation() {
    let s = session();
    let sql = "SELECT count(*) FROM big, medium, small WHERE big.bx = medium.mx AND medium.my = small.sy";
    let p = plan_with(&s, sql, vec![Box::new(PredicatePushdown), Box::new(JoinReorder)]);
    assert_eq!(scan_order(&p), vec!["small", "medium", "big"], "{p}");
}

#[test]
fn join_reorder_avoids_cross_products() {
    let s = session();
    // small and big are not directly connected; medium links them.
    let sql = "SELECT count(*) FROM small, big, medium WHERE small.sx = medium.mx AND big.by = medium.my";
    let p = plan_with(&s, sql, vec![Box::new(PredicatePushdown), Box::new(JoinReorder)]);
    assert_eq!(scan_order(&p), vec!["small", "medium", "big"], "{p}");
    assert!(!p.contains("CrossJoin"), "{p}");
}

#[test]
fn limit_pushdown_creates_top_k() {
    let s = session();
    let p = plan_with(
        &s,
        "SELECT a FROM t ORDER BY b DESC LIMIT 5 OFFSET 2",
        vec![Box::new(LimitPushdown)],
    );
    assert!(p.contains("fetch=7"), "{p}");
}

#[test]
fn optimizer_preserves_results() {
    let queries = [
        "SELECT a, b FROM t WHERE a > 10 AND 1 = 1 ORDER BY a, b",
        "SELECT t.a, u.d FROM t, u WHERE t.c = u.c AND u.d < 50 ORDER BY 1, 2",
        "SELECT * FROM t LEFT JOIN u ON t.c = u.c WHERE u.d > 3 ORDER BY t.a, t.b, u.d",
        "SELECT * FROM t LEFT JOIN u ON t.c = u.c AND u.d = 1 AND t.a = 2 ORDER BY t.a, t.b, t.c",
        "SELECT count(*), sum(bx) FROM big, medium, small WHERE big.bx = medium.mx AND medium.my = small.sy",
        "SELECT x.a, x.total FROM (SELECT a, sum(b) AS total FROM t GROUP BY a) x WHERE x.a > 1 AND x.total > 10 ORDER BY 1",
        "SELECT a FROM t ORDER BY b DESC, a LIMIT 5 OFFSET 2",
    ];
    let mut on = session();
    let mut off = session();
    off.config.optimize = false;
    for q in queries {
        let a = on.sql(q).unwrap().rows();
        let b = off.sql(q).unwrap().rows();
        assert_eq!(a, b, "optimizer changed the result of {q}");
        assert!(!a.is_empty(), "query returned no rows: {q}");
    }
}

#[test]
fn tpch_q3_plan_is_sensible() {
    let mut s = session();
    for t in sqlengine::tpch::generate(0.01, 1) {
        s.register_table(t.name, MemTable::new(t.schema, t.batches));
    }
    let p = full_plan(&s, sqlengine::tpch::Q3);
    // customer (most selective) is joined with orders first, lineitem last.
    assert_eq!(scan_order(&p), vec!["customer", "orders", "lineitem"], "{p}");
    assert!(p.contains("filters=[c_mktsegment = 'BUILDING']"), "{p}");
    assert!(p.contains("Sort: revenue DESC, o_orderdate ASC fetch=10"), "{p}");
}
