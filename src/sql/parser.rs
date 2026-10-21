//! Recursive-descent parser for statements with a Pratt (precedence-climbing)
//! parser for expressions.
//!
//! Binding powers (higher binds tighter):
//!
//! | bp | operators                                   |
//! |----|---------------------------------------------|
//! | 1  | `OR`                                        |
//! | 2  | `AND`                                       |
//! | 3  | prefix `NOT`                                |
//! | 4  | comparisons, `IS`, `LIKE`, `IN`, `BETWEEN`  |
//! | 6  | `\|\|`                                      |
//! | 7  | `+ -`                                       |
//! | 8  | `* / %`                                     |
//! | 9  | prefix `-` / `+`                            |
//! | 10 | `::` cast                                   |

use super::ast::*;
use super::lexer::{Token, TokenKind, tokenize};
use crate::error::{Error, ErrorKind, Result, Span};
use crate::types::DataType;

/// Words that cannot be used as implicit (AS-less) aliases.
const RESERVED: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "having",
    "order",
    "limit",
    "offset",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "on",
    "using",
    "union",
    "as",
    "and",
    "or",
    "not",
    "by",
    "with",
    "case",
    "when",
    "then",
    "else",
    "end",
    "is",
    "null",
    "in",
    "between",
    "like",
    "ilike",
    "asc",
    "desc",
    "distinct",
    "natural",
    "values",
    "except",
    "intersect",
    "nulls",
    "exists",
    "true",
    "false",
    "all",
];

const PREC_NOT: u8 = 3;
const PREC_CMP: u8 = 4;
const PREC_UNARY: u8 = 9;
const PREC_CAST: u8 = 10;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

/// Parse a script of `;`-separated statements.
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let mut p = Parser::new(sql)?;
    let mut stmts = Vec::new();
    loop {
        while p.consume(&TokenKind::Semicolon) {}
        if p.peek_kind() == &TokenKind::Eof {
            return Ok(stmts);
        }
        stmts.push(p.parse_statement()?);
        if p.peek_kind() != &TokenKind::Eof && !p.consume(&TokenKind::Semicolon) {
            return Err(p.unexpected("end of statement"));
        }
    }
}

/// Parse exactly one statement.
pub fn parse_statement(sql: &str) -> Result<Statement> {
    let mut stmts = parse_sql(sql)?;
    match stmts.len() {
        1 => Ok(stmts.pop().unwrap()),
        0 => Err(Error::new(ErrorKind::Parse, "empty statement")),
        n => Err(Error::new(
            ErrorKind::Parse,
            format!("expected one statement, found {n}"),
        )),
    }
}

/// Parse a standalone expression (used by tests and tools).
pub fn parse_expr(sql: &str) -> Result<Expr> {
    let mut p = Parser::new(sql)?;
    let e = p.parse_expr()?;
    p.expect(&TokenKind::Eof, "end of expression")?;
    Ok(e)
}

impl Parser {
    pub fn new(sql: &str) -> Result<Self> {
        Ok(Parser {
            tokens: tokenize(sql)?,
            pos: 0,
        })
    }

    // ----- token helpers -------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn peek_nth_kind(&self, n: usize) -> &TokenKind {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)].kind
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn consume(&mut self, kind: &TokenKind) -> bool {
        if self.peek_kind() == kind {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token> {
        if self.peek_kind() == kind {
            Ok(self.next())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, expected: &str) -> Error {
        let t = self.peek();
        Error::new(
            ErrorKind::Parse,
            format!("expected {expected}, found {}", t.kind.describe()),
        )
        .with_span(t.span)
    }

    fn is_keyword_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_nth_kind(n), TokenKind::Word { value, quoted: false } if value == kw)
    }

    fn is_keyword(&self, kw: &str) -> bool {
        self.is_keyword_at(0, kw)
    }

    fn consume_keyword(&mut self, kw: &str) -> bool {
        if self.is_keyword(kw) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<Token> {
        if self.is_keyword(kw) {
            Ok(self.next())
        } else {
            Err(self.unexpected(&kw.to_uppercase()))
        }
    }

    fn parse_ident(&mut self) -> Result<Ident> {
        match self.peek_kind().clone() {
            TokenKind::Word { value, quoted } if quoted || !RESERVED.contains(&value.as_str()) => {
                let t = self.next();
                Ok(Ident { value, span: t.span })
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// `[AS] alias` where the AS-less form only accepts non-reserved words.
    fn parse_optional_alias(&mut self) -> Result<Option<Ident>> {
        if self.consume_keyword("as") {
            return self.parse_ident().map(Some);
        }
        match self.peek_kind() {
            TokenKind::Word { value, quoted } if *quoted || !RESERVED.contains(&value.as_str()) => {
                self.parse_ident().map(Some)
            }
            _ => Ok(None),
        }
    }

    // ----- statements ----------------------------------------------------

    pub fn parse_statement(&mut self) -> Result<Statement> {
        if self.is_keyword("select") || self.is_keyword("with") {
            return Ok(Statement::Query(Box::new(self.parse_query()?)));
        }
        if self.consume_keyword("explain") {
            let analyze = self.consume_keyword("analyze");
            let verbose = self.consume_keyword("verbose");
            let query = Box::new(self.parse_query()?);
            return Ok(Statement::Explain {
                analyze,
                verbose,
                query,
            });
        }
        if self.consume_keyword("create") {
            self.expect_keyword("table")?;
            let name = self.parse_ident()?;
            if self.consume_keyword("as") {
                let query = Box::new(self.parse_query()?);
                return Ok(Statement::CreateTableAs { name, query });
            }
            self.expect(&TokenKind::LParen, "\"(\" or AS")?;
            let mut columns = Vec::new();
            loop {
                let col = self.parse_ident()?;
                let ty = self.parse_data_type()?;
                // Tolerate (and ignore) simple column constraints.
                loop {
                    if self.consume_keyword("not") {
                        self.expect_keyword("null")?;
                    } else if self.consume_keyword("null") {
                    } else if self.consume_keyword("primary") {
                        self.expect_keyword("key")?;
                    } else {
                        break;
                    }
                }
                columns.push((col, ty));
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "\",\" or \")\"")?;
            return Ok(Statement::CreateTable { name, columns });
        }
        if self.consume_keyword("insert") {
            self.expect_keyword("into")?;
            let table = self.parse_ident()?;
            let columns = if self.peek_kind() == &TokenKind::LParen
                && !(self.is_keyword_at(1, "select") || self.is_keyword_at(1, "with"))
            {
                self.next();
                let cols = self.parse_comma_separated(Self::parse_ident)?;
                self.expect(&TokenKind::RParen, "\")\"")?;
                Some(cols)
            } else {
                None
            };
            let source = if self.consume_keyword("values") {
                let rows = self.parse_comma_separated(|p| {
                    p.expect(&TokenKind::LParen, "\"(\"")?;
                    let row = p.parse_comma_separated(Self::parse_expr)?;
                    p.expect(&TokenKind::RParen, "\")\"")?;
                    Ok(row)
                })?;
                InsertSource::Values(rows)
            } else if self.is_keyword("select") || self.is_keyword("with") {
                InsertSource::Query(Box::new(self.parse_query()?))
            } else {
                return Err(self.unexpected("VALUES or SELECT"));
            };
            return Ok(Statement::Insert { table, columns, source });
        }
        if self.consume_keyword("drop") {
            self.expect_keyword("table")?;
            let if_exists = if self.consume_keyword("if") {
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            let name = self.parse_ident()?;
            return Ok(Statement::DropTable { name, if_exists });
        }
        if self.consume_keyword("show") {
            self.expect_keyword("tables")?;
            return Ok(Statement::ShowTables);
        }
        if self.consume_keyword("describe") || self.consume_keyword("desc") {
            let name = self.parse_ident()?;
            return Ok(Statement::Describe { name });
        }
        Err(self.unexpected("a statement (SELECT, WITH, EXPLAIN, CREATE, INSERT, DROP, SHOW, DESCRIBE)"))
    }

    fn parse_comma_separated<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut out = vec![f(self)?];
        while self.consume(&TokenKind::Comma) {
            out.push(f(self)?);
        }
        Ok(out)
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        let t = self.peek().clone();
        let TokenKind::Word { value, .. } = &t.kind else {
            return Err(self.unexpected("a data type"));
        };
        self.next();
        let ty = match value.as_str() {
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "int8" | "int4" | "int2" | "long" => {
                DataType::Int64
            }
            "double" | "float" | "real" | "decimal" | "numeric" | "float8" | "float4" => {
                self.consume_keyword("precision");
                DataType::Float64
            }
            "varchar" | "text" | "string" | "char" | "character" => {
                self.consume_keyword("varying");
                DataType::Utf8
            }
            "date" => DataType::Date,
            "boolean" | "bool" => DataType::Boolean,
            other => {
                return Err(Error::new(ErrorKind::Parse, format!("unknown data type \"{other}\"")).with_span(t.span));
            }
        };
        // Optional precision/length arguments, e.g. DECIMAL(12, 2), VARCHAR(25).
        if self.consume(&TokenKind::LParen) {
            while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
                self.next();
            }
            self.expect(&TokenKind::RParen, "\")\"")?;
        }
        Ok(ty)
    }

    pub fn parse_query(&mut self) -> Result<Query> {
        let mut ctes = Vec::new();
        if self.consume_keyword("with") {
            if self.is_keyword("recursive") {
                return Err(
                    Error::new(ErrorKind::Unsupported, "WITH RECURSIVE is not supported").with_span(self.peek().span)
                );
            }
            ctes = self.parse_comma_separated(|p| {
                let name = p.parse_ident()?;
                p.expect_keyword("as")?;
                p.expect(&TokenKind::LParen, "\"(\"")?;
                let query = Box::new(p.parse_query()?);
                p.expect(&TokenKind::RParen, "\")\"")?;
                Ok(Cte { name, query })
            })?;
        }
        let body = Box::new(self.parse_select()?);
        if self.is_keyword("union") || self.is_keyword("except") || self.is_keyword("intersect") {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "set operations (UNION/EXCEPT/INTERSECT) are not supported",
            )
            .with_span(self.peek().span));
        }
        let mut order_by = Vec::new();
        if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            order_by = self.parse_comma_separated(|p| {
                let expr = p.parse_expr()?;
                let asc = if p.consume_keyword("desc") {
                    false
                } else {
                    p.consume_keyword("asc");
                    true
                };
                let nulls_first = if p.consume_keyword("nulls") {
                    if p.consume_keyword("first") {
                        Some(true)
                    } else {
                        p.expect_keyword("last")?;
                        Some(false)
                    }
                } else {
                    None
                };
                Ok(OrderByItem { expr, asc, nulls_first })
            })?;
        }
        let mut limit = None;
        let mut offset = None;
        loop {
            if limit.is_none() && self.consume_keyword("limit") {
                limit = Some(self.parse_expr()?);
            } else if offset.is_none() && self.consume_keyword("offset") {
                offset = Some(self.parse_expr()?);
            } else {
                break;
            }
        }
        Ok(Query {
            ctes,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_select(&mut self) -> Result<Select> {
        self.expect_keyword("select")?;
        let distinct = if self.consume_keyword("distinct") {
            true
        } else {
            self.consume_keyword("all");
            false
        };
        let projection = self.parse_comma_separated(Self::parse_select_item)?;
        let from = if self.consume_keyword("from") {
            Some(self.parse_table_refs()?)
        } else {
            None
        };
        let selection = if self.consume_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.consume_keyword("group") {
            self.expect_keyword("by")?;
            group_by = self.parse_comma_separated(Self::parse_expr)?;
        }
        let having = if self.consume_keyword("having") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Select {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
        })
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.peek_kind() == &TokenKind::Star {
            return Ok(SelectItem::Wildcard(self.next().span));
        }
        // t.*
        if matches!(self.peek_kind(), TokenKind::Word { .. })
            && self.peek_nth_kind(1) == &TokenKind::Dot
            && self.peek_nth_kind(2) == &TokenKind::Star
        {
            let q = self.parse_ident()?;
            self.next();
            self.next();
            return Ok(SelectItem::QualifiedWildcard(q));
        }
        let expr = self.parse_expr()?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn parse_table_refs(&mut self) -> Result<TableRef> {
        let mut left = self.parse_joined_table()?;
        while self.consume(&TokenKind::Comma) {
            let right = self.parse_joined_table()?;
            left = TableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind: JoinKind::Cross,
                constraint: JoinConstraint::None,
            };
        }
        Ok(left)
    }

    fn parse_joined_table(&mut self) -> Result<TableRef> {
        let mut left = self.parse_table_factor()?;
        loop {
            let kind = if self.is_keyword("join") || self.is_keyword("inner") {
                self.consume_keyword("inner");
                self.expect_keyword("join")?;
                JoinKind::Inner
            } else if self.is_keyword("left") {
                self.next();
                self.consume_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Left
            } else if self.is_keyword("cross") {
                self.next();
                self.expect_keyword("join")?;
                JoinKind::Cross
            } else if self.is_keyword("right") || self.is_keyword("full") || self.is_keyword("natural") {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "only INNER, LEFT and CROSS joins are supported (rewrite RIGHT JOIN as LEFT JOIN)",
                )
                .with_span(self.peek().span));
            } else {
                return Ok(left);
            };
            let right = self.parse_table_factor()?;
            let constraint = if kind == JoinKind::Cross {
                JoinConstraint::None
            } else if self.consume_keyword("on") {
                JoinConstraint::On(self.parse_expr()?)
            } else if self.consume_keyword("using") {
                self.expect(&TokenKind::LParen, "\"(\"")?;
                let cols = self.parse_comma_separated(Self::parse_ident)?;
                self.expect(&TokenKind::RParen, "\")\"")?;
                JoinConstraint::Using(cols)
            } else {
                return Err(self.unexpected("ON or USING"));
            };
            left = TableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                constraint,
            };
        }
    }

    fn parse_table_factor(&mut self) -> Result<TableRef> {
        if self.peek_kind() == &TokenKind::LParen {
            let span = self.next().span;
            if self.is_keyword("select") || self.is_keyword("with") {
                let query = Box::new(self.parse_query()?);
                self.expect(&TokenKind::RParen, "\")\"")?;
                let alias = self.parse_optional_alias()?;
                return Ok(TableRef::Subquery { query, alias, span });
            }
            let inner = self.parse_table_refs()?;
            self.expect(&TokenKind::RParen, "\")\"")?;
            return Ok(inner);
        }
        if let TokenKind::String(path) = self.peek_kind().clone() {
            let span = self.next().span;
            let alias = self.parse_optional_alias()?;
            return Ok(TableRef::File { path, span, alias });
        }
        let name = self.parse_ident()?;
        // read_csv('path') / read_parquet('path') table functions.
        if self.peek_kind() == &TokenKind::LParen && (name.value == "read_csv" || name.value == "read_parquet") {
            self.next();
            let tok = self.next();
            let TokenKind::String(path) = tok.kind else {
                return Err(
                    Error::new(ErrorKind::Parse, format!("{}() expects a file path string", name.value))
                        .with_span(tok.span),
                );
            };
            self.expect(&TokenKind::RParen, "\")\"")?;
            let alias = self.parse_optional_alias()?;
            return Ok(TableRef::File {
                path,
                span: tok.span,
                alias,
            });
        }
        let alias = self.parse_optional_alias()?;
        Ok(TableRef::Table { name, alias })
    }

    // ----- expressions ---------------------------------------------------

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_expr_bp(0)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr> {
        let mut left = self.parse_prefix()?;
        loop {
            let tok = self.peek().clone();
            let span = tok.span;
            let binop = match &tok.kind {
                TokenKind::Plus => Some(BinaryOperator::Plus),
                TokenKind::Minus => Some(BinaryOperator::Minus),
                TokenKind::Star => Some(BinaryOperator::Multiply),
                TokenKind::Slash => Some(BinaryOperator::Divide),
                TokenKind::Percent => Some(BinaryOperator::Modulo),
                TokenKind::Eq => Some(BinaryOperator::Eq),
                TokenKind::NotEq => Some(BinaryOperator::NotEq),
                TokenKind::Lt => Some(BinaryOperator::Lt),
                TokenKind::LtEq => Some(BinaryOperator::LtEq),
                TokenKind::Gt => Some(BinaryOperator::Gt),
                TokenKind::GtEq => Some(BinaryOperator::GtEq),
                TokenKind::Concat => Some(BinaryOperator::Concat),
                TokenKind::Word { value, quoted: false } if value == "and" => Some(BinaryOperator::And),
                TokenKind::Word { value, quoted: false } if value == "or" => Some(BinaryOperator::Or),
                _ => None,
            };
            if let Some(op) = binop {
                let bp = op.precedence();
                if bp <= min_bp {
                    break;
                }
                self.next();
                let right = self.parse_expr_bp(bp)?;
                left = Expr::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                    span,
                };
                continue;
            }
            if tok.kind == TokenKind::DoubleColon {
                if PREC_CAST <= min_bp {
                    break;
                }
                self.next();
                let data_type = self.parse_data_type()?;
                left = Expr::Cast {
                    expr: Box::new(left),
                    data_type,
                    span,
                };
                continue;
            }
            // Postfix-ish predicates at comparison precedence.
            if PREC_CMP <= min_bp {
                break;
            }
            if self.is_keyword("is") {
                self.next();
                let negated = self.consume_keyword("not");
                self.expect_keyword("null")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                continue;
            }
            let negated = self.is_keyword("not")
                && (self.is_keyword_at(1, "like")
                    || self.is_keyword_at(1, "ilike")
                    || self.is_keyword_at(1, "in")
                    || self.is_keyword_at(1, "between"));
            if negated {
                self.next();
            }
            if self.is_keyword("like") || self.is_keyword("ilike") {
                let case_insensitive = self.is_keyword("ilike");
                self.next();
                let pattern = self.parse_expr_bp(PREC_CMP)?;
                left = Expr::Like {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
                    negated,
                    case_insensitive,
                };
            } else if self.consume_keyword("in") {
                self.expect(&TokenKind::LParen, "\"(\"")?;
                if self.is_keyword("select") || self.is_keyword("with") {
                    return Err(Error::new(ErrorKind::Unsupported, "IN (subquery) is not supported")
                        .with_span(self.peek().span));
                }
                let list = self.parse_comma_separated(Self::parse_expr)?;
                self.expect(&TokenKind::RParen, "\",\" or \")\"")?;
                left = Expr::InList {
                    expr: Box::new(left),
                    list,
                    negated,
                };
            } else if self.consume_keyword("between") {
                let low = self.parse_expr_bp(PREC_CMP)?;
                self.expect_keyword("and")?;
                let high = self.parse_expr_bp(PREC_CMP)?;
                left = Expr::Between {
                    expr: Box::new(left),
                    negated,
                    low: Box::new(low),
                    high: Box::new(high),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        let tok = self.next();
        let span = tok.span;
        match tok.kind {
            TokenKind::Number(n) => {
                let lit = if n.contains(['.', 'e', 'E']) {
                    Literal::Float(
                        n.parse()
                            .map_err(|_| Error::new(ErrorKind::Parse, format!("invalid number {n}")).with_span(span))?,
                    )
                } else {
                    match n.parse::<i64>() {
                        Ok(v) => Literal::Integer(v),
                        Err(_) => Literal::Float(n.parse().unwrap_or(f64::INFINITY)),
                    }
                };
                Ok(Expr::Literal(lit, span))
            }
            TokenKind::String(s) => Ok(Expr::Literal(Literal::String(s), span)),
            TokenKind::LParen => {
                if self.is_keyword("select") || self.is_keyword("with") {
                    let q = self.parse_query()?;
                    self.expect(&TokenKind::RParen, "\")\"")?;
                    return Ok(Expr::Subquery(Box::new(q), span));
                }
                let e = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "\")\"")?;
                Ok(e)
            }
            TokenKind::Minus | TokenKind::Plus => {
                let op = if tok.kind == TokenKind::Minus {
                    UnaryOperator::Minus
                } else {
                    UnaryOperator::Plus
                };
                let expr = self.parse_expr_bp(PREC_UNARY)?;
                // Fold negative numeric literals directly.
                if op == UnaryOperator::Minus
                    && let Expr::Literal(lit, lspan) = &expr
                {
                    let merged = Span::new(span.line, span.col, span.offset, lspan.offset + lspan.len - span.offset);
                    match lit {
                        Literal::Integer(v) => return Ok(Expr::Literal(Literal::Integer(-v), merged)),
                        Literal::Float(v) => return Ok(Expr::Literal(Literal::Float(-v), merged)),
                        _ => {}
                    }
                }
                Ok(Expr::Unary {
                    op,
                    expr: Box::new(expr),
                    span,
                })
            }
            TokenKind::Word { value, quoted: false } => self.parse_word_prefix(value, span),
            TokenKind::Word { value, quoted: true } => self.parse_identifier_tail(Ident { value, span }),
            other => Err(Error::new(
                ErrorKind::Parse,
                format!("expected an expression, found {}", other.describe()),
            )
            .with_span(span)),
        }
    }

    fn parse_word_prefix(&mut self, value: String, span: Span) -> Result<Expr> {
        match value.as_str() {
            "null" => Ok(Expr::Literal(Literal::Null, span)),
            "true" => Ok(Expr::Literal(Literal::Boolean(true), span)),
            "false" => Ok(Expr::Literal(Literal::Boolean(false), span)),
            "not" => {
                let expr = self.parse_expr_bp(PREC_NOT)?;
                Ok(Expr::Unary {
                    op: UnaryOperator::Not,
                    expr: Box::new(expr),
                    span,
                })
            }
            "case" => self.parse_case(span),
            "cast" => {
                self.expect(&TokenKind::LParen, "\"(\"")?;
                let expr = self.parse_expr()?;
                self.expect_keyword("as")?;
                let data_type = self.parse_data_type()?;
                self.expect(&TokenKind::RParen, "\")\"")?;
                Ok(Expr::Cast {
                    expr: Box::new(expr),
                    data_type,
                    span,
                })
            }
            "date" if matches!(self.peek_kind(), TokenKind::String(_)) => {
                let TokenKind::String(s) = self.next().kind else {
                    unreachable!()
                };
                Ok(Expr::Literal(Literal::Date(s), span))
            }
            "interval" => self.parse_interval(span),
            "extract" if self.peek_kind() == &TokenKind::LParen => {
                self.next();
                let field = self.parse_ident()?;
                self.expect_keyword("from")?;
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "\")\"")?;
                Ok(Expr::Extract {
                    field,
                    expr: Box::new(expr),
                })
            }
            "exists" => Err(Error::new(ErrorKind::Unsupported, "EXISTS subqueries are not supported").with_span(span)),
            v if RESERVED.contains(&v) && self.peek_kind() != &TokenKind::LParen => {
                Err(Error::new(ErrorKind::Parse, format!("expected an expression, found \"{v}\"")).with_span(span))
            }
            _ => self.parse_identifier_tail(Ident { value, span }),
        }
    }

    /// After an identifier: function call, compound identifier or plain column.
    fn parse_identifier_tail(&mut self, first: Ident) -> Result<Expr> {
        if self.peek_kind() == &TokenKind::LParen {
            self.next();
            return self.parse_function_args(first);
        }
        let mut parts = vec![first];
        while self.peek_kind() == &TokenKind::Dot {
            self.next();
            // After a dot any word is a valid column name (e.g. `t.by`).
            match self.next() {
                Token {
                    kind: TokenKind::Word { value, .. },
                    span,
                } => parts.push(Ident { value, span }),
                t => {
                    return Err(Error::new(
                        ErrorKind::Parse,
                        format!("expected column name after \".\", found {}", t.kind.describe()),
                    )
                    .with_span(t.span));
                }
            }
        }
        if parts.len() == 1 {
            Ok(Expr::Identifier(parts.pop().unwrap()))
        } else {
            Ok(Expr::CompoundIdentifier(parts))
        }
    }

    fn parse_function_args(&mut self, name: Ident) -> Result<Expr> {
        if self.consume(&TokenKind::Star) {
            self.expect(&TokenKind::RParen, "\")\"")?;
            return Ok(Expr::Function {
                name,
                args: vec![],
                distinct: false,
                star: true,
            });
        }
        if self.consume(&TokenKind::RParen) {
            return Ok(Expr::Function {
                name,
                args: vec![],
                distinct: false,
                star: false,
            });
        }
        let distinct = self.consume_keyword("distinct");
        let mut args = vec![self.parse_expr()?];
        if (name.value == "substring" || name.value == "substr") && self.consume_keyword("from") {
            args.push(self.parse_expr()?);
            if self.consume_keyword("for") {
                args.push(self.parse_expr()?);
            }
        } else {
            while self.consume(&TokenKind::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(&TokenKind::RParen, "\",\" or \")\"")?;
        Ok(Expr::Function {
            name,
            args,
            distinct,
            star: false,
        })
    }

    fn parse_case(&mut self, span: Span) -> Result<Expr> {
        let operand = if self.is_keyword("when") {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut branches = Vec::new();
        while self.consume_keyword("when") {
            let cond = self.parse_expr()?;
            self.expect_keyword("then")?;
            let result = self.parse_expr()?;
            branches.push((cond, result));
        }
        if branches.is_empty() {
            return Err(self.unexpected("WHEN"));
        }
        let else_result = if self.consume_keyword("else") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword("end")?;
        Ok(Expr::Case {
            operand,
            branches,
            else_result,
            span,
        })
    }

    /// `INTERVAL '90' DAY`, `INTERVAL '1' YEAR`, `INTERVAL '3 months'`.
    fn parse_interval(&mut self, span: Span) -> Result<Expr> {
        let tok = self.next();
        let TokenKind::String(s) = tok.kind else {
            return Err(Error::new(
                ErrorKind::Parse,
                format!("expected interval string such as '90', found {}", tok.kind.describe()),
            )
            .with_span(tok.span));
        };
        let mut parts = s.split_whitespace();
        let bad = || Error::new(ErrorKind::Parse, format!("invalid interval '{s}'")).with_span(tok.span);
        let n: i32 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        let unit = match parts.next() {
            Some(u) => u.to_ascii_lowercase(),
            None => match self.peek_kind().clone() {
                TokenKind::Word { value, quoted: false } => {
                    self.next();
                    value
                }
                _ => return Err(self.unexpected("interval unit (YEAR, MONTH, DAY)")),
            },
        };
        let (months, days) = match unit.trim_end_matches('s') {
            "year" => (n * 12, 0),
            "month" => (n, 0),
            "week" => (0, n * 7),
            "day" => (0, n),
            other => {
                return Err(
                    Error::new(ErrorKind::Parse, format!("unsupported interval unit \"{other}\"")).with_span(tok.span),
                );
            }
        };
        Ok(Expr::Literal(Literal::Interval { months, days }, span))
    }
}
