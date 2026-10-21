//! Generate TPC-H-like tables as CSV and/or Parquet.
//!
//! ```text
//! cargo run --release --bin tpchgen -- --sf 1 --out data/sf1 --format both
//! ```

use sqlengine::datasource::{csv, parquet};
use sqlengine::tpch;
use std::time::Instant;

/// mimalloc scales much better than the macOS system allocator when many
/// threads allocate and free batch-sized buffers concurrently.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sf = 0.1;
    let mut out = "data/sf0.1".to_string();
    let mut format = "both".to_string();
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--sf" => sf = val.parse().expect("invalid --sf"),
            "--out" => out = val,
            "--format" => format = val,
            _ => {
                eprintln!("usage: tpchgen [--sf N] [--out DIR] [--format csv|parquet|both]");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    std::fs::create_dir_all(&out).expect("create output directory");
    let start = Instant::now();
    let tables = tpch::generate(sf, 42);
    println!("generated SF {sf} in {:.2}s", start.elapsed().as_secs_f64());
    for t in &tables {
        let rows: usize = t.batches.iter().map(|b| b.num_rows).sum();
        if format == "csv" || format == "both" {
            let path = format!("{out}/{}.csv", t.name);
            let s = Instant::now();
            csv::write_csv(&t.schema, &t.batches, &path).expect("write csv");
            println!("  {path:<32} {rows:>10} rows  {:.2}s", s.elapsed().as_secs_f64());
        }
        if format == "parquet" || format == "both" {
            let path = format!("{out}/{}.parquet", t.name);
            let s = Instant::now();
            parquet::write_parquet(&t.schema, &t.batches, &path, 1 << 20).expect("write parquet");
            println!("  {path:<32} {rows:>10} rows  {:.2}s", s.elapsed().as_secs_f64());
        }
    }
}
