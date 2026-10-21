//! A deterministic TPC-H-like data generator (customer, orders, lineitem)
//! and a few TPC-H queries.
//!
//! The value distributions follow the TPC-H specification closely enough for
//! the benchmark queries to have realistic selectivities (e.g. Q1 touches
//! ~98% of lineitem, Q6 ~2%), but this is *not* the official `dbgen`, so the
//! numbers are "TPC-H-like", not audited TPC-H results.

use crate::column::{BATCH_SIZE, Batch, ColumnBuilder, Field, Schema};
use crate::types::{DataType, days_from_civil};
use crate::util::Rng;

pub struct GeneratedTable {
    pub name: &'static str,
    pub schema: Schema,
    pub batches: Vec<Batch>,
}

struct TableBuilder {
    types: Vec<DataType>,
    builders: Vec<ColumnBuilder>,
    batches: Vec<Batch>,
}

impl TableBuilder {
    fn new(types: &[DataType]) -> Self {
        TableBuilder {
            types: types.to_vec(),
            builders: types
                .iter()
                .map(|t| ColumnBuilder::with_capacity(*t, BATCH_SIZE))
                .collect(),
            batches: vec![],
        }
    }
    fn b(&mut self, i: usize) -> &mut ColumnBuilder {
        &mut self.builders[i]
    }
    fn end_row(&mut self) {
        if self.builders[0].len() == BATCH_SIZE {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.builders[0].is_empty() {
            return;
        }
        let done = std::mem::replace(
            &mut self.builders,
            self.types
                .iter()
                .map(|t| ColumnBuilder::with_capacity(*t, BATCH_SIZE))
                .collect(),
        );
        self.batches
            .push(Batch::from_columns(done.into_iter().map(|b| b.finish()).collect()));
    }
    fn finish(mut self) -> Vec<Batch> {
        self.flush();
        self.batches
    }
}

const SEGMENTS: [&str; 5] = ["AUTOMOBILE", "BUILDING", "FURNITURE", "MACHINERY", "HOUSEHOLD"];
const PRIORITIES: [&str; 5] = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
const INSTRUCTIONS: [&str; 4] = ["DELIVER IN PERSON", "COLLECT COD", "NONE", "TAKE BACK RETURN"];
const MODES: [&str; 7] = ["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];
const WORDS: [&str; 24] = [
    "furiously",
    "special",
    "requests",
    "packages",
    "carefully",
    "final",
    "deposits",
    "regular",
    "accounts",
    "ironic",
    "pending",
    "quickly",
    "blithely",
    "express",
    "bold",
    "theodolites",
    "foxes",
    "slyly",
    "even",
    "asymptotes",
    "instructions",
    "dependencies",
    "pinto",
    "beans",
];

fn text(rng: &mut Rng, min_words: i64, max_words: i64) -> String {
    let n = rng.range(min_words, max_words);
    (0..n).map(|_| *rng.choose(&WORDS)).collect::<Vec<_>>().join(" ")
}

fn money(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn fields(spec: &[(&str, DataType)]) -> (Schema, Vec<DataType>) {
    let fields = spec.iter().map(|(n, t)| Field::new(*n, *t, false)).collect();
    (Schema::new(fields), spec.iter().map(|(_, t)| *t).collect())
}

/// Generate customer, orders and lineitem at scale factor `sf`
/// (SF 1 = 150k customers, 1.5M orders, ~6M line items).
pub fn generate(sf: f64, seed: u64) -> Vec<GeneratedTable> {
    use DataType::*;
    let mut rng = Rng::new(seed);
    let n_cust = ((150_000.0 * sf) as i64).max(10);
    let n_orders = ((1_500_000.0 * sf) as i64).max(10);
    let n_parts = ((200_000.0 * sf) as i64).max(10);
    let n_supp = ((10_000.0 * sf) as i64).max(10);
    let n_clerks = ((1000.0 * sf) as i64).max(1);
    let start = days_from_civil(1992, 1, 1);
    let end = days_from_civil(1998, 12, 31);
    let current = days_from_civil(1995, 6, 17);

    // customer
    let (c_schema, c_types) = fields(&[
        ("c_custkey", Int64),
        ("c_name", Utf8),
        ("c_address", Utf8),
        ("c_nationkey", Int64),
        ("c_phone", Utf8),
        ("c_acctbal", Float64),
        ("c_mktsegment", Utf8),
        ("c_comment", Utf8),
    ]);
    let mut c = TableBuilder::new(&c_types);
    for k in 1..=n_cust {
        let nation = rng.range(0, 24);
        c.b(0).push_i64(k);
        c.b(1).push_str(&format!("Customer#{k:09}"));
        c.b(2).push_str(&text(&mut rng, 2, 4));
        c.b(3).push_i64(nation);
        c.b(4).push_str(&format!(
            "{}-{:03}-{:03}-{:04}",
            nation + 10,
            rng.range(100, 999),
            rng.range(100, 999),
            rng.range(1000, 9999)
        ));
        c.b(5).push_f64(money(rng.range(-99999, 999999) as f64 / 100.0));
        c.b(6).push_str(rng.choose(&SEGMENTS));
        c.b(7).push_str(&text(&mut rng, 4, 10));
        c.end_row();
    }

    let (o_schema, o_types) = fields(&[
        ("o_orderkey", Int64),
        ("o_custkey", Int64),
        ("o_orderstatus", Utf8),
        ("o_totalprice", Float64),
        ("o_orderdate", Date),
        ("o_orderpriority", Utf8),
        ("o_clerk", Utf8),
        ("o_shippriority", Int64),
        ("o_comment", Utf8),
    ]);
    let (l_schema, l_types) = fields(&[
        ("l_orderkey", Int64),
        ("l_partkey", Int64),
        ("l_suppkey", Int64),
        ("l_linenumber", Int64),
        ("l_quantity", Float64),
        ("l_extendedprice", Float64),
        ("l_discount", Float64),
        ("l_tax", Float64),
        ("l_returnflag", Utf8),
        ("l_linestatus", Utf8),
        ("l_shipdate", Date),
        ("l_commitdate", Date),
        ("l_receiptdate", Date),
        ("l_shipinstruct", Utf8),
        ("l_shipmode", Utf8),
        ("l_comment", Utf8),
    ]);
    let mut o = TableBuilder::new(&o_types);
    let mut l = TableBuilder::new(&l_types);
    for i in 0..n_orders {
        let orderkey = (i / 8) * 32 + (i % 8) + 1; // sparse keys, as in dbgen
        let mut custkey = rng.range(1, n_cust);
        if custkey % 3 == 0 {
            custkey = if custkey < n_cust { custkey + 1 } else { custkey - 1 }; // dbgen leaves 1/3 of customers without orders
        }
        let orderdate = rng.range(start as i64, (end - 151) as i64) as i32;
        let lines = rng.range(1, 7);
        let mut total = 0.0;
        let mut f_count = 0;
        for ln in 1..=lines {
            let partkey = rng.range(1, n_parts);
            let retail = (90000 + ((partkey / 10) % 20001) + 100 * (partkey % 1000)) as f64 / 100.0;
            let qty = rng.range(1, 50) as f64;
            let price = money(qty * retail);
            let disc = rng.range(0, 10) as f64 / 100.0;
            let tax = rng.range(0, 8) as f64 / 100.0;
            let ship = orderdate + rng.range(1, 121) as i32;
            let commit = orderdate + rng.range(30, 90) as i32;
            let receipt = ship + rng.range(1, 30) as i32;
            let returnflag = if receipt <= current {
                if rng.bool(0.5) { "R" } else { "A" }
            } else {
                "N"
            };
            let linestatus = if ship > current { "O" } else { "F" };
            if linestatus == "F" {
                f_count += 1;
            }
            total += price * (1.0 + tax) * (1.0 - disc);
            l.b(0).push_i64(orderkey);
            l.b(1).push_i64(partkey);
            l.b(2).push_i64(rng.range(1, n_supp));
            l.b(3).push_i64(ln);
            l.b(4).push_f64(qty);
            l.b(5).push_f64(price);
            l.b(6).push_f64(disc);
            l.b(7).push_f64(tax);
            l.b(8).push_str(returnflag);
            l.b(9).push_str(linestatus);
            l.b(10).push_date(ship);
            l.b(11).push_date(commit);
            l.b(12).push_date(receipt);
            l.b(13).push_str(rng.choose(&INSTRUCTIONS));
            l.b(14).push_str(rng.choose(&MODES));
            l.b(15).push_str(&text(&mut rng, 2, 6));
            l.end_row();
        }
        let status = if f_count == lines {
            "F"
        } else if f_count == 0 {
            "O"
        } else {
            "P"
        };
        o.b(0).push_i64(orderkey);
        o.b(1).push_i64(custkey);
        o.b(2).push_str(status);
        o.b(3).push_f64(money(total));
        o.b(4).push_date(orderdate);
        o.b(5).push_str(rng.choose(&PRIORITIES));
        o.b(6).push_str(&format!("Clerk#{:09}", rng.range(1, n_clerks)));
        o.b(7).push_i64(0);
        o.b(8).push_str(&text(&mut rng, 3, 8));
        o.end_row();
    }
    vec![
        GeneratedTable {
            name: "customer",
            schema: c_schema,
            batches: c.finish(),
        },
        GeneratedTable {
            name: "orders",
            schema: o_schema,
            batches: o.finish(),
        },
        GeneratedTable {
            name: "lineitem",
            schema: l_schema,
            batches: l.finish(),
        },
    ]
}

/// TPC-H Q1: pricing summary report (scan-heavy aggregation).
pub const Q1: &str = include_str!("../bench/queries/q1.sql");

/// TPC-H Q3: shipping priority (3-way join + aggregation + top-k).
pub const Q3: &str = include_str!("../bench/queries/q3.sql");

/// TPC-H Q6: forecasting revenue change (selective filter + global aggregate).
pub const Q6: &str = include_str!("../bench/queries/q6.sql");

/// TPC-H Q12: shipping modes and order priority (join + CASE aggregation).
pub const Q12: &str = include_str!("../bench/queries/q12.sql");

pub fn queries() -> Vec<(&'static str, &'static str)> {
    vec![("Q1", Q1), ("Q3", Q3), ("Q6", Q6), ("Q12", Q12)]
}
