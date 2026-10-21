//! Bound (resolved and type-checked) expressions used by logical plans.

use crate::ast::BinaryOperator;
use crate::column::Schema;
use crate::compute::functions::ScalarFunc;
use crate::error::{Error, ErrorKind, Result};
use crate::types::{DataType, ScalarValue};
use std::collections::BTreeSet;
use std::fmt;

/// A reference to a column of the input relation by (qualifier, name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnRef {
    pub relation: Option<String>,
    pub name: String,
}

impl ColumnRef {
    pub fn new(relation: Option<&str>, name: &str) -> Self {
        ColumnRef {
            relation: relation.map(str::to_string),
            name: name.to_string(),
        }
    }

    /// Resolve this reference to an index in `schema`.
    pub fn index_in(&self, schema: &Schema) -> Result<usize> {
        schema.index_of(self.relation.as_deref(), &self.name)?.ok_or_else(|| {
            Error::new(
                ErrorKind::Plan,
                format!("internal error: column {self} not found in schema {schema}"),
            )
        })
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.relation {
            Some(r) => write!(f, "{r}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" | "mean" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub func: AggFunc,
    /// `None` means `COUNT(*)`.
    pub arg: Option<Expr>,
    pub distinct: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(ColumnRef),
    Literal(ScalarValue),
    Alias(Box<Expr>, String),
    Binary {
        left: Box<Expr>,
        op: BinaryOperator,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    Negative(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    /// All list items are literals (the binder desugars other cases to ORs).
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Case {
        when_then: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    Function {
        func: ScalarFunc,
        args: Vec<Expr>,
    },
    Aggregate(Box<AggregateExpr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortExpr {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: bool,
}

impl fmt::Display for SortExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}{}",
            self.expr,
            if self.asc { "ASC" } else { "DESC" },
            if self.nulls_first == self.asc {
                " NULLS FIRST"
            } else {
                ""
            }
        )
    }
}

pub fn col(relation: Option<&str>, name: &str) -> Expr {
    Expr::Column(ColumnRef::new(relation, name))
}

pub fn lit(v: ScalarValue) -> Expr {
    Expr::Literal(v)
}

pub fn binary(left: Expr, op: BinaryOperator, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

/// Split `a AND b AND c` into `[a, b, c]`.
pub fn split_conjunction(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut v = split_conjunction(*left);
            v.extend(split_conjunction(*right));
            v
        }
        other => vec![other],
    }
}

/// Inverse of [`split_conjunction`].
pub fn conjunction(exprs: Vec<Expr>) -> Option<Expr> {
    exprs.into_iter().reduce(|a, b| binary(a, BinaryOperator::And, b))
}

impl Expr {
    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Column(_) | Expr::Literal(_) => vec![],
            Expr::Alias(e, _) | Expr::Not(e) | Expr::Negative(e) => vec![e],
            Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => vec![expr],
            Expr::Binary { left, right, .. } => vec![left, right],
            Expr::Like { expr, pattern, .. } => vec![expr, pattern],
            Expr::InList { expr, list, .. } => std::iter::once(expr.as_ref()).chain(list).collect(),
            Expr::Case { when_then, else_expr } => {
                let mut v: Vec<&Expr> = when_then.iter().flat_map(|(w, t)| [w, t]).collect();
                if let Some(e) = else_expr {
                    v.push(e);
                }
                v
            }
            Expr::Function { args, .. } => args.iter().collect(),
            Expr::Aggregate(a) => a.arg.iter().collect(),
        }
    }

    /// Rebuild the expression bottom-up, applying `f` to every node after its children.
    pub fn transform_up(self, f: &mut impl FnMut(Expr) -> Result<Expr>) -> Result<Expr> {
        let bx = |e: Box<Expr>, f: &mut dyn FnMut(Expr) -> Result<Expr>| -> Result<Box<Expr>> {
            Ok(Box::new(transform_dyn(*e, f)?))
        };
        let rebuilt = match self {
            e @ (Expr::Column(_) | Expr::Literal(_)) => e,
            Expr::Alias(e, n) => Expr::Alias(bx(e, f)?, n),
            Expr::Not(e) => Expr::Not(bx(e, f)?),
            Expr::Negative(e) => Expr::Negative(bx(e, f)?),
            Expr::IsNull { expr, negated } => Expr::IsNull {
                expr: bx(expr, f)?,
                negated,
            },
            Expr::Cast { expr, data_type } => Expr::Cast {
                expr: bx(expr, f)?,
                data_type,
            },
            Expr::Binary { left, op, right } => Expr::Binary {
                left: bx(left, f)?,
                op,
                right: bx(right, f)?,
            },
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => Expr::Like {
                expr: bx(expr, f)?,
                pattern: bx(pattern, f)?,
                negated,
                case_insensitive,
            },
            Expr::InList { expr, list, negated } => Expr::InList {
                expr: bx(expr, f)?,
                list: list.into_iter().map(|e| transform_dyn(e, f)).collect::<Result<_>>()?,
                negated,
            },
            Expr::Case { when_then, else_expr } => Expr::Case {
                when_then: when_then
                    .into_iter()
                    .map(|(w, t)| Ok((transform_dyn(w, f)?, transform_dyn(t, f)?)))
                    .collect::<Result<_>>()?,
                else_expr: else_expr.map(|e| bx(e, f)).transpose()?,
            },
            Expr::Function { func, args } => Expr::Function {
                func,
                args: args.into_iter().map(|e| transform_dyn(e, f)).collect::<Result<_>>()?,
            },
            Expr::Aggregate(a) => {
                let AggregateExpr { func, arg, distinct } = *a;
                Expr::Aggregate(Box::new(AggregateExpr {
                    func,
                    arg: arg.map(|e| transform_dyn(e, f)).transpose()?,
                    distinct,
                }))
            }
        };
        f(rebuilt)
    }

    /// True if any node satisfies `pred`.
    pub fn any(&self, pred: &impl Fn(&Expr) -> bool) -> bool {
        pred(self) || self.children().into_iter().any(|c| c.any(pred))
    }

    pub fn contains_aggregate(&self) -> bool {
        self.any(&|e| matches!(e, Expr::Aggregate(_)))
    }

    pub fn collect_columns(&self, out: &mut BTreeSet<ColumnRef>) {
        if let Expr::Column(c) = self {
            out.insert(c.clone());
        }
        for c in self.children() {
            c.collect_columns(out);
        }
    }

    pub fn columns(&self) -> BTreeSet<ColumnRef> {
        let mut s = BTreeSet::new();
        self.collect_columns(&mut s);
        s
    }

    /// Strip a top-level alias.
    pub fn unalias(&self) -> &Expr {
        match self {
            Expr::Alias(e, _) => e.unalias(),
            e => e,
        }
    }

    pub fn data_type(&self, schema: &Schema) -> Result<DataType> {
        use BinaryOperator::*;
        Ok(match self {
            Expr::Column(c) => schema.field(c.index_in(schema)?).data_type,
            Expr::Literal(v) => v.data_type(),
            Expr::Alias(e, _) | Expr::Negative(e) => e.data_type(schema)?,
            Expr::Binary { left, op, right } => match op {
                Eq | NotEq | Lt | LtEq | Gt | GtEq | And | Or => DataType::Boolean,
                Concat => DataType::Utf8,
                _ => {
                    let (l, r) = (left.data_type(schema)?, right.data_type(schema)?);
                    match (l, r) {
                        (DataType::Date, DataType::Date) => DataType::Int64,
                        (DataType::Date, _) | (_, DataType::Date) => DataType::Date,
                        (DataType::Float64, _) | (_, DataType::Float64) => DataType::Float64,
                        (DataType::Null, other) | (other, DataType::Null) => other,
                        _ => DataType::Int64,
                    }
                }
            },
            Expr::Not(_) | Expr::IsNull { .. } | Expr::Like { .. } | Expr::InList { .. } => DataType::Boolean,
            Expr::Case { when_then, else_expr } => {
                let mut t = DataType::Null;
                for (_, then) in when_then {
                    t = then.data_type(schema)?;
                    if t != DataType::Null {
                        return Ok(t);
                    }
                }
                if let Some(e) = else_expr {
                    t = e.data_type(schema)?;
                }
                t
            }
            Expr::Cast { data_type, .. } => *data_type,
            Expr::Function { func, args } => {
                use ScalarFunc::*;
                match func {
                    Length | Year | Month | Day => DataType::Int64,
                    Sqrt => DataType::Float64,
                    Upper | Lower | Trim | Substr => DataType::Utf8,
                    Coalesce => {
                        for a in args {
                            let t = a.data_type(schema)?;
                            if t != DataType::Null {
                                return Ok(t);
                            }
                        }
                        DataType::Null
                    }
                    Abs | Round | Floor | Ceil | NullIf => args[0].data_type(schema)?,
                }
            }
            Expr::Aggregate(a) => match a.func {
                AggFunc::Count => DataType::Int64,
                AggFunc::Avg => DataType::Float64,
                AggFunc::Sum | AggFunc::Min | AggFunc::Max => match &a.arg {
                    Some(e) => e.data_type(schema)?,
                    None => DataType::Int64,
                },
            },
        })
    }

    /// Name of the output column this expression produces in a projection.
    pub fn output_name(&self) -> String {
        match self {
            Expr::Alias(_, name) => name.clone(),
            Expr::Column(c) => c.name.clone(),
            other => other.to_string(),
        }
    }

    fn precedence(&self) -> u8 {
        match self {
            Expr::Binary { op, .. } => op.precedence(),
            Expr::Not(_) => 3,
            Expr::IsNull { .. } | Expr::Like { .. } | Expr::InList { .. } => 4,
            _ => 10,
        }
    }

    fn fmt_child(&self, child: &Expr, f: &mut fmt::Formatter<'_>, right: bool) -> fmt::Result {
        let p = self.precedence();
        let cp = child.precedence();
        if cp < p || (right && cp == p && cp < 10) {
            write!(f, "({child})")
        } else {
            write!(f, "{child}")
        }
    }
}

fn transform_dyn(e: Expr, f: &mut dyn FnMut(Expr) -> Result<Expr>) -> Result<Expr> {
    let mut g = |x: Expr| f(x);
    e.transform_up(&mut g)
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Column(c) => f.write_str(&c.name),
            Expr::Literal(ScalarValue::Utf8(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            Expr::Literal(ScalarValue::Date(d)) => write!(f, "DATE '{}'", crate::types::format_date(*d)),
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Alias(e, name) => write!(f, "{e} AS {name}"),
            Expr::Binary { left, op, right } => {
                self.fmt_child(left, f, false)?;
                write!(f, " {op} ")?;
                self.fmt_child(right, f, true)
            }
            Expr::Not(e) => {
                f.write_str("NOT ")?;
                self.fmt_child(e, f, false)
            }
            Expr::Negative(e) => {
                f.write_str("-")?;
                self.fmt_child(e, f, false)
            }
            Expr::IsNull { expr, negated } => {
                self.fmt_child(expr, f, false)?;
                f.write_str(if *negated { " IS NOT NULL" } else { " IS NULL" })
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                self.fmt_child(expr, f, false)?;
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(f, " {}{kw} ", if *negated { "NOT " } else { "" })?;
                self.fmt_child(pattern, f, true)
            }
            Expr::InList { expr, list, negated } => {
                self.fmt_child(expr, f, false)?;
                let items: Vec<String> = list.iter().map(|e| e.to_string()).collect();
                write!(f, " {}IN ({})", if *negated { "NOT " } else { "" }, items.join(", "))
            }
            Expr::Case { when_then, else_expr } => {
                f.write_str("CASE")?;
                for (w, t) in when_then {
                    write!(f, " WHEN {w} THEN {t}")?;
                }
                if let Some(e) = else_expr {
                    write!(f, " ELSE {e}")?;
                }
                f.write_str(" END")
            }
            Expr::Cast { expr, data_type } => write!(f, "CAST({expr} AS {data_type})"),
            Expr::Function { func, args } => {
                let a: Vec<String> = args.iter().map(|e| e.to_string()).collect();
                write!(f, "{func}({})", a.join(", "))
            }
            Expr::Aggregate(a) => match &a.arg {
                None => write!(f, "{}(*)", a.func.name()),
                Some(arg) => write!(
                    f,
                    "{}({}{arg})",
                    a.func.name(),
                    if a.distinct { "DISTINCT " } else { "" }
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_minimal_parens() {
        let e = binary(
            col(None, "a"),
            BinaryOperator::Multiply,
            binary(lit(ScalarValue::Int64(1)), BinaryOperator::Minus, col(None, "b")),
        );
        assert_eq!(e.to_string(), "a * (1 - b)");
        let e = binary(
            binary(col(None, "a"), BinaryOperator::Minus, col(None, "b")),
            BinaryOperator::Minus,
            col(None, "c"),
        );
        assert_eq!(e.to_string(), "a - b - c");
    }

    #[test]
    fn conjunction_roundtrip() {
        let parts = vec![col(None, "a"), col(None, "b"), col(None, "c")];
        let c = conjunction(parts.clone()).unwrap();
        assert_eq!(split_conjunction(c), parts);
    }
}
