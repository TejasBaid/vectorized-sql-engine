//! The user-facing entry point: parse, bind, optimize, plan and execute SQL.

use crate::ast::{InsertSource, Statement};
use crate::catalog::{Catalog, MemTable};
use crate::column::{BATCH_SIZE, Batch, ColumnBuilder, Field, Schema, SchemaRef};
use crate::compute::cast::{cast_column, cast_scalar};
use crate::datasource;
use crate::err;
use crate::error::{Error, ErrorKind, Result};
use crate::format::pretty_table;
use crate::logical::{Binder, LogicalPlan};
use crate::optimizer::Optimizer;
use crate::physical::planner::PhysicalPlanner;
use crate::physical::{ExecutionPlan, collect, display_plan};
use crate::sql::{parse_sql, parse_statement};
use crate::types::{DataType, ScalarValue};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Number of partitions (threads) used for scans and parallel operators.
    pub target_partitions: usize,
    /// Run the logical optimizer (disable to compare plans).
    pub optimize: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            target_partitions: std::thread::available_parallelism().map_or(4, |n| n.get()),
            optimize: true,
        }
    }
}

/// Result of executing one statement.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub schema: SchemaRef,
    pub batches: Vec<Batch>,
    /// For EXPLAIN and DDL: text to show instead of a table.
    pub message: Option<String>,
}

impl QueryResult {
    fn message(msg: impl Into<String>) -> Self {
        QueryResult {
            schema: Arc::new(Schema::empty()),
            batches: vec![],
            message: Some(msg.into()),
        }
    }

    fn text_column(name: &str, lines: Vec<String>, message: Option<String>) -> Self {
        let mut b = ColumnBuilder::new(DataType::Utf8);
        for l in &lines {
            b.push_str(l);
        }
        QueryResult {
            schema: Arc::new(Schema::new(vec![Field::new(name, DataType::Utf8, false)])),
            batches: vec![Batch::from_columns(vec![b.finish()])],
            message,
        }
    }

    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(|b| b.num_rows).sum()
    }

    /// All rows as scalar values (convenient for tests).
    pub fn rows(&self) -> Vec<Vec<ScalarValue>> {
        self.batches
            .iter()
            .flat_map(|b| (0..b.num_rows).map(move |r| b.row(r)))
            .collect()
    }

    pub fn to_pretty(&self, max_rows: usize) -> String {
        match &self.message {
            Some(m) => m.clone(),
            None => pretty_table(&self.schema, &self.batches, max_rows),
        }
    }
}

pub struct Session {
    catalog: Catalog,
    pub config: SessionConfig,
    optimizer: Optimizer,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip qualifiers from a result schema (users see plain column names).
fn output_schema(plan: &LogicalPlan) -> SchemaRef {
    let fields = plan
        .schema()
        .fields
        .iter()
        .map(|f| f.clone().with_qualifier(None))
        .collect();
    Arc::new(Schema::new(fields))
}

impl Session {
    pub fn new() -> Self {
        Self::with_config(SessionConfig::default())
    }

    pub fn with_config(config: SessionConfig) -> Self {
        Session {
            catalog: Catalog::new(),
            config,
            optimizer: Optimizer::new(),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn register_table(&mut self, name: &str, table: MemTable) {
        self.catalog.register(name, table);
    }

    pub fn register_csv(&mut self, name: &str, path: &str) -> Result<()> {
        let t = datasource::csv::read_csv(path, &datasource::csv::CsvOptions::default())?;
        self.catalog.register(name, t);
        Ok(())
    }

    pub fn register_parquet(&mut self, name: &str, path: &str) -> Result<()> {
        let t = datasource::parquet::read_parquet(path)?;
        self.catalog.register(name, t);
        Ok(())
    }

    /// Execute a script of one or more statements; returns the last result.
    pub fn sql(&mut self, sql: &str) -> Result<QueryResult> {
        let stmts = parse_sql(sql)?;
        let mut last = QueryResult::message("OK");
        for s in &stmts {
            last = self.execute(s)?;
        }
        Ok(last)
    }

    /// Parse and bind a single query into an unoptimized logical plan.
    pub fn logical_plan(&self, sql: &str) -> Result<LogicalPlan> {
        match parse_statement(sql)? {
            Statement::Query(q) => Binder::new(&self.catalog).bind_query(&q),
            _ => err!(Plan, "expected a query"),
        }
    }

    pub fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        if self.config.optimize {
            self.optimizer.optimize(plan)
        } else {
            Ok(plan)
        }
    }

    pub fn physical_plan(&self, plan: &LogicalPlan) -> Result<Arc<dyn ExecutionPlan>> {
        PhysicalPlanner::new(self.config.target_partitions).create_physical_plan(plan)
    }

    fn run_query(&self, q: &crate::ast::Query) -> Result<QueryResult> {
        let plan = Binder::new(&self.catalog).bind_query(q)?;
        let schema = output_schema(&plan);
        let plan = self.optimize(plan)?;
        let physical = self.physical_plan(&plan)?;
        let batches = collect(&physical)?;
        Ok(QueryResult {
            schema,
            batches,
            message: None,
        })
    }

    pub fn execute(&mut self, stmt: &Statement) -> Result<QueryResult> {
        match stmt {
            Statement::Query(q) => self.run_query(q),
            Statement::Explain {
                analyze,
                verbose,
                query,
            } => {
                let plan = Binder::new(&self.catalog).bind_query(query)?;
                let mut text = String::new();
                let optimized = if *verbose {
                    text.push_str("== Unoptimized Logical Plan ==\n");
                    text.push_str(&plan.to_string());
                    let (p, trace) = self.optimizer.optimize_traced(plan)?;
                    for (rule, after) in trace {
                        text.push_str(&format!("\n== After {rule} ==\n{after}"));
                    }
                    p
                } else {
                    self.optimize(plan)?
                };
                if !*verbose {
                    text.push_str("== Optimized Logical Plan ==\n");
                    text.push_str(&optimized.to_string());
                }
                let physical = self.physical_plan(&optimized)?;
                if *analyze {
                    let start = Instant::now();
                    let batches = collect(&physical)?;
                    let elapsed = start.elapsed();
                    let rows: usize = batches.iter().map(|b| b.num_rows).sum();
                    text.push_str("\n== Physical Plan (analyzed) ==\n");
                    text.push_str(&display_plan(&physical, true));
                    text.push_str(&format!(
                        "\nExecution: {rows} rows in {:.2} ms using {} partitions (cpu = per-operator self time summed over threads)\n",
                        elapsed.as_secs_f64() * 1e3,
                        self.config.target_partitions
                    ));
                } else {
                    text.push_str("\n== Physical Plan ==\n");
                    text.push_str(&display_plan(&physical, false));
                }
                let lines = text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
                Ok(QueryResult::text_column("plan", lines, Some(text)))
            }
            Statement::CreateTable { name, columns } => {
                if self.catalog.contains(&name.value) {
                    return Err(
                        Error::new(ErrorKind::Bind, format!("table \"{}\" already exists", name.value))
                            .with_span(name.span),
                    );
                }
                let fields = columns.iter().map(|(c, t)| Field::new(&c.value, *t, true)).collect();
                self.catalog.register(&name.value, MemTable::empty(Schema::new(fields)));
                Ok(QueryResult::message(format!("Created table {}", name.value)))
            }
            Statement::CreateTableAs { name, query } => {
                if self.catalog.contains(&name.value) {
                    return Err(
                        Error::new(ErrorKind::Bind, format!("table \"{}\" already exists", name.value))
                            .with_span(name.span),
                    );
                }
                let r = self.run_query(query)?;
                let n = r.num_rows();
                let fields = r
                    .schema
                    .fields
                    .iter()
                    .map(|f| {
                        Field::new(
                            &f.name,
                            if f.data_type == DataType::Null {
                                DataType::Int64
                            } else {
                                f.data_type
                            },
                            true,
                        )
                    })
                    .collect();
                self.catalog
                    .register(&name.value, MemTable::new(Schema::new(fields), rechunk(r.batches)));
                Ok(QueryResult::message(format!("Created table {} ({n} rows)", name.value)))
            }
            Statement::Insert { table, columns, source } => self.insert(table, columns.as_deref(), source),
            Statement::DropTable { name, if_exists } => {
                if *if_exists && !self.catalog.contains(&name.value) {
                    return Ok(QueryResult::message("OK"));
                }
                self.catalog
                    .drop_table(&name.value)
                    .map_err(|e| e.with_span(name.span))?;
                Ok(QueryResult::message(format!("Dropped table {}", name.value)))
            }
            Statement::ShowTables => Ok(QueryResult::text_column("name", self.catalog.table_names(), None)),
            Statement::Describe { name } => {
                let t = self.catalog.get(&name.value).ok_or_else(|| {
                    Error::new(ErrorKind::Bind, format!("table \"{}\" does not exist", name.value)).with_span(name.span)
                })?;
                let mut names = ColumnBuilder::new(DataType::Utf8);
                let mut types = ColumnBuilder::new(DataType::Utf8);
                for f in &t.schema.fields {
                    names.push_str(&f.name);
                    types.push_str(&f.data_type.to_string());
                }
                Ok(QueryResult {
                    schema: Arc::new(Schema::new(vec![
                        Field::new("column_name", DataType::Utf8, false),
                        Field::new("column_type", DataType::Utf8, false),
                    ])),
                    batches: vec![Batch::from_columns(vec![names.finish(), types.finish()])],
                    message: None,
                })
            }
        }
    }

    fn insert(
        &mut self,
        table: &crate::ast::Ident,
        columns: Option<&[crate::ast::Ident]>,
        source: &InsertSource,
    ) -> Result<QueryResult> {
        let t = self.catalog.get(&table.value).ok_or_else(|| {
            Error::new(ErrorKind::Bind, format!("table \"{}\" does not exist", table.value)).with_span(table.span)
        })?;
        let schema = t.schema.clone();
        // Map each provided column to its index in the table.
        let targets: Vec<usize> = match columns {
            None => (0..schema.len()).collect(),
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema.index_of(None, &c.value)?.ok_or_else(|| {
                        Error::new(
                            ErrorKind::Bind,
                            format!("column \"{}\" not found in table \"{}\"", c.value, table.value),
                        )
                        .with_span(c.span)
                    })
                })
                .collect::<Result<_>>()?,
        };
        let batch = match source {
            InsertSource::Values(rows) => {
                let binder = Binder::new(&self.catalog);
                let mut builders: Vec<ColumnBuilder> = schema
                    .fields
                    .iter()
                    .map(|f| ColumnBuilder::with_capacity(f.data_type, rows.len()))
                    .collect();
                for row in rows {
                    if row.len() != targets.len() {
                        return Err(Error::new(
                            ErrorKind::Bind,
                            format!("INSERT has {} values but {} target columns", row.len(), targets.len()),
                        )
                        .with_span(row.first().map(|e| e.span()).unwrap_or(table.span)));
                    }
                    let mut values = vec![ScalarValue::Null(DataType::Null); schema.len()];
                    for (expr, &ti) in row.iter().zip(&targets) {
                        let bound = binder.bind_constant(expr)?;
                        let folded = crate::optimizer::constant_folding::fold_expr(bound)?;
                        let crate::logical::Expr::Literal(v) = folded else {
                            return Err(
                                Error::new(ErrorKind::Bind, "INSERT values must be constant").with_span(expr.span())
                            );
                        };
                        values[ti] =
                            cast_scalar(&v, schema.field(ti).data_type).map_err(|e| e.with_span(expr.span()))?;
                    }
                    for (b, v) in builders.iter_mut().zip(&values) {
                        b.push(v);
                    }
                }
                Batch::from_columns(builders.into_iter().map(|b| b.finish()).collect())
            }
            InsertSource::Query(q) => {
                let r = self.run_query(q)?;
                if r.schema.len() != targets.len() {
                    return err!(
                        Bind,
                        "INSERT query returns {} columns but {} target columns",
                        r.schema.len(),
                        targets.len()
                    );
                }
                let n = r.num_rows();
                let src = if r.batches.is_empty() {
                    Batch::new_empty(&r.schema)
                } else {
                    Batch::concat(&r.batches, r.schema.len())
                };
                let mut cols = vec![None; schema.len()];
                for (k, &ti) in targets.iter().enumerate() {
                    cols[ti] = Some(Arc::new(cast_column(&src.columns[k], schema.field(ti).data_type)?));
                }
                let cols = cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| {
                        c.unwrap_or_else(|| Arc::new(crate::column::Column::new_null(schema.field(i).data_type, n)))
                    })
                    .collect();
                Batch::new(cols, n)
            }
        };
        let n = batch.num_rows;
        self.catalog.update(&table.value, |t| {
            for chunk in rechunk(vec![batch]) {
                t.append(chunk);
            }
        })?;
        Ok(QueryResult::message(format!("Inserted {n} rows")))
    }
}

/// Split oversized batches so stored tables use `BATCH_SIZE` morsels.
fn rechunk(batches: Vec<Batch>) -> Vec<Batch> {
    let mut out = vec![];
    for b in batches {
        if b.num_rows <= BATCH_SIZE {
            if b.num_rows > 0 {
                out.push(b);
            }
            continue;
        }
        let mut off = 0;
        while off < b.num_rows {
            let len = BATCH_SIZE.min(b.num_rows - off);
            out.push(b.slice(off, len));
            off += len;
        }
    }
    out
}
