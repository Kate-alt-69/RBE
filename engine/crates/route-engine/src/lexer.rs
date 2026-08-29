//! Hand-written lexer for the JavaScript-shaped `.route` language.
//! Every token carries precise source information so compiler diagnostics can
//! point at the exact source range.

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Colon, LBracket, RBracket, LBrace, RBrace, LParen, RParen,
    Comma, Dot, Semicolon,
    Eq, EqEq, EqEqEq, Not, NotEq, NotEqEq,
    Lt, LtEq, Gt, GtEq,
    AndAnd, OrOr, Plus, Minus, Star, Slash, Percent,
    Dollar,
    Import, Class, Async, Const, Let, Return, Function, If, Else,
    True, False, Null,
    Ident(String), String(String), Number(f64),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
    column: usize,
    offset: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().peekable(),
            line: 1,
            column: 1,
            offset: 0,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();

        loop {
            self.skip_ws_comments();
            let line = self.line;
            let column = self.column;
            let start = self.offset;
            let Some(&c) = self.chars.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    line,
                    column,
                    end_line: line,
                    end_column: column,
                    start,
                    end: start,
                });
                return Ok(tokens);
            };

            let kind = match c {
                ':' => { self.bump(); TokenKind::Colon }
                '[' => { self.bump(); TokenKind::LBracket }
                ']' => { self.bump(); TokenKind::RBracket }
                '{' => { self.bump(); TokenKind::LBrace }
                '}' => { self.bump(); TokenKind::RBrace }
                '(' => { self.bump(); TokenKind::LParen }
                ')' => { self.bump(); TokenKind::RParen }
                ',' => { self.bump(); TokenKind::Comma }
                '.' => { self.bump(); TokenKind::Dot }
                ';' => { self.bump(); TokenKind::Semicolon }
                '$' => { self.bump(); TokenKind::Dollar }
                '!' => {
                    self.bump();
                    if self.take_if('=') {
                        if self.take_if('=') { TokenKind::NotEqEq } else { TokenKind::NotEq }
                    } else { TokenKind::Not }
                }
                '=' => {
                    self.bump();
                    if self.take_if('=') {
                        if self.take_if('=') { TokenKind::EqEqEq } else { TokenKind::EqEq }
                    } else { TokenKind::Eq }
                }
                '<' => {
                    self.bump();
                    if self.take_if('=') { TokenKind::LtEq } else { TokenKind::Lt }
                }
                '>' => {
                    self.bump();
                    if self.take_if('=') { TokenKind::GtEq } else { TokenKind::Gt }
                }
                '&' => {
                    self.bump();
                    if !self.take_if('&') {
                        return Err(LexError { message: "single '&' is not supported; use '&&'".into(), line, column });
                    }
                    TokenKind::AndAnd
                }
                '|' => {
                    self.bump();
                    if !self.take_if('|') {
                        return Err(LexError { message: "single '|' is not supported; use '||'".into(), line, column });
                    }
                    TokenKind::OrOr
                }
                '+' => { self.bump(); TokenKind::Plus }
                '-' => { self.bump(); TokenKind::Minus }
                '*' => { self.bump(); TokenKind::Star }
                '/' => { self.bump(); TokenKind::Slash }
                '%' => { self.bump(); TokenKind::Percent }
                '"' | '\'' => self.read_string(c, line, column)?,
                c if c.is_ascii_digit() => self.read_number(line, column)?,
                c if c.is_alphabetic() || c == '_' => self.read_ident_or_keyword(),
                other => {
                    return Err(LexError {
                        message: format!("unexpected character {other:?}"),
                        line, column,
                    });
                }
            };

            let end_line = self.line;
            let end_column = self.column;
            let end = self.offset;
            tokens.push(Token { kind, line, column, end_line, end_column, start, end });
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        self.offset += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn take_if(&mut self, expected: char) -> bool {
        if self.chars.peek() == Some(&expected) { self.bump(); true } else { false }
    }

    fn skip_ws_comments(&mut self) {
        loop {
            match self.chars.peek() {
                Some(c) if c.is_whitespace() => { self.bump(); }
                Some('/') => {
                    let mut clone = self.chars.clone();
                    clone.next();
                    if clone.peek() == Some(&'/') {
                        while let Some(c) = self.bump() { if c == '\n' { break; } }
                    } else { break; }
                }
                _ => break,
            }
        }
    }

    fn read_string(&mut self, quote: char, line: usize, column: usize) -> Result<TokenKind, LexError> {
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                Some(c) if c == quote => return Ok(TokenKind::String(out)),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(other) => out.push(other),
                    None => return Err(LexError { message: "unterminated escape in string".into(), line: self.line, column: self.column }),
                },
                Some(c) => out.push(c),
                None => return Err(LexError { message: "unterminated string literal".into(), line, column }),
            }
        }
    }

    fn read_number(&mut self, line: usize, column: usize) -> Result<TokenKind, LexError> {
        let mut out = String::new();
        let mut seen_dot = false;
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                out.push(c);
                self.bump();
            } else if c == '.' {
                if seen_dot {
                    return Err(LexError {
                        message: format!("malformed numeric literal {out}."),
                        line,
                        column,
                    });
                }
                seen_dot = true;
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
        let value = out.parse::<f64>().map_err(|_| LexError {
            message: format!("malformed numeric literal {out}"),
            line,
            column,
        })?;
        Ok(TokenKind::Number(value))
    }

    fn read_ident_or_keyword(&mut self) -> TokenKind {
        let mut out = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '&' {
                out.push(c); self.bump();
            } else { break; }
        }
        match out.as_str() {
            "import" => TokenKind::Import,
            "class" => TokenKind::Class,
            "async" => TokenKind::Async,
            "const" => TokenKind::Const,
            "let" => TokenKind::Let,
            "return" => TokenKind::Return,
            "function" => TokenKind::Function,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            _ => TokenKind::Ident(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_byte_and_line_spans() {
        let tokens = Lexer::new("const value = 1;\nreturn value;").tokenize().unwrap();
        let const_token = &tokens[0];
        assert_eq!(const_token.line, 1);
        assert_eq!(const_token.column, 1);
        assert_eq!(const_token.end_line, 1);
        assert_eq!(const_token.end_column, 6);
        assert_eq!(const_token.start, 0);
        assert_eq!(const_token.end, 5);

        let return_token = &tokens[5];
        assert_eq!(return_token.line, 2);
        assert_eq!(return_token.column, 1);
        assert_eq!(return_token.start, "const value = 1;\n".len());
    }

    #[test]
    fn tracks_multibyte_offsets_as_bytes() {
        let tokens = Lexer::new("const café = 1;").tokenize().unwrap();
        let ident = tokens.iter().find(|token| matches!(&token.kind, TokenKind::Ident(name) if name == "café")).unwrap();
        assert_eq!(&"const café = 1;"[ident.start..ident.end], "café");
    }

    #[test]
    fn rejects_multiple_decimal_points() {
        let error = Lexer::new("const value = 1.2.3;").tokenize().unwrap_err();
        assert!(error.message.contains("malformed numeric literal"));
    }
}
