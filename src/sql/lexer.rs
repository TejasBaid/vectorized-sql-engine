//! Hand-written SQL tokenizer. Every token records its source [`Span`].

use crate::error::{Error, ErrorKind, Result, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Identifier or keyword. `quoted` identifiers are never keywords and keep
    /// their case; unquoted ones are case-insensitive.
    Word {
        value: String,
        quoted: bool,
    },
    Number(String),
    String(String),
    Comma,
    Dot,
    Semicolon,
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    DoubleColon,
    Eof,
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Word { value, .. } => format!("\"{value}\""),
            TokenKind::Number(n) => format!("number {n}"),
            TokenKind::String(s) => format!("string '{s}'"),
            TokenKind::Eof => "end of input".to_string(),
            other => format!("\"{}\"", other.symbol()),
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            TokenKind::Comma => ",",
            TokenKind::Dot => ".",
            TokenKind::Semicolon => ";",
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Eq => "=",
            TokenKind::NotEq => "<>",
            TokenKind::Lt => "<",
            TokenKind::LtEq => "<=",
            TokenKind::Gt => ">",
            TokenKind::GtEq => ">=",
            TokenKind::Concat => "||",
            TokenKind::DoubleColon => "::",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    line_start: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            line_start: 0,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments()?;
            let start = self.pos;
            let (line, col) = (self.line, (start - self.line_start) as u32 + 1);
            let Some(&c) = self.bytes.get(self.pos) else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(line, col, start, 0),
                });
                return Ok(tokens);
            };
            let kind = match c {
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                    while self
                        .peek()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
                    {
                        self.pos += 1;
                    }
                    TokenKind::Word {
                        value: self.src[start..self.pos].to_ascii_lowercase(),
                        quoted: false,
                    }
                }
                b'"' => {
                    self.pos += 1;
                    let value = self.read_quoted(b'"', start)?;
                    TokenKind::Word { value, quoted: true }
                }
                b'\'' => {
                    self.pos += 1;
                    TokenKind::String(self.read_quoted(b'\'', start)?)
                }
                b'0'..=b'9' => self.read_number(),
                b'.' if self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit) => self.read_number(),
                _ => {
                    self.pos += 1;
                    let next = self.peek();
                    let two = |s: &mut Self, k| {
                        s.pos += 1;
                        k
                    };
                    match (c, next) {
                        (b',', _) => TokenKind::Comma,
                        (b'.', _) => TokenKind::Dot,
                        (b';', _) => TokenKind::Semicolon,
                        (b'(', _) => TokenKind::LParen,
                        (b')', _) => TokenKind::RParen,
                        (b'+', _) => TokenKind::Plus,
                        (b'-', _) => TokenKind::Minus,
                        (b'*', _) => TokenKind::Star,
                        (b'/', _) => TokenKind::Slash,
                        (b'%', _) => TokenKind::Percent,
                        (b'=', Some(b'=')) => two(&mut self, TokenKind::Eq),
                        (b'=', _) => TokenKind::Eq,
                        (b'<', Some(b'=')) => two(&mut self, TokenKind::LtEq),
                        (b'<', Some(b'>')) => two(&mut self, TokenKind::NotEq),
                        (b'<', _) => TokenKind::Lt,
                        (b'>', Some(b'=')) => two(&mut self, TokenKind::GtEq),
                        (b'>', _) => TokenKind::Gt,
                        (b'!', Some(b'=')) => two(&mut self, TokenKind::NotEq),
                        (b'|', Some(b'|')) => two(&mut self, TokenKind::Concat),
                        (b':', Some(b':')) => two(&mut self, TokenKind::DoubleColon),
                        _ => {
                            let ch = self.src[start..].chars().next().unwrap_or('?');
                            return Err(Error::new(ErrorKind::Lex, format!("unexpected character '{ch}'"))
                                .with_span(Span::new(line, col, start, ch.len_utf8())));
                        }
                    }
                }
            };
            tokens.push(Token {
                kind,
                span: Span::new(line, col, start, self.pos - start),
            });
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn newline(&mut self) {
        self.line += 1;
        self.line_start = self.pos + 1;
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b'\n') => {
                    self.newline();
                    self.pos += 1;
                }
                Some(c) if c.is_ascii_whitespace() => self.pos += 1,
                Some(b'-') if self.bytes.get(self.pos + 1) == Some(&b'-') => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.bytes.get(self.pos + 1) == Some(&b'*') => {
                    let start = self.pos;
                    let (line, col) = (self.line, (start - self.line_start) as u32 + 1);
                    self.pos += 2;
                    loop {
                        match self.peek() {
                            None => {
                                return Err(Error::new(ErrorKind::Lex, "unterminated block comment")
                                    .with_span(Span::new(line, col, start, 2)));
                            }
                            Some(b'*') if self.bytes.get(self.pos + 1) == Some(&b'/') => {
                                self.pos += 2;
                                break;
                            }
                            Some(b'\n') => {
                                self.newline();
                                self.pos += 1;
                            }
                            _ => self.pos += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// Read a quoted string/identifier; doubled quote characters are escapes.
    fn read_quoted(&mut self, quote: u8, start: usize) -> Result<String> {
        let (line, col) = (self.line, (start - self.line_start) as u32 + 1);
        let mut out = String::new();
        let mut seg_start = self.pos;
        loop {
            match self.peek() {
                None => {
                    let what = if quote == b'\'' {
                        "string literal"
                    } else {
                        "quoted identifier"
                    };
                    return Err(Error::new(ErrorKind::Lex, format!("unterminated {what}"))
                        .with_span(Span::new(line, col, start, 1)));
                }
                Some(c) if c == quote => {
                    out.push_str(&self.src[seg_start..self.pos]);
                    self.pos += 1;
                    if self.peek() == Some(quote) {
                        out.push(quote as char);
                        self.pos += 1;
                        seg_start = self.pos;
                    } else {
                        return Ok(out);
                    }
                }
                Some(b'\n') => {
                    self.newline();
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }
    }

    fn read_number(&mut self) -> TokenKind {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }
        TokenKind::Number(self.src[start..self.pos].to_string())
    }
}

pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    Lexer::new(sql).tokenize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn word(s: &str) -> TokenKind {
        TokenKind::Word {
            value: s.into(),
            quoted: false,
        }
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            kinds("SELECT a, 1.5e3 FROM t WHERE x <> 'it''s' -- comment\n"),
            vec![
                word("select"),
                word("a"),
                TokenKind::Comma,
                TokenKind::Number("1.5e3".into()),
                word("from"),
                word("t"),
                word("where"),
                word("x"),
                TokenKind::NotEq,
                TokenKind::String("it's".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn operators_and_quoted_identifiers() {
        assert_eq!(
            kinds("\"MyCol\" >= 2 || x::int /* c */ != .5"),
            vec![
                TokenKind::Word {
                    value: "MyCol".into(),
                    quoted: true
                },
                TokenKind::GtEq,
                TokenKind::Number("2".into()),
                TokenKind::Concat,
                word("x"),
                TokenKind::DoubleColon,
                word("int"),
                TokenKind::NotEq,
                TokenKind::Number(".5".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn spans_track_lines() {
        let toks = tokenize("SELECT\n  foo").unwrap();
        assert_eq!(toks[1].span, Span::new(2, 3, 9, 3));
    }

    #[test]
    fn errors_have_positions() {
        let e = tokenize("SELECT 'abc").unwrap_err();
        assert_eq!(e.span.unwrap().col, 8);
        assert!(e.message.contains("unterminated string"));
        let e = tokenize("SELECT #").unwrap_err();
        assert!(e.message.contains("unexpected character '#'"));
    }
}
