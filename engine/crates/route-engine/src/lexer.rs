//! Hand-written tokenizer. Small grammar, small lexer — no reason to
//! pull in a lexer-generator crate for this.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Punctuation / structure
    Colon,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Dot,
    Semicolon,
    Eq,

    // Keywords
    Import,
    Class,
    Async,
    Const,
    Return,
    True,
    False,
    Null,

    // Literals / identifiers
    Ident(String),
    String(String),
    Number(f64),

    Eof,
}

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub line: usize,
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().peekable(),
            line: 1,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let Some(&c) = self.chars.peek() else {
                tokens.push(Token::Eof);
                break;
            };

            let token = match c {
                ':' => {
                    self.chars.next();
                    Token::Colon
                }
                '[' => {
                    self.chars.next();
                    Token::LBracket
                }
                ']' => {
                    self.chars.next();
                    Token::RBracket
                }
                '{' => {
                    self.chars.next();
                    Token::LBrace
                }
                '}' => {
                    self.chars.next();
                    Token::RBrace
                }
                '(' => {
                    self.chars.next();
                    Token::LParen
                }
                ')' => {
                    self.chars.next();
                    Token::RParen
                }
                ',' => {
                    self.chars.next();
                    Token::Comma
                }
                '.' => {
                    self.chars.next();
                    Token::Dot
                }
                ';' => {
                    self.chars.next();
                    Token::Semicolon
                }
                '=' => {
                    self.chars.next();
                    Token::Eq
                }
                '"' | '\'' => self.read_string(c)?,
                c if c.is_ascii_digit() => self.read_number(),
                c if c.is_alphabetic() || c == '_' || c == '$' => self.read_ident_or_keyword(),
                other => {
                    return Err(LexError {
                        message: format!("unexpected character {other:?}"),
                        line: self.line,
                    })
                }
            };

            tokens.push(token);
        }
        Ok(tokens)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.chars.peek() {
                Some('\n') => {
                    self.line += 1;
                    self.chars.next();
                }
                Some(c) if c.is_whitespace() => {
                    self.chars.next();
                }
                Some('/') => {
                    // Peek ahead for a `//` line comment without
                    // consuming a lone `/` that isn't one.
                    let mut clone = self.chars.clone();
                    clone.next();
                    if clone.peek() == Some(&'/') {
                        for c in self.chars.by_ref() {
                            if c == '\n' {
                                self.line += 1;
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
    }

    fn read_string(&mut self, quote: char) -> Result<Token, LexError> {
        self.chars.next(); // consume opening quote
        let mut out = String::new();
        loop {
            match self.chars.next() {
                Some(c) if c == quote => break,
                Some('\\') => {
                    // Minimal escape support: \" \' \\ \n \t
                    match self.chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some(other) => out.push(other),
                        None => {
                            return Err(LexError {
                                message: "unterminated escape in string".to_string(),
                                line: self.line,
                            })
                        }
                    }
                }
                Some(c) => out.push(c),
                None => {
                    return Err(LexError {
                        message: "unterminated string literal".to_string(),
                        line: self.line,
                    })
                }
            }
        }
        Ok(Token::String(out))
    }

    fn read_number(&mut self) -> Token {
        let mut out = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() || c == '.' {
                out.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        // Malformed numbers (e.g. "1.2.3") intentionally fall back to
        // 0.0 rather than panicking — a real diagnostic here is a
        // fast-follow, not core to proving the grammar out.
        Token::Number(out.parse().unwrap_or(0.0))
    }

    fn read_ident_or_keyword(&mut self) -> Token {
        let mut out = String::new();
        while let Some(&c) = self.chars.peek() {
            // `-` and `&` are allowed as identifier-continuation
            // characters (not as a leading character — this fn is only
            // entered on an alphabetic/`_`/`$` first char) specifically
            // so the `:import[module&setting-menu]` shorthand (§ see
            // parser.rs / api/README.md's ".module" section) lexes as
            // one token. Safe to allow everywhere, not just inside
            // import brackets: this grammar has no subtraction or
            // bitwise-AND operator to collide with.
            if c.is_alphanumeric() || c == '_' || c == '$' || c == '-' || c == '&' {
                out.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        match out.as_str() {
            "import" => Token::Import,
            "class" => Token::Class,
            "async" => Token::Async,
            "const" => Token::Const,
            "return" => Token::Return,
            "true" => Token::True,
            "false" => Token::False,
            "null" => Token::Null,
            _ => Token::Ident(out),
        }
    }
}
