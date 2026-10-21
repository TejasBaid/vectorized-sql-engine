//! Abstract syntax tree produced by the parser.

use crate::error::Span;
use crate::types::DataType;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub value: String,
    pub span: Span,
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Box<Query>),
    Explain {
        analyze: bool,
        verbose: bool,
        query: Box<Query>,
    },
    CreateTable {
        name: Ident,
        columns: Vec<(Ident, DataType)>,
    },
    CreateTableAs {
        name: Ident,
        query: Box<Query>,
    },
    Insert {
        table: Ident,
        columns: Option<Vec<Ident>>,
        source: InsertSource,
    },
    DropTable {
        name: Ident,
        if_exists: bool,
    },
    ShowTables,
    Describe {
        name: Ident,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: Ident,
    pub query: Box<Query>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub ctes: Vec<Cte>,
    pub body: Box<Select>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Option<TableRef>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard(Span),
    QualifiedWildcard(Ident),
    Expr { expr: Expr, alias: Option<Ident> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinConstraint {
    None,
    On(Expr),
    Using(Vec<Ident>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: Ident,
        alias: Option<Ident>,
    },
    /// `FROM 'data/lineitem.parquet'` (DuckDB-style direct file query).
    File {
        path: String,
        span: Span,
        alias: Option<Ident>,
    },
    Subquery {
        query: Box<Query>,
        alias: Option<Ident>,
        span: Span,
    },
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        constraint: JoinConstraint,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOperator {
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Concat,
}

impl BinaryOperator {
    pub fn is_comparison(self) -> bool {
        use BinaryOperator::*;
        matches!(self, Eq | NotEq | Lt | LtEq | Gt | GtEq)
    }

    pub fn is_arithmetic(self) -> bool {
        use BinaryOperator::*;
        matches!(self, Plus | Minus | Multiply | Divide | Modulo)
    }

    /// Operator with operands swapped: `a < b` <=> `b > a`.
    pub fn swap(self) -> Self {
        use BinaryOperator::*;
        match self {
            Lt => Gt,
            LtEq => GtEq,
            Gt => Lt,
            GtEq => LtEq,
            other => other,
        }
    }

    /// Binding power used both by the Pratt parser and the expression printer.
    pub fn precedence(self) -> u8 {
        use BinaryOperator::*;
        match self {
            Or => 1,
            And => 2,
            Eq | NotEq | Lt | LtEq | Gt | GtEq => 4,
            Concat => 6,
            Plus | Minus => 7,
            Multiply | Divide | Modulo => 8,
        }
    }
}

impl fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use BinaryOperator::*;
        f.write_str(match self {
            Plus => "+",
            Minus => "-",
            Multiply => "*",
            Divide => "/",
            Modulo => "%",
            Eq => "=",
            NotEq => "<>",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            And => "AND",
            Or => "OR",
            Concat => "||",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
    Minus,
    Plus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
    Date(String),
    Interval { months: i32, days: i32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Identifier(Ident),
    CompoundIdentifier(Vec<Ident>),
    Literal(Literal, Span),
    Binary {
        left: Box<Expr>,
        op: BinaryOperator,
        right: Box<Expr>,
        span: Span,
    },
    Unary {
        op: UnaryOperator,
        expr: Box<Expr>,
        span: Span,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        negated: bool,
        low: Box<Expr>,
        high: Box<Expr>,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
        span: Span,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
        span: Span,
    },
    Function {
        name: Ident,
        args: Vec<Expr>,
        distinct: bool,
        /// `COUNT(*)`
        star: bool,
    },
    Extract {
        field: Ident,
        expr: Box<Expr>,
    },
    /// A parenthesised subquery used as an expression (not supported by the binder yet).
    Subquery(Box<Query>, Span),
}

impl Expr {
    /// Best-effort source position of the expression, for error messages.
    pub fn span(&self) -> Span {
        match self {
            Expr::Identifier(i) => i.span,
            Expr::CompoundIdentifier(parts) => {
                let first = parts[0].span;
                let last = parts[parts.len() - 1].span;
                Span::new(
                    first.line,
                    first.col,
                    first.offset,
                    last.offset + last.len - first.offset,
                )
            }
            Expr::Literal(_, s) => *s,
            Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Case { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Subquery(_, span) => *span,
            Expr::IsNull { expr, .. }
            | Expr::Between { expr, .. }
            | Expr::InList { expr, .. }
            | Expr::Like { expr, .. } => expr.span(),
            Expr::Function { name, .. } | Expr::Extract { field: name, .. } => name.span,
        }
    }
}
