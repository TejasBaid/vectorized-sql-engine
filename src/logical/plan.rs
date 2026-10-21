//! Logical query plans.

use super::expr::{ColumnRef, Expr, SortExpr};
use crate::catalog::MemTable;
use crate::column::{Field, Schema, SchemaRef};
use crate::error::Result;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
}

#[derive(Debug, Clone)]
pub struct TableScan {
    pub table_name: String,
    /// Qualifier the columns are visible under (alias or table name).
    pub qualifier: String,
    pub source: Arc<MemTable>,
    /// Indices of the source columns to read (`None` = all).
    pub projection: Option<Vec<usize>>,
    /// Predicates pushed into the scan (applied right after reading).
    pub filters: Vec<Expr>,
    pub schema: SchemaRef,
}

impl TableScan {
    pub fn new(table_name: &str, qualifier: &str, source: Arc<MemTable>) -> Self {
        let mut scan = TableScan {
            table_name: table_name.to_string(),
            qualifier: qualifier.to_string(),
            source,
            projection: None,
            filters: vec![],
            schema: Arc::new(Schema::empty()),
        };
        scan.recompute_schema();
        scan
    }

    pub fn recompute_schema(&mut self) {
        let all: Vec<usize> = (0..self.source.schema.len()).collect();
        let idx = self.projection.as_deref().unwrap_or(&all);
        let fields = idx
            .iter()
            .map(|&i| {
                self.source
                    .schema
                    .field(i)
                    .clone()
                    .with_qualifier(Some(self.qualifier.clone()))
            })
            .collect();
        self.schema = Arc::new(Schema::new(fields));
    }
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    TableScan(TableScan),
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    Projection {
        input: Box<LogicalPlan>,
        exprs: Vec<Expr>,
        schema: SchemaRef,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_exprs: Vec<Expr>,
        /// `Expr::Aggregate` nodes (possibly aliased with unique output names).
        aggr_exprs: Vec<Expr>,
        schema: SchemaRef,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        /// Equi-join keys `(left expr, right expr)`. Empty = cross/theta join.
        on: Vec<(Expr, Expr)>,
        /// Non-equi residual condition.
        filter: Option<Expr>,
        schema: SchemaRef,
    },
    Sort {
        input: Box<LogicalPlan>,
        exprs: Vec<SortExpr>,
        /// Only the first `fetch` rows are needed (top-k).
        fetch: Option<usize>,
    },
    Limit {
        input: Box<LogicalPlan>,
        skip: usize,
        fetch: Option<usize>,
    },
    SubqueryAlias {
        input: Box<LogicalPlan>,
        alias: String,
        schema: SchemaRef,
    },
    /// Literal rows. `SELECT 1` uses one zero-column row; an empty relation has no rows.
    Values {
        rows: Vec<Vec<Expr>>,
        schema: SchemaRef,
    },
}

/// Output field for a projected/grouped expression.
pub fn expr_field(e: &Expr, input: &Schema) -> Result<Field> {
    let dt = e.data_type(input)?;
    Ok(match e {
        Expr::Column(c) => {
            let f = input.field(c.index_in(input)?);
            Field::new(&c.name, dt, f.nullable).with_qualifier(c.relation.clone())
        }
        other => Field::new(other.output_name(), dt, true),
    })
}

impl LogicalPlan {
    pub fn schema(&self) -> SchemaRef {
        match self {
            LogicalPlan::TableScan(s) => s.schema.clone(),
            LogicalPlan::Filter { input, .. } | LogicalPlan::Sort { input, .. } | LogicalPlan::Limit { input, .. } => {
                input.schema()
            }
            LogicalPlan::Projection { schema, .. }
            | LogicalPlan::Aggregate { schema, .. }
            | LogicalPlan::Join { schema, .. }
            | LogicalPlan::SubqueryAlias { schema, .. }
            | LogicalPlan::Values { schema, .. } => schema.clone(),
        }
    }

    pub fn projection(input: LogicalPlan, exprs: Vec<Expr>) -> Result<LogicalPlan> {
        let in_schema = input.schema();
        let fields = exprs.iter().map(|e| expr_field(e, &in_schema)).collect::<Result<_>>()?;
        Ok(LogicalPlan::Projection {
            input: Box::new(input),
            exprs,
            schema: Arc::new(Schema::new(fields)),
        })
    }

    pub fn aggregate(input: LogicalPlan, group_exprs: Vec<Expr>, aggr_exprs: Vec<Expr>) -> Result<LogicalPlan> {
        let in_schema = input.schema();
        let fields = group_exprs
            .iter()
            .chain(&aggr_exprs)
            .map(|e| expr_field(e, &in_schema))
            .collect::<Result<_>>()?;
        Ok(LogicalPlan::Aggregate {
            input: Box::new(input),
            group_exprs,
            aggr_exprs,
            schema: Arc::new(Schema::new(fields)),
        })
    }

    pub fn join(
        left: LogicalPlan,
        right: LogicalPlan,
        join_type: JoinType,
        on: Vec<(Expr, Expr)>,
        filter: Option<Expr>,
    ) -> LogicalPlan {
        let mut schema = left.schema().join(&right.schema());
        if join_type == JoinType::Left {
            let n = left.schema().len();
            for f in &mut schema.fields[n..] {
                f.nullable = true;
            }
        }
        LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            on,
            filter,
            schema: Arc::new(schema),
        }
    }

    pub fn subquery_alias(input: LogicalPlan, alias: &str) -> LogicalPlan {
        let fields = input
            .schema()
            .fields
            .iter()
            .map(|f| f.clone().with_qualifier(Some(alias.to_string())))
            .collect();
        LogicalPlan::SubqueryAlias {
            input: Box::new(input),
            alias: alias.to_string(),
            schema: Arc::new(Schema::new(fields)),
        }
    }

    pub fn filter(input: LogicalPlan, predicate: Expr) -> LogicalPlan {
        LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        }
    }

    pub fn empty(schema: SchemaRef) -> LogicalPlan {
        LogicalPlan::Values { rows: vec![], schema }
    }

    pub fn inputs(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::TableScan(_) | LogicalPlan::Values { .. } => vec![],
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Projection { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::SubqueryAlias { input, .. } => vec![input],
            LogicalPlan::Join { left, right, .. } => vec![left, right],
        }
    }

    /// Column references produced by this node's output.
    pub fn output_columns(&self) -> Vec<ColumnRef> {
        self.schema()
            .fields
            .iter()
            .map(|f| ColumnRef {
                relation: f.qualifier.clone(),
                name: f.name.clone(),
            })
            .collect()
    }

    fn fmt_node(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |v: &[Expr]| v.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(", ");
        match self {
            LogicalPlan::TableScan(s) => {
                write!(f, "TableScan: {}", s.table_name)?;
                if s.qualifier != s.table_name {
                    write!(f, " AS {}", s.qualifier)?;
                }
                if s.projection.is_some() {
                    let cols: Vec<&str> = s.schema.fields.iter().map(|f| f.name.as_str()).collect();
                    write!(f, " projection=[{}]", cols.join(", "))?;
                }
                if !s.filters.is_empty() {
                    write!(f, " filters=[{}]", list(&s.filters))?;
                }
                Ok(())
            }
            LogicalPlan::Filter { predicate, .. } => write!(f, "Filter: {predicate}"),
            LogicalPlan::Projection { exprs, .. } => write!(f, "Projection: {}", list(exprs)),
            LogicalPlan::Aggregate {
                group_exprs,
                aggr_exprs,
                ..
            } => {
                write!(
                    f,
                    "Aggregate: groupBy=[{}], aggr=[{}]",
                    list(group_exprs),
                    list(aggr_exprs)
                )
            }
            LogicalPlan::Join {
                join_type, on, filter, ..
            } => {
                if on.is_empty() && filter.is_none() {
                    return write!(f, "CrossJoin");
                }
                write!(f, "{:?} Join:", join_type)?;
                if !on.is_empty() {
                    let qual = |e: &Expr| match e {
                        Expr::Column(c) => c.to_string(),
                        other => other.to_string(),
                    };
                    let keys: Vec<String> = on.iter().map(|(l, r)| format!("{} = {}", qual(l), qual(r))).collect();
                    write!(f, " on=[{}]", keys.join(", "))?;
                }
                if let Some(flt) = filter {
                    write!(f, " filter={flt}")?;
                }
                Ok(())
            }
            LogicalPlan::Sort { exprs, fetch, .. } => {
                let s: Vec<String> = exprs.iter().map(|e| e.to_string()).collect();
                write!(f, "Sort: {}", s.join(", "))?;
                if let Some(n) = fetch {
                    write!(f, " fetch={n}")?;
                }
                Ok(())
            }
            LogicalPlan::Limit { skip, fetch, .. } => {
                write!(f, "Limit: skip={skip}, fetch=")?;
                match fetch {
                    Some(n) => write!(f, "{n}"),
                    None => write!(f, "None"),
                }
            }
            LogicalPlan::SubqueryAlias { alias, .. } => write!(f, "SubqueryAlias: {alias}"),
            LogicalPlan::Values { rows, .. } => {
                if rows.is_empty() {
                    write!(f, "EmptyRelation")
                } else {
                    write!(f, "Values: {} row(s)", rows.len())
                }
            }
        }
    }

    fn fmt_indent(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        write!(f, "{}", "  ".repeat(depth))?;
        self.fmt_node(f)?;
        writeln!(f)?;
        for i in self.inputs() {
            i.fmt_indent(f, depth + 1)?;
        }
        Ok(())
    }
}

impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indent(f, 0)
    }
}
