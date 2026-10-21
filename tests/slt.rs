//! sqllogictest-style end-to-end tests.
//!
//! Every `tests/slt/*.slt` file is run in a fresh session. Record types:
//!
//! ```text
//! statement ok | statement error <substring>
//! <sql>
//!
//! query <type letters> [rowsort]
//! <sql>
//! ----
//! <expected rows, values separated by single spaces>
//! ```
//!
//! Type letters: `I` integer/boolean, `R` real (printed with 3 decimals),
//! `T` text. NULL prints as `NULL`, the empty string as `(empty)`.
//! Run with `SLT_COMPLETE=1` to (re)generate expected output in place.

use sqlengine::types::ScalarValue;
use sqlengine::{QueryResult, Session, SessionConfig};
use std::fmt::Write as _;
use std::path::Path;

enum Record {
    Statement {
        sql: String,
        error: Option<String>,
        line: usize,
    },
    Query {
        sql: String,
        types: String,
        rowsort: bool,
        expected: Vec<String>,
        line: usize,
    },
}

fn parse_file(text: &str) -> Vec<(String, Record)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let mut out = vec![];
    let mut comments = String::new();
    while i < lines.len() {
        let l = lines[i].trim();
        if l.starts_with('#') {
            comments.push_str(l);
            comments.push('\n');
            i += 1;
            continue;
        }
        if l.is_empty() {
            i += 1;
            continue;
        }
        let comment = std::mem::take(&mut comments);
        let header_line = i + 1;
        let header: Vec<&str> = l.split_whitespace().collect();
        i += 1;
        let mut sql = vec![];
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i].trim() != "----" {
            sql.push(lines[i]);
            i += 1;
        }
        let sql = sql.join("\n");
        match header[0] {
            "statement" => {
                let error = (header.get(1) == Some(&"error")).then(|| header[2..].join(" "));
                out.push((
                    comment,
                    Record::Statement {
                        sql,
                        error,
                        line: header_line,
                    },
                ));
            }
            "query" => {
                let mut expected = vec![];
                if i < lines.len() && lines[i].trim() == "----" {
                    i += 1;
                    while i < lines.len() && !lines[i].trim().is_empty() {
                        expected.push(lines[i].trim_end().to_string());
                        i += 1;
                    }
                }
                out.push((
                    comment,
                    Record::Query {
                        sql,
                        types: header.get(1).unwrap_or(&"").to_string(),
                        rowsort: header.get(2) == Some(&"rowsort"),
                        expected,
                        line: header_line,
                    },
                ));
            }
            other => panic!("unknown record type {other} at line {header_line}"),
        }
    }
    out
}

fn format_value(v: &ScalarValue, ty: char) -> String {
    match v {
        ScalarValue::Null(_) => "NULL".into(),
        ScalarValue::Utf8(s) if s.is_empty() => "(empty)".into(),
        ScalarValue::Float64(f) if ty == 'R' || ty == 'I' => {
            let s = format!("{f:.3}");
            if s == "-0.000" { "0.000".into() } else { s }
        }
        ScalarValue::Int64(x) if ty == 'R' => format!("{:.3}", *x as f64),
        other => other.to_string(),
    }
}

fn format_rows(r: &QueryResult, types: &str, rowsort: bool) -> Vec<String> {
    let tys: Vec<char> = types.chars().collect();
    let mut rows: Vec<String> = r
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, v)| format_value(v, *tys.get(i).unwrap_or(&'T')))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    if rowsort {
        rows.sort();
    }
    rows
}

fn run_file(path: &Path, complete: bool) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let records = parse_file(&text);
    // Use several partitions even for tiny tables to exercise parallel paths.
    let mut session = Session::with_config(SessionConfig {
        target_partitions: 4,
        optimize: true,
    });
    let mut failures = vec![];
    let mut completed = String::new();
    let name = path.file_name().unwrap().to_string_lossy();
    for (comment, rec) in &records {
        completed.push_str(comment);
        match rec {
            Record::Statement { sql, error, line } => {
                let result = session.sql(sql);
                match (error, &result) {
                    (None, Err(e)) => failures.push(format!("{name}:{line}: statement failed: {e}\n{sql}")),
                    (Some(_), Ok(_)) => {
                        failures.push(format!("{name}:{line}: expected error, statement succeeded\n{sql}"))
                    }
                    (Some(want), Err(e)) if !e.to_string().contains(want.as_str()) => failures.push(format!(
                        "{name}:{line}: wrong error\n  expected substring: {want}\n  actual: {e}"
                    )),
                    _ => {}
                }
                let header = match (error, &result) {
                    (Some(want), _) => format!("statement error {want}"),
                    (None, Err(e)) if complete => format!("statement error {}", e.message),
                    _ => "statement ok".to_string(),
                };
                let _ = write!(completed, "{header}\n{sql}\n\n");
            }
            Record::Query {
                sql,
                types,
                rowsort,
                expected,
                line,
            } => {
                let sort = if *rowsort { " rowsort" } else { "" };
                match session.sql(sql) {
                    Ok(r) => {
                        if r.schema.len() != types.len() {
                            failures.push(format!(
                                "{name}:{line}: expected {} columns, got {}\n{sql}",
                                types.len(),
                                r.schema.len()
                            ));
                        }
                        let actual = format_rows(&r, types, *rowsort);
                        if !complete && actual != *expected {
                            failures.push(format!(
                                "{name}:{line}: result mismatch\n{sql}\n--- expected\n{}\n--- actual\n{}",
                                expected.join("\n"),
                                actual.join("\n")
                            ));
                        }
                        let _ = write!(completed, "query {types}{sort}\n{sql}\n----\n{}\n\n", actual.join("\n"));
                    }
                    Err(e) => {
                        failures.push(format!("{name}:{line}: query failed: {e}\n{sql}"));
                        let _ = write!(
                            completed,
                            "query {types}{sort}\n{sql}\n----\n{}\n\n",
                            expected.join("\n")
                        );
                    }
                }
            }
        }
    }
    if complete {
        std::fs::write(path, completed.trim_end().to_string() + "\n").unwrap();
    }
    failures
}

#[test]
fn sqllogictests() {
    let complete = std::env::var("SLT_COMPLETE").is_ok();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "slt"))
        .collect();
    files.sort();
    assert!(!files.is_empty());
    let mut failures = vec![];
    let mut count = 0;
    for f in &files {
        count += parse_file(&std::fs::read_to_string(f).unwrap()).len();
        failures.extend(run_file(f, complete));
    }
    if !failures.is_empty() {
        panic!(
            "{} of {count} slt records failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
    println!("{count} slt records in {} files passed", files.len());
}
