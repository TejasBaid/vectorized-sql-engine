//! TPC-H-like benchmark runner.
//!
//! ```text
//! cargo run --release --bin bench -- --data data/sf1 --format parquet --runs 5
//! cargo run --release --bin bench -- --data data/sf1 --threads 1,2,4,8 --queries Q1,Q6
//! ```
//!
//! Tables are loaded into memory first (load time reported separately); each
//! query is then run `--runs` times after one warm-up run, and the minimum and
//! median wall-clock times are reported.

use sqlengine::{Session, SessionConfig, tpch};
use std::time::Instant;

/// mimalloc scales much better than the macOS system allocator when many
/// threads allocate and free batch-sized buffers concurrently.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data = "data/sf1".to_string();
    let mut format = "parquet".to_string();
    let mut runs = 5usize;
    let mut threads: Vec<usize> = vec![std::thread::available_parallelism().map_or(4, |n| n.get())];
    let mut only: Option<Vec<String>> = None;
    let mut print = false;
    let mut csv_out: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--data" => data = val,
            "--format" => format = val,
            "--runs" => runs = val.parse().expect("invalid --runs"),
            "--threads" => threads = val.split(',').map(|t| t.parse().expect("invalid --threads")).collect(),
            "--queries" => only = Some(val.split(',').map(|s| s.to_uppercase()).collect()),
            "--csv-out" => csv_out = Some(val),
            "--print" => {
                print = true;
                i += 1;
                continue;
            }
            _ => {
                eprintln!(
                    "usage: bench [--data DIR] [--format csv|parquet] [--runs N] [--threads 1,2,4] [--queries Q1,Q3] [--print] [--csv-out DIR]"
                );
                std::process::exit(2);
            }
        }
        i += 2;
    }

    let mut session = Session::with_config(SessionConfig::default());
    for name in ["customer", "orders", "lineitem"] {
        let path = format!("{data}/{name}.{format}");
        let start = Instant::now();
        let result = if format == "csv" {
            session.register_csv(name, &path)
        } else {
            session.register_parquet(name, &path)
        };
        if let Err(e) = result {
            eprintln!("failed to load {path}: {e}");
            std::process::exit(1);
        }
        let rows = session.catalog().get(name).map_or(0, |t| t.num_rows);
        println!(
            "loaded {name:<9} {rows:>10} rows from {format} in {:>7.1} ms",
            start.elapsed().as_secs_f64() * 1e3
        );
    }
    println!();
    println!("| query | threads | rows | min (ms) | median (ms) |");
    println!("|-------|--------:|-----:|---------:|------------:|");
    for &t in &threads {
        session.config.target_partitions = t;
        for (name, sql) in tpch::queries() {
            if only.as_ref().is_some_and(|o| !o.iter().any(|q| q == name)) {
                continue;
            }
            let warm = session.sql(sql).expect("query failed");
            if print {
                eprintln!("-- {name}\n{}", warm.to_pretty(20));
            }
            if let Some(dir) = &csv_out {
                std::fs::create_dir_all(dir).expect("create csv dir");
                let path = format!("{dir}/{}.csv", name.to_lowercase());
                sqlengine::datasource::csv::write_csv(&warm.schema, &warm.batches, &path).expect("write csv");
            }
            let mut times = vec![];
            for _ in 0..runs {
                let start = Instant::now();
                let r = session.sql(sql).expect("query failed");
                times.push(start.elapsed().as_secs_f64() * 1e3);
                assert_eq!(r.num_rows(), warm.num_rows());
            }
            let min = times.iter().cloned().fold(f64::INFINITY, f64::min);
            println!(
                "| {name} | {t} | {} | {min:.1} | {:.1} |",
                warm.num_rows(),
                median(&mut times)
            );
        }
    }
}
