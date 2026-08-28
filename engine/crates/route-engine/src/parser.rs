//! Recursive-descent / precedence parser for the RBE `.route` language.
//! The parser is intentionally strict and reports line/column information.

use crate::ast::{
    BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, Statement,
};
use crate::lexer::{Token, TokenKind};

const KNOWN_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse_file(self) -> Result<RouteFile, ParseError> {
        let (file, errors) = self.parse_file_collecting();
        if let Some(error) = errors.into_iter().next() {
            return Err(error);
        }
        file.ok_or_else(|| ParseError {
            message: "route file did not produce an AST".to_string(),
            line: 0,
            column: 0,
        })
    }

    pub fn parse_module_file(mut self) -> Result<ModuleFile, ParseError> {
        let mut imports = Vec::new();
        while self.check(&TokenKind::Colon) {
            imports.extend(self.parse_imports()?);
        }

        let mut functions = Vec::new();
        let mut exports = Vec::new();
        while !self.check(&TokenKind::Eof) {
            let exported = self.is_export_keyword();
            if exported {
                self.advance();
            }
            if self.check(&TokenKind::Async) {
                self.advance();
            }
            if !self.check(&TokenKind::Function) {
                return Err(
                    self.error_here("expected `function` or `export function` in .module file")
                );
            }
            let function = self.parse_function()?;
            if exported {
                exports.push(function.name.clone());
            }
            functions.push(function);
        }

        Ok(ModuleFile {
            imports,
            functions,
            exports,
        })
    }

    /// Parse a route while retaining recoverable statement errors. A valid
    /// AST is returned when the file structure can still be reconstructed;
    /// callers can render every recovered diagnostic instead of stopping at
    /// the first broken statement.
    pub fn parse_file_collecting(mut self) -> (Option<RouteFile>, Vec<ParseError>) {
        let mut errors = Vec::new();
        let mut imports = Vec::new();
        let mut functions = Vec::new();

        while self.check(&TokenKind::Colon) {
            match self.parse_imports() {
                Ok(entries) => imports.extend(entries),
                Err(error) => {
                    errors.push(error);
                    self.recover_top_level();
                }
            }
        }

        while self.check(&TokenKind::Function) {
            match self.parse_function_collecting(&mut errors) {
                Some(function) => functions.push(function),
                None => self.recover_top_level(),
            }
        }

        let class_result = self.parse_class_collecting(&mut errors);
        let Some((class_name, methods)) = class_result else {
            return (None, errors);
        };

        if !self.check(&TokenKind::Eof) {
            errors.push(self.error_here("unexpected content after Route class"));
        }

        (
            Some(RouteFile {
                imports,
                functions,
                class_name,
                methods,
            }),
            errors,
        )
    }

    fn parse_imports(&mut self) -> Result<Vec<ImportTarget>, ParseError> {
        self.expect(TokenKind::Colon)?;
        self.expect(TokenKind::Import)?;
        self.expect(TokenKind::LBracket)?;

        let mut entries = Vec::new();
        loop {
            if self.check(&TokenKind::RBracket) {
                if entries.is_empty() {
                    return Err(
                        self.error_here("expected at least one import entry inside :import[...]")
                    );
                }
                self.advance();
                break;
            }

            let target = self.parse_import_target()?;
            let target = if self.is_as_keyword() {
                self.advance();
                let alias = self.expect_ident()?;
                ImportTarget::Aliased {
                    target: Box::new(target),
                    alias,
                }
            } else {
                target
            };
            entries.push(target);

            if self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    return Err(self.error_here("trailing commas are not allowed in :import[...]"));
                }
                continue;
            }

            if !self.check(&TokenKind::RBracket) {
                return Err(self.error_here("expected `,` between import entries"));
            }
        }
        Ok(entries)
    }

    fn parse_import_target(&mut self) -> Result<ImportTarget, ParseError> {
        match self.advance().kind {
            TokenKind::Ident(name) => {
                if self.check(&TokenKind::Dot) {
                    self.advance();
                    let function = self.expect_ident()?;
                    Ok(ImportTarget::BuiltinFunction {
                        module: name,
                        function,
                    })
                } else if let Some((prefix, module_name)) = name.split_once('&') {
                    if prefix != "module" {
                        return Err(self.error_here("only module&name shorthand is supported"));
                    }
                    Ok(ImportTarget::Custom(format!("./module/{module_name}")))
                } else {
                    Ok(ImportTarget::Builtin(name))
                }
            }
            TokenKind::String(path) => {
                if self.check(&TokenKind::RBracket)
                    || self.check(&TokenKind::Comma)
                    || self.is_as_keyword()
                {
                    Ok(ImportTarget::Custom(path))
                } else {
                    self.expect(TokenKind::Dot)?;
                    let function = self.expect_ident()?;
                    Ok(ImportTarget::CustomFunction { path, function })
                }
            }
            other => Err(self.error_here(&format!(
                "expected builtin identifier or string path inside :import[...], got {other:?}"
            ))),
        }
    }

    fn is_export_keyword(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(TokenKind::Ident(name)) if name == "export"
        )
    }

    fn is_as_keyword(&self) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(TokenKind::Ident(name)) if name == "as")
    }

    fn parse_function(&mut self) -> Result<FunctionDef, ParseError> {
        self.expect(TokenKind::Function)?;
        let name = self.expect_ident()?;
        let params = self.parse_params()?;
        let body = self.parse_block()?;
        Ok(FunctionDef { name, params, body })
    }

    fn parse_function_collecting(&mut self, errors: &mut Vec<ParseError>) -> Option<FunctionDef> {
        if let Err(error) = self.expect(TokenKind::Function) {
            errors.push(error);
            return None;
        }
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(error) => {
                errors.push(error);
                return None;
            }
        };
        let params = match self.parse_params() {
            Ok(params) => params,
            Err(error) => {
                errors.push(error);
                self.recover_top_level();
                return None;
            }
        };
        let body = self.parse_block_collecting(errors)?;
        Some(FunctionDef { name, params, body })
    }

    fn parse_class(&mut self) -> Result<(String, Vec<MethodDef>), ParseError> {
        self.expect(TokenKind::Class)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.check(&TokenKind::Async) {
                self.advance();
            }
            let method_name = self.expect_ident()?;
            let verb = method_name.to_lowercase();
            if !KNOWN_VERBS.contains(&verb.as_str()) {
                return Err(self.error_here(&format!(
                    "method {method_name:?} is not an HTTP verb; expected one of {KNOWN_VERBS:?}"
                )));
            }

            let params = self.parse_params()?;
            if params.len() > 1 {
                return Err(
                    self.error_here("route methods currently accept zero or one request parameter")
                );
            }
            let body = self.parse_block()?;
            methods.push(MethodDef {
                verb,
                param_name: params.into_iter().next(),
                body,
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok((name, methods))
    }

    fn parse_class_collecting(
        &mut self,
        errors: &mut Vec<ParseError>,
    ) -> Option<(String, Vec<MethodDef>)> {
        if let Err(error) = self.expect(TokenKind::Class) {
            errors.push(error);
            return None;
        }
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(error) => {
                errors.push(error);
                return None;
            }
        };
        if let Err(error) = self.expect(TokenKind::LBrace) {
            errors.push(error);
            return None;
        }

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.check(&TokenKind::Async) {
                self.advance();
            }
            let method_name = match self.expect_ident() {
                Ok(name) => name,
                Err(error) => {
                    errors.push(error);
                    self.recover_class_member();
                    continue;
                }
            };
            let verb = method_name.to_lowercase();
            if !KNOWN_VERBS.contains(&verb.as_str()) {
                errors.push(self.error_here(&format!(
                    "method {method_name:?} is not an HTTP verb; expected one of {KNOWN_VERBS:?}"
                )));
                self.recover_class_member();
                continue;
            }

            let params = match self.parse_params() {
                Ok(params) => params,
                Err(error) => {
                    errors.push(error);
                    self.recover_class_member();
                    continue;
                }
            };
            if params.len() > 1 {
                errors.push(
                    self.error_here("route methods currently accept zero or one request parameter"),
                );
                self.recover_class_member();
                continue;
            }
            let Some(body) = self.parse_block_collecting(errors) else {
                self.recover_class_member();
                continue;
            };
            methods.push(MethodDef {
                verb,
                param_name: params.into_iter().next(),
                body,
            });
        }

        if let Err(error) = self.expect(TokenKind::RBrace) {
            errors.push(error);
            return None;
        }
        Some((name, methods))
    }

    fn parse_params(&mut self) -> Result<Vec<String>, ParseError> {
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                params.push(self.parse_identifier_spelling()?);
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok(params)
    }

    fn parse_identifier_spelling(&mut self) -> Result<String, ParseError> {
        if self.check(&TokenKind::Dollar) {
            self.advance();
            self.expect(TokenKind::LBracket)?;
            let name = self.expect_ident()?;
            self.expect(TokenKind::RBracket)?;
            return Ok(name);
        }
        self.expect_ident()
    }

    fn parse_block(&mut self) -> Result<Vec<Statement>, ParseError> {
        let mut errors = Vec::new();
        let body = match self.parse_block_collecting(&mut errors) {
            Some(body) => body,
            None => {
                return Err(errors.into_iter().next().unwrap_or(ParseError {
                    message: "failed to parse block".to_string(),
                    line: 0,
                    column: 0,
                }));
            }
        };
        if let Some(error) = errors.into_iter().next() {
            Err(error)
        } else {
            Ok(body)
        }
    }

    fn parse_block_collecting(&mut self, errors: &mut Vec<ParseError>) -> Option<Vec<Statement>> {
        if let Err(error) = self.expect(TokenKind::LBrace) {
            errors.push(error);
            return None;
        }

        let mut body = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            match self.parse_statement() {
                Ok(statement) => body.push(statement),
                Err(error) => {
                    errors.push(error);
                    self.recover_statement();
                }
            }
        }

        if let Err(error) = self.expect(TokenKind::RBrace) {
            errors.push(error);
            return None;
        }
        Some(body)
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek_kind() {
            TokenKind::Const | TokenKind::Let => {
                self.advance();
                let name = self.parse_identifier_spelling()?;
                self.expect(TokenKind::Eq)?;
                let value = self.parse_expression()?;
                self.expect(TokenKind::Semicolon)?;
                Ok(Statement::Const { name, value })
            }
            TokenKind::Return => {
                self.advance();
                let value = self.parse_expression()?;
                self.expect(TokenKind::Semicolon)?;
                Ok(Statement::Return(value))
            }
            TokenKind::If => self.parse_if(),
            _ => {
                let expr = self.parse_expression()?;
                self.expect(TokenKind::Semicolon)?;
                Ok(Statement::Expr(expr))
            }
        }
    }

    fn parse_if(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::If)?;
        self.expect(TokenKind::LParen)?;
        let condition = self.parse_expression()?;
        self.expect(TokenKind::RParen)?;
        let then_body = self.parse_block()?;
        let else_body = if self.check(&TokenKind::Else) {
            self.advance();
            self.parse_block()?
        } else {
            Vec::new()
        };
        Ok(Statement::If {
            condition,
            then_body,
            else_body,
        })
    }

    fn parse_expression(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_and()?;
        while self.check(&TokenKind::OrOr) {
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op: BinaryOp::Or,
                right: Box::new(self.parse_and()?),
            };
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_equality()?;
        while self.check(&TokenKind::AndAnd) {
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op: BinaryOp::And,
                right: Box::new(self.parse_equality()?),
            };
        }
        Ok(expr)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_comparison()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::EqEq => Some(BinaryOp::Equal),
                TokenKind::EqEqEq => Some(BinaryOp::StrictEqual),
                TokenKind::NotEq => Some(BinaryOp::NotEqual),
                TokenKind::NotEqEq => Some(BinaryOp::StrictNotEqual),
                _ => None,
            };
            let Some(op) = op else { break };
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_comparison()?),
            };
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_term()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => Some(BinaryOp::Less),
                TokenKind::LtEq => Some(BinaryOp::LessEqual),
                TokenKind::Gt => Some(BinaryOp::Greater),
                TokenKind::GtEq => Some(BinaryOp::GreaterEqual),
                _ => None,
            };
            let Some(op) = op else { break };
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_term()?),
            };
        }
        Ok(expr)
    }

    fn parse_term(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_factor()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => Some(BinaryOp::Add),
                TokenKind::Minus => Some(BinaryOp::Subtract),
                _ => None,
            };
            let Some(op) = op else { break };
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_factor()?),
            };
        }
        Ok(expr)
    }

    fn parse_factor(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => Some(BinaryOp::Multiply),
                TokenKind::Slash => Some(BinaryOp::Divide),
                TokenKind::Percent => Some(BinaryOp::Modulo),
                _ => None,
            };
            let Some(op) = op else { break };
            self.advance();
            expr = Expr::Binary {
                left: Box::new(expr),
                op,
                right: Box::new(self.parse_unary()?),
            };
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.check(&TokenKind::Not) {
            self.advance();
            return Ok(Expr::UnaryNot(Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.advance();
                    let field = self.expect_ident()?;
                    expr = Expr::Member(Box::new(expr), field);
                }
                TokenKind::LParen => {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expression()?);
                            if !self.check(&TokenKind::Comma) {
                                break;
                            }
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    expr = Expr::Call(Box::new(expr), args);
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        match self.advance().kind {
            TokenKind::String(s) => Ok(Expr::String(s)),
            TokenKind::Number(n) => Ok(Expr::Number(n)),
            TokenKind::True => Ok(Expr::Bool(true)),
            TokenKind::False => Ok(Expr::Bool(false)),
            TokenKind::Null => Ok(Expr::Null),
            TokenKind::Ident(name) => Ok(Expr::Ident(name)),
            TokenKind::Dollar => {
                self.expect(TokenKind::LBracket)?;
                let name = self.expect_ident()?;
                self.expect(TokenKind::RBracket)?;
                Ok(Expr::Ident(name))
            }
            TokenKind::LBrace => self.parse_object_tail(),
            TokenKind::LBracket => self.parse_array_tail(),
            other => Err(self.error_here(&format!("unexpected token in expression: {other:?}"))),
        }
    }

    fn parse_object_tail(&mut self) -> Result<Expr, ParseError> {
        let mut fields = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            loop {
                let key = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let value = self.parse_expression()?;
                fields.push((key, value));
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
                if self.check(&TokenKind::RBrace) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Object(fields))
    }

    fn parse_array_tail(&mut self) -> Result<Expr, ParseError> {
        let mut items = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            loop {
                items.push(self.parse_expression()?);
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(Expr::Array(items))
    }

    fn recover_statement(&mut self) {
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;

        while !self.check(&TokenKind::Eof) {
            match self.peek_kind() {
                TokenKind::LParen => {
                    paren_depth += 1;
                    self.advance();
                }
                TokenKind::RParen if paren_depth > 0 => {
                    paren_depth -= 1;
                    self.advance();
                }
                TokenKind::LBracket => {
                    bracket_depth += 1;
                    self.advance();
                }
                TokenKind::RBracket if bracket_depth > 0 => {
                    bracket_depth -= 1;
                    self.advance();
                }
                TokenKind::LBrace => {
                    brace_depth += 1;
                    self.advance();
                }
                TokenKind::RBrace if brace_depth > 0 => {
                    brace_depth -= 1;
                    self.advance();
                }
                TokenKind::Semicolon
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    self.advance();
                    break;
                }
                TokenKind::RBrace if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                    break
                }
                TokenKind::Const
                | TokenKind::Let
                | TokenKind::Return
                | TokenKind::If
                | TokenKind::Else
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    break
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn recover_top_level(&mut self) {
        while !self.check(&TokenKind::Eof)
            && !self.check(&TokenKind::Colon)
            && !self.check(&TokenKind::Function)
            && !self.check(&TokenKind::Class)
        {
            self.advance();
        }
    }

    fn recover_class_member(&mut self) {
        while !self.check(&TokenKind::Eof) && !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Async) || self.is_identifier_followed_by_lparen() {
                return;
            }
            self.advance();
        }
    }

    fn is_identifier_followed_by_lparen(&self) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(TokenKind::Ident(_))
        ) && matches!(
            self.tokens.get(self.pos + 1).map(|token| &token.kind),
            Some(TokenKind::LParen)
        )
    }

    fn peek_kind(&self) -> TokenKind {
        self.tokens
            .get(self.pos)
            .map(|t| t.kind.clone())
            .unwrap_or(TokenKind::Eof)
    }

    fn check(&self, expected: &TokenKind) -> bool {
        self.tokens
            .get(self.pos)
            .map(|t| &t.kind == expected)
            .unwrap_or(false)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token {
            kind: TokenKind::Eof,
            line: 0,
            column: 0,
            end_line: 0,
            end_column: 0,
            start: 0,
            end: 0,
        });
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: TokenKind) -> Result<(), ParseError> {
        if self.check(&expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.error_here(&format!(
                "expected {expected:?}, got {:?}",
                self.peek_kind()
            )))
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.advance().kind {
            TokenKind::Ident(name) => Ok(name),
            other => Err(self.error_here(&format!("expected identifier, got {other:?}"))),
        }
    }

    fn error_here(&self, message: &str) -> ParseError {
        let token = self.tokens.get(self.pos);
        ParseError {
            message: message.to_string(),
            line: token.map(|t| t.line).unwrap_or(0),
            column: token.map(|t| t.column).unwrap_or(0),
        }
    }
}
