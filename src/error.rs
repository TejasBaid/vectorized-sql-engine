//! Error type shared by every layer of the engine.
//!
//! Errors carry an optional [`Span`] pointing into the original SQL text so the
//! REPL can render a caret under the offending token.

use std::fmt;

/// A location in the SQL source text (1-based line/column, 0-based byte offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub offset: usize,
    pub len: usize,
}

impl Span {
    pub fn new(line: u32, col: u32, offset: usize, len: usize) -> Self {
        Span { line, col, offset, len }
    }
}

/// Which stage of query processing produced the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Lex,
    Parse,
    Bind,
    Type,
    Plan,
    Execution,
    Io,
    Unsupported,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ErrorKind::Lex => "Lexer",
            ErrorKind::Parse => "Parser",
            ErrorKind::Bind => "Binder",
            ErrorKind::Type => "Type",
            ErrorKind::Plan => "Planner",
            ErrorKind::Execution => "Execution",
            ErrorKind::Io => "IO",
            ErrorKind::Unsupported => "Not supported",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub span: Option<Span>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
            span: None,
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    /// Render the error together with the offending source line and a caret, e.g.
    ///
    /// ```text
    /// Binder error: column "nope" not found (line 1, column 8)
    ///   SELECT nope FROM t
    ///          ^^^^
    /// ```
    pub fn render(&self, sql: &str) -> String {
        let mut out = self.to_string();
        if let Some(span) = self.span
            && let Some(line) = sql.lines().nth(span.line.saturating_sub(1) as usize)
        {
            let col = span.col.saturating_sub(1) as usize;
            let width = span.len.max(1);
            out.push_str(&format!(
                "\n  {}\n  {}{}",
                line,
                " ".repeat(col.min(line.len())),
                "^".repeat(width)
            ));
        }
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} error: {}", self.kind, self.message)?;
        if let Some(span) = self.span {
            write!(f, " (line {}, column {})", span.line, span.col)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(ErrorKind::Io, e.to_string())
    }
}

impl From<parquet::errors::ParquetError> for Error {
    fn from(e: parquet::errors::ParquetError) -> Self {
        Error::new(ErrorKind::Io, format!("parquet: {e}"))
    }
}

/// Build an `Err(Error)` of the given kind with `format!`-style arguments.
#[macro_export]
macro_rules! err {
    ($kind:ident, $($arg:tt)*) => {
        Err($crate::error::Error::new($crate::error::ErrorKind::$kind, format!($($arg)*)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_points_at_span() {
        let e = Error::new(ErrorKind::Bind, "column \"nope\" not found").with_span(Span::new(1, 8, 7, 4));
        let r = e.render("SELECT nope FROM t");
        assert!(r.contains("line 1, column 8"));
        assert!(r.ends_with("  SELECT nope FROM t\n         ^^^^"));
    }
}
