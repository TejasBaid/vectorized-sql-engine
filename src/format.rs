//! Pretty-printing of query results as box-drawn tables.

use crate::column::{Batch, Schema};
use crate::types::DataType;

const MAX_CELL: usize = 48;

fn truncate(s: String) -> String {
    if s.chars().count() <= MAX_CELL {
        s
    } else {
        let mut t: String = s.chars().take(MAX_CELL - 1).collect();
        t.push('…');
        t
    }
}

/// Render up to `max_rows` rows (head and tail are shown if truncated).
pub fn pretty_table(schema: &Schema, batches: &[Batch], max_rows: usize) -> String {
    let total: usize = batches.iter().map(|b| b.num_rows).sum();
    let mut rows: Vec<Option<Vec<String>>> = vec![];
    let all_rows = || batches.iter().flat_map(|b| (0..b.num_rows).map(move |r| (b, r)));
    let render = |(b, r): (&Batch, usize)| -> Option<Vec<String>> {
        Some(b.columns.iter().map(|c| truncate(c.value(r).to_string())).collect())
    };
    if total <= max_rows {
        rows.extend(all_rows().map(render));
    } else {
        let head = max_rows / 2;
        let tail = max_rows - head;
        rows.extend(all_rows().take(head).map(render));
        rows.push(None);
        rows.extend(all_rows().skip(total - tail).map(render));
    }
    let names: Vec<String> = schema.fields.iter().map(|f| f.name.clone()).collect();
    let types: Vec<String> = schema.fields.iter().map(|f| f.data_type.to_string()).collect();
    let ncols = names.len();
    let mut widths: Vec<usize> = (0..ncols)
        .map(|i| names[i].chars().count().max(types[i].len()))
        .collect();
    for r in rows.iter().flatten() {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let numeric: Vec<bool> = schema
        .fields
        .iter()
        .map(|f| matches!(f.data_type, DataType::Int64 | DataType::Float64))
        .collect();
    let line = |l: &str, m: &str, r: &str| -> String {
        let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{l}{}{r}\n", segs.join(m))
    };
    let cell_row = |cells: &[String], align_right: bool| -> String {
        let segs: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let pad = widths[i] - c.chars().count();
                if align_right && numeric[i] {
                    format!(" {}{} ", " ".repeat(pad), c)
                } else {
                    format!(" {}{} ", c, " ".repeat(pad))
                }
            })
            .collect();
        format!("│{}│\n", segs.join("│"))
    };
    if ncols == 0 {
        return format!("({total} rows, no columns)\n");
    }
    let mut out = String::new();
    out.push_str(&line("┌", "┬", "┐"));
    out.push_str(&cell_row(&names, false));
    out.push_str(&cell_row(&types, false));
    out.push_str(&line("├", "┼", "┤"));
    for r in &rows {
        match r {
            Some(cells) => out.push_str(&cell_row(cells, true)),
            None => {
                let dots: Vec<String> = (0..ncols).map(|_| "·".to_string()).collect();
                out.push_str(&cell_row(&dots, false));
            }
        }
    }
    out.push_str(&line("└", "┴", "┘"));
    out
}
