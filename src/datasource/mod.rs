//! File data sources. Everything is decoded into the engine's own columnar
//! [`Batch`](crate::column::Batch) format at load time.

pub mod csv;
pub mod parquet;

use crate::catalog::MemTable;
use crate::err;
use crate::error::Result;

/// Load a file by extension (`.csv`, `.tsv`, `.tbl`, `.parquet`).
pub fn load_file(path: &str) -> Result<MemTable> {
    let lower = path.to_lowercase();
    if lower.ends_with(".parquet") {
        parquet::read_parquet(path)
    } else if lower.ends_with(".csv") || lower.ends_with(".tsv") || lower.ends_with(".tbl") || lower.ends_with(".txt") {
        csv::read_csv(path, &csv::CsvOptions::default())
    } else {
        err!(
            Io,
            "cannot infer file format of '{path}' (expected .csv, .tsv or .parquet)"
        )
    }
}
