//! Interactive SQL shell.
//!
//! ```text
//! sqlengine                         # start the REPL
//! sqlengine --tpch 0.1              # preload generated TPC-H tables
//! sqlengine --load t=data/t.csv     # register a CSV / Parquet file as table t
//! sqlengine -c "SELECT 1 + 1"       # run SQL and exit
//! ```

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use sqlengine::catalog::MemTable;
use sqlengine::{Session, tpch};
use std::time::Instant;

/// mimalloc scales much better than the macOS system allocator when many
/// threads allocate and free batch-sized buffers concurrently.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const MAX_ROWS: usize = 40;

const HELP: &str = "\
Enter SQL terminated by ';'. Dot commands:
  .tables                  list tables
  .schema <table>          show a table's columns
  .load <name> <path>      load a .csv/.tsv/.parquet file as a table
  .tpch <scale>            generate TPC-H-like customer/orders/lineitem tables
  .threads <n>             set the number of execution partitions
  .optimizer on|off        toggle the logical optimizer
  .timer on|off            toggle query timing
  .help                    this message
  .quit                    exit
Try:  EXPLAIN ANALYZE SELECT ...;";

struct Shell {
    session: Session,
    timer: bool,
}

impl Shell {
    fn run_sql(&mut self, sql: &str) {
        let start = Instant::now();
        match self.session.sql(sql) {
            Ok(r) => {
                let elapsed = start.elapsed();
                match &r.message {
                    Some(m) => println!("{}", m.trim_end()),
                    None => {
                        print!("{}", r.to_pretty(MAX_ROWS));
                        let n = r.num_rows();
                        print!("{n} row{}", if n == 1 { "" } else { "s" });
                        if self.timer {
                            print!(" ({:.2} ms)", elapsed.as_secs_f64() * 1e3);
                        }
                        println!();
                    }
                }
            }
            Err(e) => eprintln!("{}", e.render(sql)),
        }
    }

    fn load_tpch(&mut self, sf: f64) {
        let start = Instant::now();
        for t in tpch::generate(sf, 42) {
            let rows: usize = t.batches.iter().map(|b| b.num_rows).sum();
            println!("  {:<10} {rows:>10} rows", t.name);
            self.session.register_table(t.name, MemTable::new(t.schema, t.batches));
        }
        println!("generated TPC-H SF {sf} in {:.2}s", start.elapsed().as_secs_f64());
    }

    fn load(&mut self, name: &str, path: &str) {
        let start = Instant::now();
        match sqlengine::datasource::load_file(path) {
            Ok(t) => {
                println!(
                    "loaded {} rows into {name} in {:.2}s",
                    t.num_rows,
                    start.elapsed().as_secs_f64()
                );
                self.session.register_table(name, t);
            }
            Err(e) => eprintln!("{e}"),
        }
    }

    /// Returns false to exit.
    fn dot_command(&mut self, line: &str) -> bool {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            [".quit"] | [".exit"] => return false,
            [".help"] => println!("{HELP}"),
            [".tables"] => self.run_sql("SHOW TABLES"),
            [".schema", t] => self.run_sql(&format!("DESCRIBE {t}")),
            [".load", name, path] => self.load(name, path),
            [".tpch", sf] => match sf.parse() {
                Ok(sf) => self.load_tpch(sf),
                Err(_) => eprintln!("invalid scale factor"),
            },
            [".threads", n] => match n.parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.session.config.target_partitions = n;
                    println!("using {n} partitions");
                }
                _ => eprintln!("invalid thread count"),
            },
            [".optimizer", v] => {
                self.session.config.optimize = *v == "on";
                println!("optimizer {}", if self.session.config.optimize { "on" } else { "off" });
            }
            [".timer", v] => {
                self.timer = *v == "on";
                println!("timer {}", if self.timer { "on" } else { "off" });
            }
            _ => eprintln!("unknown command: {line} (try .help)"),
        }
        true
    }
}

fn main() {
    let mut shell = Shell {
        session: Session::new(),
        timer: true,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--command" if i + 1 < args.len() => {
                command = Some(args[i + 1].clone());
                i += 1;
            }
            "--tpch" if i + 1 < args.len() => {
                shell.load_tpch(args[i + 1].parse().unwrap_or(0.01));
                i += 1;
            }
            "--load" if i + 1 < args.len() => {
                if let Some((name, path)) = args[i + 1].split_once('=') {
                    shell.load(name, path);
                }
                i += 1;
            }
            "--threads" if i + 1 < args.len() => {
                shell.session.config.target_partitions = args[i + 1].parse().unwrap_or(1).max(1);
                i += 1;
            }
            "-h" | "--help" => {
                println!("usage: sqlengine [--tpch SF] [--load name=path]... [--threads N] [-c SQL]\n\n{HELP}");
                return;
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if let Some(sql) = command {
        shell.run_sql(&sql);
        return;
    }

    println!(
        "sqlengine {} — vectorized SQL engine. Type .help for help.",
        env!("CARGO_PKG_VERSION")
    );
    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("failed to start line editor: {e}");
            return;
        }
    };
    let history = std::env::var("HOME")
        .map(|h| format!("{h}/.sqlengine_history"))
        .unwrap_or_default();
    let _ = rl.load_history(&history);
    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "sql> " } else { "...> " };
        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.starts_with('.') {
                    let _ = rl.add_history_entry(trimmed);
                    if !shell.dot_command(trimmed) {
                        break;
                    }
                    continue;
                }
                if trimmed.is_empty() && buffer.is_empty() {
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if trimmed.ends_with(';') {
                    let sql = std::mem::take(&mut buffer);
                    let _ = rl.add_history_entry(sql.trim());
                    shell.run_sql(&sql);
                }
            }
            Err(ReadlineError::Interrupted) => buffer.clear(),
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        }
    }
    let _ = rl.save_history(&history);
}
