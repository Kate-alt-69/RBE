//! Recursive-descent parser over the token stream from `lexer`. Grammar
//! (informal — matches the crate root doc comment):
//!
//! ```text
//! file       := import_line* class_decl
//! import_line:= ":" "import" "[" (IDENT | STRING) "]"
//! class_decl := "class" IDENT "{" method* "}"
//! method     := ["async"] IDENT "(" [IDENT] ")" "{" statement* "}"
//! statement  := "const" IDENT "=" expr ";"
//!             | "return" expr ";"
//!             | expr ";"
//! expr       := object | array | call | member | literal
//! object     := "{" [IDENT ":" expr ("," IDENT ":" expr)*] "}"
//! array      := "[" [expr ("," expr)*] "]"
//! ```
//!
//! `call` and `member` share a left-recursive-looking shape
//! (`a.b.c(x)`), handled here by parsing a primary identifier then
//! repeatedly consuming `.ident` / `(args)` suffixes.

use crate::ast::{Expr, ImportTarget, MethodDef, RouteFile, Statement};
use crate::lexer::Token;

const KNOWN_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse_file(mut self) -> Result<RouteFile, ParseError> {
        let mut imports = Vec::new();
        while self.check(&Token::Colon) {
            imports.push(self.parse_import_line()?);
        }

        let class = self.parse_class()?;

        Ok(RouteFile {
            imports,
            class_name: class.0,
            methods: class.1,
        })
    }

    fn parse_import_line(&mut self) -> Result<ImportTarget, ParseError> {
        self.expect(Token::Colon)?;
        self.expect(Token::Import)?;
        self.expect(Token::LBracket)?;

        let target = match self.advance() {
            Token::Ident(name) => Self::classify_bareword_import(&name)?,
            Token::String(path) => ImportTarget::Custom(path),
            other => {
                return Err(ParseError {
                    message: format!(
                        "expected an identifier (builtin) or string (custom path) inside :import[...], got {other:?}"
                    ),
                })
            }
        };

        self.expect(Token::RBracket)?;
        Ok(target)
    }

    /// A bareword import target is either a true builtin (`net`,
    /// `env`, ...) or the `module&name` shorthand for "look up `name`
    /// in the default `/module/` folder — a sibling of the compiled
    /// binary, not the current working directory (see
    /// `crate::paths::binary_dir`)." Desugars directly to the same
    /// `ImportTarget::Custom` a string-path import produces, so
    /// everything downstream (resolution, caching, the "not
    /// implemented yet" error) is one code path regardless of which
    /// syntax was used to write it.
    fn classify_bareword_import(name: &str) -> Result<ImportTarget, ParseError> {
        if let Some((prefix, module_name)) = name.split_once('&') {
            if prefix != "module" {
                return Err(ParseError {
                    message: format!(
                        "unknown import shorthand {prefix:?} in {name:?} — only \"module&name\" \
                         is a recognized shorthand (resolves to the default /module/ folder \
                         next to the compiled binary)"
                    ),
                });
            }
            if module_name.is_empty() {
                return Err(ParseError {
                    message: format!("{name:?} is missing a module name after \"module&\""),
                });
            }
            return Ok(ImportTarget::Custom(format!("./module/{module_name}")));
        }

        Ok(ImportTarget::Builtin(name.to_string()))
    }

    fn parse_class(&mut self) -> Result<(String, Vec<MethodDef>), ParseError> {
        self.expect(Token::Class)?;
        let name = self.expect_ident()?;
        self.expect(Token::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&Token::RBrace) {
            methods.push(self.parse_method()?);
        }
        self.expect(Token::RBrace)?;

        Ok((name, methods))
    }

    fn parse_method(&mut self) -> Result<MethodDef, ParseError> {
        // `async` is accepted and ignored in v1 — every route method is
        // effectively async already (the whole request pipeline is),
        // so the keyword doesn't change evaluation here. Kept in the
        // grammar purely so real-looking JS class syntax parses.
        if self.check(&Token::Async) {
            self.advance();
        }

        let name = self.expect_ident()?;
        let verb = name.to_lowercase();
        if !KNOWN_VERBS.contains(&verb.as_str()) {
            return Err(ParseError {
                message: format!(
                    "method name {name:?} is not a known HTTP verb (expected one of {KNOWN_VERBS:?}) — v1 .route files only support one method per verb"
                ),
            });
        }

        self.expect(Token::LParen)?;
        let param_name = if !self.check(&Token::RParen) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(Token::RParen)?;

        self.expect(Token::LBrace)?;
        let mut body = Vec::new();
        while !self.check(&Token::RBrace) {
            body.push(self.parse_statement()?);
        }
        self.expect(Token::RBrace)?;

        Ok(MethodDef {
            verb,
            param_name,
            body,
        })
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        if self.check(&Token::Const) {
            self.advance();
            let name = self.expect_ident()?;
            self.expect(Token::Eq)?;
            let value = self.parse_expr()?;
            self.expect(Token::Semicolon)?;
            return Ok(Statement::Const { name, value });
        }

        if self.check(&Token::Return) {
            self.advance();
            let value = self.parse_expr()?;
            self.expect(Token::Semicolon)?;
            return Ok(Statement::Return(value));
        }

        let expr = self.parse_expr()?;
        self.expect(Token::Semicolon)?;
        Ok(Statement::Expr(expr))
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;

        loop {
            if self.check(&Token::Dot) {
                self.advance();
                let field = self.expect_ident()?;
                expr = Expr::Member(Box::new(expr), field);
            } else if self.check(&Token::LParen) {
                self.advance();
                let mut args = Vec::new();
                if !self.check(&Token::RParen) {
                    args.push(self.parse_expr()?);
                    while self.check(&Token::Comma) {
                        self.advance();
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(Token::RParen)?;
                expr = Expr::Call(Box::new(expr), args);
            } else {
                break;
            }
        }

        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        match self.advance() {
            Token::String(s) => Ok(Expr::String(s)),
            Token::Number(n) => Ok(Expr::Number(n)),
            Token::True => Ok(Expr::Bool(true)),
            Token::False => Ok(Expr::Bool(false)),
            Token::Null => Ok(Expr::Null),
            Token::Ident(name) => Ok(Expr::Ident(name)),
            Token::LBrace => self.parse_object_tail(),
            Token::LBracket => self.parse_array_tail(),
            other => Err(ParseError {
                message: format!("unexpected token in expression: {other:?}"),
            }),
        }
    }

    fn parse_object_tail(&mut self) -> Result<Expr, ParseError> {
        let mut fields = Vec::new();
        if !self.check(&Token::RBrace) {
            fields.push(self.parse_object_field()?);
            while self.check(&Token::Comma) {
                self.advance();
                if self.check(&Token::RBrace) {
                    break; // trailing comma
                }
                fields.push(self.parse_object_field()?);
            }
        }
        self.expect(Token::RBrace)?;
        Ok(Expr::Object(fields))
    }

    fn parse_object_field(&mut self) -> Result<(String, Expr), ParseError> {
        let key = self.expect_ident()?;
        self.expect(Token::Colon)?;
        let value = self.parse_expr()?;
        Ok((key, value))
    }

    fn parse_array_tail(&mut self) -> Result<Expr, ParseError> {
        let mut items = Vec::new();
        if !self.check(&Token::RBracket) {
            items.push(self.parse_expr()?);
            while self.check(&Token::Comma) {
                self.advance();
                if self.check(&Token::RBracket) {
                    break;
                }
                items.push(self.parse_expr()?);
            }
        }
        self.expect(Token::RBracket)?;
        Ok(Expr::Array(items))
    }

    // --- token stream helpers ---

    fn check(&self, expected: &Token) -> bool {
        matches!(self.tokens.get(self.pos), Some(t) if std::mem::discriminant(t) == std::mem::discriminant(expected))
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: Token) -> Result<(), ParseError> {
        if self.check(&expected) {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                message: format!("expected {expected:?}, got {:?}", self.tokens.get(self.pos)),
            })
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Token::Ident(name) => Ok(name),
            other => Err(ParseError {
                message: format!("expected identifier, got {other:?}"),
            }),
        }
    }
}
