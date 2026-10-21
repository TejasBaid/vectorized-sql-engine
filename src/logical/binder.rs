//! The binder turns an AST into a typed [`LogicalPlan`].
//!
//! Responsibilities:
//! * resolve table names (catalog, CTEs, files) and column references,
//! * type-check expressions and insert implicit casts (e.g. INT -> DOUBLE,
//!   `'1995-03-15'` -> DATE when compared with a DATE column),
//! * plan aggregation: collect aggregate calls, validate that non-aggregated
//!   columns appear in GROUP BY, and rewrite SELECT/HAVING/ORDER BY to refer to
//!   the aggregate's output columns,
//! * resolve ORDER BY against output aliases / ordinals / hidden columns.

use super::expr::*;
use super::plan::{JoinType, LogicalPlan, TableScan};
use crate::ast::{self, BinaryOperator, JoinConstraint, JoinKind, Literal, SelectItem, TableRef, UnaryOperator};
use crate::catalog::Catalog;
use crate::column::Schema;
use crate::compute::cast::cast_scalar;
use crate::compute::functions::ScalarFunc;
use crate::error::{Error, ErrorKind, Result, Span};
use crate::types::{DataType, ScalarValue, parse_date};
use std::collections::HashMap;
use std::sync::Arc;

fn bind_err(msg: impl Into<String>, span: Span) -> Error {
    Error::new(ErrorKind::Bind, msg).with_span(span)
}

fn type_err(msg: impl Into<String>, span: Span) -> Error {
    Error::new(ErrorKind::Type, msg).with_span(span)
}

#[derive(Clone, Copy)]
enum AggMode {
    Allow,
    Disallow(&'static str),
}

pub struct Binder<'a> {
    catalog: &'a Catalog,
    /// Stack of CTE scopes; inner scopes shadow outer ones.
    ctes: Vec<HashMap<String, LogicalPlan>>,
}

/// Aggregation state while binding a SELECT with GROUP BY / aggregates.
struct AggContext {
    groups: Vec<Expr>,
    group_names: Vec<ColumnRef>,
    aggs: Vec<(Expr, String)>,
}

impl AggContext {
    fn agg_ref(&mut self, agg: &Expr) -> ColumnRef {
        if let Some((_, n)) = self.aggs.iter().find(|(e, _)| e == agg) {
            return ColumnRef::new(None, n);
        }
        let base = agg.to_string();
        let mut name = base.clone();
        let mut k = 1;
        while self.aggs.iter().any(|(_, n)| *n == name)
            || self.group_names.iter().any(|g| g.relation.is_none() && g.name == name)
        {
            k += 1;
            name = format!("{base}#{k}");
        }
        self.aggs.push((agg.clone(), name.clone()));
        ColumnRef::new(None, &name)
    }

    /// Rewrite an expression bound against the aggregate's *input* so that it
    /// only references the aggregate's *output* (group keys and aggregates).
    fn rewrite(&mut self, e: &Expr, span: Span) -> Result<Expr> {
        if let Some(i) = self.groups.iter().position(|g| g == e) {
            return Ok(Expr::Column(self.group_names[i].clone()));
        }
        match e {
            Expr::Aggregate(_) => Ok(Expr::Column(self.agg_ref(e))),
            Expr::Column(c) => Err(bind_err(
                format!("column \"{c}\" must appear in the GROUP BY clause or be used in an aggregate function"),
                span,
            )),
            Expr::Literal(_) => Ok(e.clone()),
            other => {
                // Rebuild children through the rewrite (top-down so that whole
                // sub-expressions matching a GROUP BY expression are replaced).
                let mut err = None;
                let rebuilt = rebuild_children(other, &mut |child| match self.rewrite(child, span) {
                    Ok(x) => x,
                    Err(e) => {
                        err.get_or_insert(e);
                        child.clone()
                    }
                });
                match err {
                    Some(e) => Err(e),
                    None => Ok(rebuilt),
                }
            }
        }
    }
}

/// Clone `e`, mapping each direct child through `f`.
fn rebuild_children(e: &Expr, f: &mut dyn FnMut(&Expr) -> Expr) -> Expr {
    let b = |x: &Expr, f: &mut dyn FnMut(&Expr) -> Expr| Box::new(f(x));
    match e {
        Expr::Column(_) | Expr::Literal(_) => e.clone(),
        Expr::Alias(x, n) => Expr::Alias(b(x, f), n.clone()),
        Expr::Not(x) => Expr::Not(b(x, f)),
        Expr::Negative(x) => Expr::Negative(b(x, f)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: b(expr, f),
            negated: *negated,
        },
        Expr::Cast { expr, data_type } => Expr::Cast {
            expr: b(expr, f),
            data_type: *data_type,
        },
        Expr::Binary { left, op, right } => Expr::Binary {
            left: b(left, f),
            op: *op,
            right: b(right, f),
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: b(expr, f),
            pattern: b(pattern, f),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::InList { expr, list, negated } => Expr::InList {
            expr: b(expr, f),
            list: list.iter().map(&mut *f).collect(),
            negated: *negated,
        },
        Expr::Case { when_then, else_expr } => Expr::Case {
            when_then: when_then.iter().map(|(w, t)| (f(w), f(t))).collect(),
            else_expr: else_expr.as_ref().map(|x| b(x, f)),
        },
        Expr::Function { func, args } => Expr::Function {
            func: *func,
            args: args.iter().map(f).collect(),
        },
        Expr::Aggregate(_) => e.clone(),
    }
}

/// Levenshtein distance for "did you mean" hints.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + (ca != *cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn suggestion<'s>(name: &str, candidates: impl Iterator<Item = &'s str>) -> String {
    candidates
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, c)| *d <= 2.max(c.len() / 3))
        .min()
        .map(|(_, c)| format!(" (did you mean \"{c}\"?)"))
        .unwrap_or_default()
}

/// Wrap `e` in a cast unless it already has type `to`.
fn cast(e: Expr, from: DataType, to: DataType) -> Result<Expr> {
    if from == to {
        return Ok(e);
    }
    if let Expr::Literal(v) = &e {
        // Fold literal casts right away (and surface invalid literals early).
        return Ok(Expr::Literal(cast_scalar(v, to)?));
    }
    Ok(Expr::Cast {
        expr: Box::new(e),
        data_type: to,
    })
}

/// Common supertype for UNION-like contexts (CASE branches, IN lists, COALESCE).
fn common_type(a: DataType, b: DataType) -> Option<DataType> {
    use DataType::*;
    Some(match (a, b) {
        _ if a == b => a,
        (Null, t) | (t, Null) => t,
        (Int64, Float64) | (Float64, Int64) => Float64,
        _ => return None,
    })
}

fn is_string_literal(e: &Expr) -> bool {
    matches!(e, Expr::Literal(ScalarValue::Utf8(_)))
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Binder { catalog, ctes: vec![] }
    }

    pub fn bind_query(&mut self, q: &ast::Query) -> Result<LogicalPlan> {
        let mut scope = HashMap::new();
        self.ctes.push(HashMap::new());
        for cte in &q.ctes {
            let plan = self.bind_query(&cte.query)?;
            scope.insert(cte.name.value.clone(), plan.clone());
            // Later CTEs may refer to earlier ones.
            self.ctes.last_mut().unwrap().insert(cte.name.value.clone(), plan);
        }
        let result = self.bind_select(&q.body, q);
        self.ctes.pop();
        result
    }

    fn lookup_cte(&self, name: &str) -> Option<LogicalPlan> {
        self.ctes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    // ----- FROM ----------------------------------------------------------

    fn bind_table_ref(&mut self, t: &TableRef) -> Result<LogicalPlan> {
        match t {
            TableRef::Table { name, alias } => {
                let qualifier = alias.as_ref().unwrap_or(name).value.clone();
                if let Some(plan) = self.lookup_cte(&name.value) {
                    return Ok(LogicalPlan::subquery_alias(plan, &qualifier));
                }
                let source = self.catalog.get(&name.value).ok_or_else(|| {
                    let names = self.catalog.table_names();
                    bind_err(
                        format!(
                            "table \"{}\" does not exist{}",
                            name.value,
                            suggestion(&name.value, names.iter().map(String::as_str))
                        ),
                        name.span,
                    )
                })?;
                Ok(LogicalPlan::TableScan(TableScan::new(&name.value, &qualifier, source)))
            }
            TableRef::File { path, span, alias } => {
                let table = crate::datasource::load_file(path).map_err(|e| e.with_span(*span))?;
                let stem = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_lowercase();
                let qualifier = alias.as_ref().map_or(stem, |a| a.value.clone());
                Ok(LogicalPlan::TableScan(TableScan::new(
                    path,
                    &qualifier,
                    Arc::new(table),
                )))
            }
            TableRef::Subquery { query, alias, .. } => {
                let plan = self.bind_query(query)?;
                let alias = alias.as_ref().map_or("subquery".to_string(), |a| a.value.clone());
                Ok(LogicalPlan::subquery_alias(plan, &alias))
            }
            TableRef::Join {
                left,
                right,
                kind,
                constraint,
            } => {
                let l = self.bind_table_ref(left)?;
                let r = self.bind_table_ref(right)?;
                let join_type = if *kind == JoinKind::Left {
                    JoinType::Left
                } else {
                    JoinType::Inner
                };
                let (ls, rs) = (l.schema(), r.schema());
                let combined = ls.join(&rs);
                let mut on = vec![];
                let mut residual = vec![];
                match constraint {
                    JoinConstraint::None => {}
                    JoinConstraint::On(expr) => {
                        let pred = self.bind_predicate(expr, &combined, "JOIN conditions")?;
                        for conj in split_conjunction(pred) {
                            match split_equi(&conj, &ls, &rs) {
                                Some(pair) => on.push(pair),
                                None => residual.push(conj),
                            }
                        }
                    }
                    JoinConstraint::Using(cols) => {
                        for c in cols {
                            let li = ls.index_of(None, &c.value)?;
                            let ri = rs.index_of(None, &c.value)?;
                            let (Some(li), Some(ri)) = (li, ri) else {
                                return Err(bind_err(
                                    format!("column \"{}\" in USING must exist on both sides", c.value),
                                    c.span,
                                ));
                            };
                            let lf = ls.field(li);
                            let rf = rs.field(ri);
                            let eq = self.coerce_binary(
                                Expr::Column(ColumnRef::new(lf.qualifier.as_deref(), &lf.name)),
                                BinaryOperator::Eq,
                                Expr::Column(ColumnRef::new(rf.qualifier.as_deref(), &rf.name)),
                                &combined,
                                c.span,
                            )?;
                            if let Expr::Binary { left, right, .. } = eq {
                                on.push((*left, *right));
                            }
                        }
                    }
                }
                Ok(LogicalPlan::join(l, r, join_type, on, conjunction(residual)))
            }
        }
    }

    // ----- SELECT --------------------------------------------------------

    fn bind_select(&mut self, s: &ast::Select, q: &ast::Query) -> Result<LogicalPlan> {
        let mut plan = match &s.from {
            Some(t) => self.bind_table_ref(t)?,
            None => LogicalPlan::Values {
                rows: vec![vec![]],
                schema: Arc::new(Schema::empty()),
            },
        };
        if let Some(w) = &s.selection {
            let pred = self.bind_predicate(w, &plan.schema(), "WHERE")?;
            plan = LogicalPlan::filter(plan, pred);
        }
        let input_schema = plan.schema();

        // 1. Bind select list against the input (aggregates allowed).
        let mut select: Vec<(Expr, Option<String>, Span)> = vec![];
        for item in &s.projection {
            match item {
                SelectItem::Wildcard(span) => {
                    if input_schema.is_empty() && s.from.is_none() {
                        return Err(bind_err("SELECT * requires a FROM clause", *span));
                    }
                    for f in &input_schema.fields {
                        select.push((
                            Expr::Column(ColumnRef::new(f.qualifier.as_deref(), &f.name)),
                            None,
                            *span,
                        ));
                    }
                }
                SelectItem::QualifiedWildcard(q) => {
                    let before = select.len();
                    for f in &input_schema.fields {
                        if f.qualifier.as_deref() == Some(q.value.as_str()) {
                            select.push((
                                Expr::Column(ColumnRef::new(f.qualifier.as_deref(), &f.name)),
                                None,
                                q.span,
                            ));
                        }
                    }
                    if select.len() == before {
                        return Err(bind_err(
                            format!("table \"{}\" not found in FROM clause", q.value),
                            q.span,
                        ));
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let e = self.bind_expr(expr, &input_schema, AggMode::Allow)?;
                    select.push((e, alias.as_ref().map(|a| a.value.clone()), expr.span()));
                }
            }
        }

        // 2. GROUP BY (ordinals and select aliases allowed).
        let mut groups = vec![];
        for g in &s.group_by {
            let e = match g {
                ast::Expr::Literal(Literal::Integer(n), span) => {
                    let idx = (*n as usize)
                        .checked_sub(1)
                        .filter(|i| *i < select.len())
                        .ok_or_else(|| bind_err(format!("GROUP BY position {n} is not in select list"), *span))?;
                    select[idx].0.clone()
                }
                ast::Expr::Identifier(id) if input_schema.index_of(None, &id.value)?.is_none() => {
                    match select.iter().find(|(_, a, _)| a.as_deref() == Some(id.value.as_str())) {
                        Some((e, _, _)) => e.clone(),
                        None => self.bind_expr(g, &input_schema, AggMode::Disallow("GROUP BY"))?,
                    }
                }
                _ => self.bind_expr(g, &input_schema, AggMode::Disallow("GROUP BY"))?,
            };
            if e.contains_aggregate() {
                return Err(bind_err("aggregate functions are not allowed in GROUP BY", g.span()));
            }
            if !groups.contains(&e) {
                groups.push(e);
            }
        }

        let having = s
            .having
            .as_ref()
            .map(|h| -> Result<(Expr, Span)> {
                let e = self.bind_expr(h, &input_schema, AggMode::Allow)?;
                self.expect_boolean(&e, &input_schema, "HAVING", h.span())?;
                Ok((e, h.span()))
            })
            .transpose()?;

        let is_agg = !groups.is_empty()
            || select.iter().any(|(e, _, _)| e.contains_aggregate())
            || having.as_ref().is_some_and(|(e, _)| e.contains_aggregate())
            || q.order_by.iter().any(|o| ast_has_aggregate(&o.expr));
        if having.is_some() && !is_agg {
            return Err(bind_err(
                "HAVING requires GROUP BY or aggregate functions",
                s.having.as_ref().unwrap().span(),
            ));
        }

        let mut agg_ctx = if is_agg {
            let group_names = groups
                .iter()
                .enumerate()
                .map(|(i, g)| match g {
                    Expr::Column(c) => c.clone(),
                    other => {
                        let base = other.to_string();
                        let dup = groups[..i].iter().any(|p| p.to_string() == base);
                        ColumnRef::new(None, &if dup { format!("{base}#{i}") } else { base })
                    }
                })
                .collect();
            Some(AggContext {
                groups: groups.clone(),
                group_names,
                aggs: vec![],
            })
        } else {
            None
        };

        // 3. Rewrite select list / HAVING over the aggregate output.
        if let Some(ctx) = agg_ctx.as_mut() {
            for (e, _, span) in select.iter_mut() {
                *e = ctx.rewrite(e, *span)?;
            }
        }
        let having = match (having, agg_ctx.as_mut()) {
            (Some((h, span)), Some(ctx)) => Some(ctx.rewrite(&h, span)?),
            _ => None,
        };

        // 4. ORDER BY: ordinal, output alias, select expression or hidden column.
        enum SortTarget {
            Output(usize),
            Hidden(Expr),
        }
        let mut sort_targets = vec![];
        for item in &q.order_by {
            let target = match &item.expr {
                ast::Expr::Literal(Literal::Integer(n), span) => {
                    let idx = (*n as usize)
                        .checked_sub(1)
                        .filter(|i| *i < select.len())
                        .ok_or_else(|| bind_err(format!("ORDER BY position {n} is not in select list"), *span))?;
                    SortTarget::Output(idx)
                }
                other => {
                    let alias_match = match other {
                        ast::Expr::Identifier(id) => select
                            .iter()
                            .position(|(_, a, _)| a.as_deref() == Some(id.value.as_str())),
                        _ => None,
                    };
                    match alias_match {
                        Some(i) => SortTarget::Output(i),
                        None => {
                            let mut e = self.bind_expr(other, &input_schema, AggMode::Allow)?;
                            if let Some(ctx) = agg_ctx.as_mut() {
                                e = ctx.rewrite(&e, other.span())?;
                            }
                            match select.iter().position(|(s, _, _)| *s == e) {
                                Some(i) => SortTarget::Output(i),
                                None => {
                                    if s.distinct {
                                        return Err(bind_err(
                                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                                            other.span(),
                                        ));
                                    }
                                    SortTarget::Hidden(e)
                                }
                            }
                        }
                    }
                }
            };
            sort_targets.push((target, item.asc, item.nulls_first.unwrap_or(!item.asc)));
        }

        // 5. Build Aggregate + HAVING.
        if let Some(ctx) = agg_ctx {
            let aggr_exprs = ctx
                .aggs
                .into_iter()
                .map(|(e, name)| {
                    if e.to_string() == name {
                        e
                    } else {
                        Expr::Alias(Box::new(e), name)
                    }
                })
                .collect();
            let group_exprs = ctx
                .groups
                .into_iter()
                .zip(ctx.group_names)
                .map(|(g, n)| match &g {
                    Expr::Column(_) => g,
                    _ if g.to_string() == n.name => g,
                    _ => Expr::Alias(Box::new(g), n.name),
                })
                .collect();
            plan = LogicalPlan::aggregate(plan, group_exprs, aggr_exprs)?;
            if let Some(h) = having {
                plan = LogicalPlan::filter(plan, h);
            }
        }

        // 6. Projection (with hidden sort columns appended).
        let mut proj_exprs: Vec<Expr> = select
            .iter()
            .map(|(e, alias, _)| match alias {
                Some(a) => Expr::Alias(Box::new(e.clone()), a.clone()),
                None => e.clone(),
            })
            .collect();
        let visible = proj_exprs.len();
        let mut hidden_names = vec![];
        for (t, _, _) in &sort_targets {
            if let SortTarget::Hidden(e) = t {
                let name = format!("__sort_{}", hidden_names.len());
                proj_exprs.push(Expr::Alias(Box::new(e.clone()), name.clone()));
                hidden_names.push(name);
            }
        }
        plan = LogicalPlan::projection(plan, proj_exprs)?;
        if s.distinct {
            let cols: Vec<Expr> = plan.output_columns().into_iter().map(Expr::Column).collect();
            plan = LogicalPlan::aggregate(plan, cols, vec![])?;
        }

        // 7. Sort / Limit.
        if !sort_targets.is_empty() {
            let outputs = plan.output_columns();
            let mut hidden_iter = hidden_names.iter();
            let exprs = sort_targets
                .into_iter()
                .map(|(t, asc, nulls_first)| {
                    let expr = match t {
                        SortTarget::Output(i) => Expr::Column(outputs[i].clone()),
                        SortTarget::Hidden(_) => Expr::Column(ColumnRef::new(None, hidden_iter.next().unwrap())),
                    };
                    SortExpr { expr, asc, nulls_first }
                })
                .collect();
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                exprs,
                fetch: None,
            };
            if !hidden_names.is_empty() {
                let keep: Vec<Expr> = plan
                    .output_columns()
                    .into_iter()
                    .take(visible)
                    .map(Expr::Column)
                    .collect();
                plan = LogicalPlan::projection(plan, keep)?;
            }
        }
        let fetch = q.limit.as_ref().map(|e| self.bind_count(e, "LIMIT")).transpose()?;
        let skip = q
            .offset
            .as_ref()
            .map(|e| self.bind_count(e, "OFFSET"))
            .transpose()?
            .unwrap_or(0);
        if fetch.is_some() || skip > 0 {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                skip,
                fetch,
            };
        }
        Ok(plan)
    }

    fn bind_count(&self, e: &ast::Expr, what: &str) -> Result<usize> {
        match e {
            ast::Expr::Literal(Literal::Integer(n), _) if *n >= 0 => Ok(*n as usize),
            other => Err(bind_err(
                format!("{what} must be a non-negative integer constant"),
                other.span(),
            )),
        }
    }

    fn bind_predicate(&self, e: &ast::Expr, schema: &Schema, ctx: &'static str) -> Result<Expr> {
        let bound = self.bind_expr(e, schema, AggMode::Disallow(ctx))?;
        self.expect_boolean(&bound, schema, ctx, e.span())?;
        Ok(bound)
    }

    fn expect_boolean(&self, e: &Expr, schema: &Schema, ctx: &str, span: Span) -> Result<()> {
        let t = e.data_type(schema)?;
        if t != DataType::Boolean && t != DataType::Null {
            return Err(type_err(format!("{ctx} condition must be BOOLEAN, got {t}"), span));
        }
        Ok(())
    }

    // ----- expressions -----------------------------------------------------

    fn bind_expr(&self, e: &ast::Expr, schema: &Schema, mode: AggMode) -> Result<Expr> {
        match e {
            ast::Expr::Identifier(id) => self.bind_column(None, id, schema),
            ast::Expr::CompoundIdentifier(parts) => {
                if parts.len() != 2 {
                    return Err(bind_err("only table.column references are supported", e.span()));
                }
                self.bind_column(Some(&parts[0]), &parts[1], schema)
            }
            ast::Expr::Literal(lit, span) => Ok(Expr::Literal(match lit {
                Literal::Integer(v) => ScalarValue::Int64(*v),
                Literal::Float(v) => ScalarValue::Float64(*v),
                Literal::String(s) => ScalarValue::Utf8(s.clone()),
                Literal::Boolean(b) => ScalarValue::Boolean(*b),
                Literal::Null => ScalarValue::Null(DataType::Null),
                Literal::Date(s) => ScalarValue::Date(
                    parse_date(s)
                        .ok_or_else(|| type_err(format!("invalid date literal '{s}' (expected YYYY-MM-DD)"), *span))?,
                ),
                Literal::Interval { months, days } => ScalarValue::Interval {
                    months: *months,
                    days: *days,
                },
            })),
            ast::Expr::Binary { left, op, right, span } => {
                let l = self.bind_expr(left, schema, mode)?;
                let r = self.bind_expr(right, schema, mode)?;
                self.coerce_binary(l, *op, r, schema, *span)
            }
            ast::Expr::Unary { op, expr, span } => {
                let inner = self.bind_expr(expr, schema, mode)?;
                let t = inner.data_type(schema)?;
                match op {
                    UnaryOperator::Not => {
                        if t != DataType::Boolean && t != DataType::Null {
                            return Err(type_err(format!("NOT requires a BOOLEAN operand, got {t}"), *span));
                        }
                        Ok(Expr::Not(Box::new(cast(inner, t, DataType::Boolean)?)))
                    }
                    UnaryOperator::Minus | UnaryOperator::Plus => {
                        if !t.is_numeric() && t != DataType::Null {
                            return Err(type_err(
                                format!("unary minus requires a numeric operand, got {t}"),
                                *span,
                            ));
                        }
                        Ok(if *op == UnaryOperator::Minus {
                            Expr::Negative(Box::new(inner))
                        } else {
                            inner
                        })
                    }
                }
            }
            ast::Expr::IsNull { expr, negated } => Ok(Expr::IsNull {
                expr: Box::new(self.bind_expr(expr, schema, mode)?),
                negated: *negated,
            }),
            ast::Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let span = expr.span();
                let x = self.bind_expr(expr, schema, mode)?;
                let lo = self.bind_expr(low, schema, mode)?;
                let hi = self.bind_expr(high, schema, mode)?;
                let ge = self.coerce_binary(x.clone(), BinaryOperator::GtEq, lo, schema, span)?;
                let le = self.coerce_binary(x, BinaryOperator::LtEq, hi, schema, span)?;
                let both = binary(ge, BinaryOperator::And, le);
                Ok(if *negated { Expr::Not(Box::new(both)) } else { both })
            }
            ast::Expr::InList { expr, list, negated } => {
                let span = expr.span();
                let x = self.bind_expr(expr, schema, mode)?;
                let items: Vec<Expr> = list
                    .iter()
                    .map(|i| self.bind_expr(i, schema, mode))
                    .collect::<Result<_>>()?;
                if items.iter().all(|i| matches!(i, Expr::Literal(_))) {
                    let xt = x.data_type(schema)?;
                    let mut target = xt;
                    for i in &items {
                        let it = i.data_type(schema)?;
                        if !(xt == DataType::Date && it == DataType::Utf8) {
                            target = common_type(target, it).ok_or_else(|| {
                                type_err(format!("IN list item of type {it} is not compatible with {xt}"), span)
                            })?;
                        }
                    }
                    let items = items
                        .into_iter()
                        .map(|i| {
                            let it = i.data_type(schema)?;
                            cast(i, it, target)
                        })
                        .collect::<Result<_>>()?;
                    return Ok(Expr::InList {
                        expr: Box::new(cast(x, xt, target)?),
                        list: items,
                        negated: *negated,
                    });
                }
                // Non-constant list: desugar into a disjunction of equalities.
                let mut disj = None;
                for i in items {
                    let eq = self.coerce_binary(x.clone(), BinaryOperator::Eq, i, schema, span)?;
                    disj = Some(match disj {
                        None => eq,
                        Some(d) => binary(d, BinaryOperator::Or, eq),
                    });
                }
                let d = disj.unwrap();
                Ok(if *negated { Expr::Not(Box::new(d)) } else { d })
            }
            ast::Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let x = self.bind_expr(expr, schema, mode)?;
                let p = self.bind_expr(pattern, schema, mode)?;
                for (v, s) in [(&x, expr.span()), (&p, pattern.span())] {
                    let t = v.data_type(schema)?;
                    if t != DataType::Utf8 && t != DataType::Null {
                        return Err(type_err(format!("LIKE requires VARCHAR operands, got {t}"), s));
                    }
                }
                Ok(Expr::Like {
                    expr: Box::new(x),
                    pattern: Box::new(p),
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                })
            }
            ast::Expr::Case {
                operand,
                branches,
                else_result,
                span,
            } => {
                let operand = operand.as_ref().map(|o| self.bind_expr(o, schema, mode)).transpose()?;
                let mut conds = vec![];
                let mut results = vec![];
                for (w, t) in branches {
                    let cond = match &operand {
                        Some(op) => {
                            let wv = self.bind_expr(w, schema, mode)?;
                            self.coerce_binary(op.clone(), BinaryOperator::Eq, wv, schema, w.span())?
                        }
                        None => {
                            let c = self.bind_expr(w, schema, mode)?;
                            self.expect_boolean(&c, schema, "CASE WHEN", w.span())?;
                            c
                        }
                    };
                    conds.push(cond);
                    results.push(self.bind_expr(t, schema, mode)?);
                }
                let else_expr = else_result
                    .as_ref()
                    .map(|e| self.bind_expr(e, schema, mode))
                    .transpose()?;
                let mut target = DataType::Null;
                for r in results.iter().chain(else_expr.iter()) {
                    let t = r.data_type(schema)?;
                    target = common_type(target, t).ok_or_else(|| {
                        type_err(format!("CASE branches have incompatible types {target} and {t}"), *span)
                    })?;
                }
                if target == DataType::Null {
                    target = DataType::Int64;
                }
                let when_then = conds
                    .into_iter()
                    .zip(results)
                    .map(|(c, r)| {
                        let t = r.data_type(schema)?;
                        Ok((c, cast(r, t, target)?))
                    })
                    .collect::<Result<_>>()?;
                let else_expr = match else_expr {
                    Some(e) => {
                        let t = e.data_type(schema)?;
                        Some(Box::new(cast(e, t, target)?))
                    }
                    None => Some(Box::new(Expr::Literal(ScalarValue::Null(target)))),
                };
                Ok(Expr::Case { when_then, else_expr })
            }
            ast::Expr::Cast { expr, data_type, span } => {
                let x = self.bind_expr(expr, schema, mode)?;
                let t = x.data_type(schema)?;
                let ok = t == *data_type
                    || t == DataType::Null
                    || *data_type == DataType::Utf8
                    || t == DataType::Utf8
                    || (t.is_numeric() && data_type.is_numeric())
                    || matches!(
                        (t, *data_type),
                        (DataType::Boolean, DataType::Int64 | DataType::Float64) | (DataType::Int64, DataType::Boolean)
                    );
                if !ok {
                    return Err(type_err(format!("cannot cast {t} to {data_type}"), *span));
                }
                cast(x, t, *data_type).map_err(|e| e.with_span(*span))
            }
            ast::Expr::Extract { field, expr } => {
                let func = match field.value.as_str() {
                    "year" => ScalarFunc::Year,
                    "month" => ScalarFunc::Month,
                    "day" => ScalarFunc::Day,
                    other => return Err(bind_err(format!("unsupported EXTRACT field \"{other}\""), field.span)),
                };
                let x = self.bind_expr(expr, schema, mode)?;
                let t = x.data_type(schema)?;
                if t != DataType::Date {
                    return Err(type_err(format!("EXTRACT requires a DATE, got {t}"), expr.span()));
                }
                Ok(Expr::Function { func, args: vec![x] })
            }
            ast::Expr::Function {
                name,
                args,
                distinct,
                star,
            } => self.bind_function(name, args, *distinct, *star, schema, mode),
            ast::Expr::Subquery(_, span) => Err(Error::new(
                ErrorKind::Unsupported,
                "subqueries in expressions are not supported (use a subquery in FROM or a CTE)",
            )
            .with_span(*span)),
        }
    }

    fn bind_column(&self, qualifier: Option<&ast::Ident>, id: &ast::Ident, schema: &Schema) -> Result<Expr> {
        let q = qualifier.map(|q| q.value.as_str());
        let span = match qualifier {
            Some(q) => Span::new(
                q.span.line,
                q.span.col,
                q.span.offset,
                id.span.offset + id.span.len - q.span.offset,
            ),
            None => id.span,
        };
        match schema.index_of(q, &id.value).map_err(|e| e.with_span(span))? {
            Some(i) => {
                let f = schema.field(i);
                Ok(Expr::Column(ColumnRef::new(f.qualifier.as_deref(), &f.name)))
            }
            None => {
                if let Some(q) = q
                    && !schema.fields.iter().any(|f| f.qualifier.as_deref() == Some(q))
                {
                    return Err(bind_err(format!("table \"{q}\" not found in FROM clause"), span));
                }
                let full = match q {
                    Some(q) => format!("{q}.{}", id.value),
                    None => id.value.clone(),
                };
                Err(bind_err(
                    format!(
                        "column \"{full}\" not found{}",
                        suggestion(&id.value, schema.fields.iter().map(|f| f.name.as_str()))
                    ),
                    span,
                ))
            }
        }
    }

    fn bind_function(
        &self,
        name: &ast::Ident,
        args: &[ast::Expr],
        distinct: bool,
        star: bool,
        schema: &Schema,
        mode: AggMode,
    ) -> Result<Expr> {
        if let Some(func) = AggFunc::from_name(&name.value) {
            if let AggMode::Disallow(ctx) = mode {
                return Err(bind_err(
                    format!("aggregate functions are not allowed in {ctx}"),
                    name.span,
                ));
            }
            if star {
                if func != AggFunc::Count {
                    return Err(bind_err(format!("{}(*) is not valid", name.value), name.span));
                }
                return Ok(Expr::Aggregate(Box::new(AggregateExpr {
                    func,
                    arg: None,
                    distinct: false,
                })));
            }
            if args.len() != 1 {
                return Err(bind_err(
                    format!("{}() takes exactly one argument", name.value),
                    name.span,
                ));
            }
            let arg = self.bind_expr(&args[0], schema, AggMode::Disallow("aggregate function arguments"))?;
            let t = arg.data_type(schema)?;
            let arg = match func {
                AggFunc::Sum | AggFunc::Avg => {
                    if !t.is_numeric() && t != DataType::Null {
                        return Err(type_err(
                            format!("{}() requires a numeric argument, got {t}", name.value),
                            args[0].span(),
                        ));
                    }
                    if t == DataType::Null {
                        cast(arg, t, DataType::Int64)?
                    } else {
                        arg
                    }
                }
                _ if t == DataType::Null => cast(arg, t, DataType::Int64)?,
                _ => arg,
            };
            return Ok(Expr::Aggregate(Box::new(AggregateExpr {
                func,
                arg: Some(arg),
                distinct,
            })));
        }
        let Some(func) = ScalarFunc::from_name(&name.value) else {
            return Err(bind_err(format!("unknown function \"{}\"", name.value), name.span));
        };
        if distinct || star {
            return Err(bind_err(
                format!("{}() is not an aggregate function", name.value),
                name.span,
            ));
        }
        let (min, max) = func.arity();
        if args.len() < min || args.len() > max {
            return Err(bind_err(
                format!("wrong number of arguments to {}()", name.value),
                name.span,
            ));
        }
        let mut bound: Vec<Expr> = args
            .iter()
            .map(|a| self.bind_expr(a, schema, mode))
            .collect::<Result<_>>()?;
        let types: Vec<DataType> = bound.iter().map(|b| b.data_type(schema)).collect::<Result<_>>()?;
        let want = |i: usize, ok: &dyn Fn(DataType) -> bool, desc: &str| -> Result<()> {
            if ok(types[i]) || types[i] == DataType::Null {
                Ok(())
            } else {
                Err(type_err(
                    format!("{}() expects {desc} argument, got {}", name.value, types[i]),
                    args[i].span(),
                ))
            }
        };
        use ScalarFunc::*;
        match func {
            Abs | Round | Floor | Ceil | Sqrt => {
                want(0, &|t| t.is_numeric(), "a numeric")?;
                if func == Round && bound.len() == 2 {
                    want(1, &|t| t == DataType::Int64, "an integer")?;
                }
                if func == Sqrt || types[0] == DataType::Null {
                    bound[0] = cast(bound[0].clone(), types[0], DataType::Float64)?;
                }
            }
            Upper | Lower | Trim | Length => want(0, &|t| t == DataType::Utf8, "a VARCHAR")?,
            Substr => {
                want(0, &|t| t == DataType::Utf8, "a VARCHAR")?;
                for i in 1..bound.len() {
                    want(i, &|t| t == DataType::Int64, "an integer")?;
                }
            }
            Year | Month | Day => want(0, &|t| t == DataType::Date, "a DATE")?,
            Coalesce | NullIf => {
                let mut target = DataType::Null;
                for (i, t) in types.iter().enumerate() {
                    target = common_type(target, *t).ok_or_else(|| {
                        type_err(
                            format!("{}() arguments have incompatible types {target} and {t}", name.value),
                            args[i].span(),
                        )
                    })?;
                }
                if target == DataType::Null {
                    target = DataType::Int64;
                }
                for (b, t) in bound.iter_mut().zip(&types) {
                    *b = cast(b.clone(), *t, target)?;
                }
            }
        }
        Ok(Expr::Function { func, args: bound })
    }

    /// Type-check a binary operator and insert implicit casts.
    pub(crate) fn coerce_binary(
        &self,
        l: Expr,
        op: BinaryOperator,
        r: Expr,
        schema: &Schema,
        span: Span,
    ) -> Result<Expr> {
        use DataType::*;
        let lt = l.data_type(schema)?;
        let rt = r.data_type(schema)?;
        let incompatible = || type_err(format!("cannot apply operator '{op}' to types {lt} and {rt}"), span);
        let (l, r) = match op {
            BinaryOperator::And | BinaryOperator::Or => {
                for t in [lt, rt] {
                    if t != Boolean && t != Null {
                        return Err(type_err(format!("{op} requires BOOLEAN operands, got {t}"), span));
                    }
                }
                (cast(l, lt, Boolean)?, cast(r, rt, Boolean)?)
            }
            _ if op.is_comparison() => match (lt, rt) {
                _ if lt == rt => (l, r),
                (Null, t) | (t, Null) if t != Interval => (cast(l, lt, t)?, cast(r, rt, t)?),
                (Int64, Float64) | (Float64, Int64) => (cast(l, lt, Float64)?, cast(r, rt, Float64)?),
                (Date, Utf8) if is_string_literal(&r) => (l, cast(r, rt, Date).map_err(|e| e.with_span(span))?),
                (Utf8, Date) if is_string_literal(&l) => (cast(l, lt, Date).map_err(|e| e.with_span(span))?, r),
                _ => return Err(type_err(format!("cannot compare {lt} with {rt}"), span)),
            },
            BinaryOperator::Concat => (cast(l, lt, Utf8)?, cast(r, rt, Utf8)?),
            _ => match (lt, rt) {
                (Int64, Int64) | (Float64, Float64) => (l, r),
                (Int64, Float64) | (Float64, Int64) => (cast(l, lt, Float64)?, cast(r, rt, Float64)?),
                (Null, t) | (t, Null) if t.is_numeric() || t == Null => {
                    let t = if t == Null { Int64 } else { t };
                    (cast(l, lt, t)?, cast(r, rt, t)?)
                }
                (Date, Int64) | (Date, Interval) if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus) => {
                    (l, r)
                }
                (Int64, Date) | (Interval, Date) if op == BinaryOperator::Plus => (r, l),
                (Date, Date) if op == BinaryOperator::Minus => (l, r),
                (Date, Null) => (l, cast(r, rt, Int64)?),
                _ => return Err(incompatible()),
            },
        };
        Ok(binary(l, op, r))
    }
}

/// If `conj` is `a = b` with `a` from the left input and `b` from the right
/// (or vice versa), return the pair oriented (left, right).
pub fn split_equi(conj: &Expr, left: &Schema, right: &Schema) -> Option<(Expr, Expr)> {
    let Expr::Binary {
        left: a,
        op: BinaryOperator::Eq,
        right: b,
    } = conj
    else {
        return None;
    };
    let side = |e: &Expr, s: &Schema| {
        let cols = e.columns();
        !cols.is_empty()
            && cols
                .iter()
                .all(|c| matches!(s.index_of(c.relation.as_deref(), &c.name), Ok(Some(_))))
    };
    if side(a, left) && side(b, right) {
        Some(((**a).clone(), (**b).clone()))
    } else if side(a, right) && side(b, left) {
        Some(((**b).clone(), (**a).clone()))
    } else {
        None
    }
}

fn ast_has_aggregate(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Function { name, args, .. } => {
            AggFunc::from_name(&name.value).is_some() || args.iter().any(ast_has_aggregate)
        }
        E::Binary { left, right, .. } => ast_has_aggregate(left) || ast_has_aggregate(right),
        E::Unary { expr, .. } | E::IsNull { expr, .. } | E::Cast { expr, .. } | E::Extract { expr, .. } => {
            ast_has_aggregate(expr)
        }
        E::Between { expr, low, high, .. } => {
            ast_has_aggregate(expr) || ast_has_aggregate(low) || ast_has_aggregate(high)
        }
        E::InList { expr, list, .. } => ast_has_aggregate(expr) || list.iter().any(ast_has_aggregate),
        E::Like { expr, pattern, .. } => ast_has_aggregate(expr) || ast_has_aggregate(pattern),
        E::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            operand.as_deref().is_some_and(ast_has_aggregate)
                || branches
                    .iter()
                    .any(|(w, t)| ast_has_aggregate(w) || ast_has_aggregate(t))
                || else_result.as_deref().is_some_and(ast_has_aggregate)
        }
        _ => false,
    }
}

impl Binder<'_> {
    /// Bind an expression that may not reference any columns (e.g. `VALUES` items).
    pub fn bind_constant(&self, e: &ast::Expr) -> Result<Expr> {
        self.bind_expr(e, &Schema::empty(), AggMode::Disallow("VALUES"))
    }
}
