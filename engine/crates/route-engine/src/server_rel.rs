//! Structural parser/compiler front-end for the root `server.server` Server REL
//! source.
//!
//! Server-only policy syntax is parsed here, while shared REL imports and
//! helper functions are deliberately delegated back through the common REL
//! parser. Source role adds capabilities; it does not create a second grammar.

use std::collections::BTreeMap;
use std::fmt;

use crate::ast::{FunctionDef, ImportTarget};
use crate::lexer::{Lexer, Token, TokenKind};
use crate::parser::{ParseError, Parser};

#[derive(Debug, Clone)]
pub struct ServerProgram {
    pub imports: Vec<ImportTarget>,
    pub name: String,
    pub settings: Vec<ServerSetting>,
    pub functions: Vec<FunctionDef>,
}

impl ServerProgram {
    pub fn setting(&self, name: &str) -> Option<&ServerSetting> {
        self.settings.iter().find(|setting| setting.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSetting {
    pub name: String,
    pub forced: bool,
    pub body: ServerSettingBody,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerSettingBody {
    Flag,
    Value(ServerValue),
    Block(Vec<ServerSetting>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerValue {
    String(String),
    Number(f64),
    Quantity { value: f64, unit: String },
    Bool(bool),
    Null,
    Ident(String),
    Array(Vec<ServerValue>),
}

#[derive(Debug, Clone)]
pub enum ServerCompileError {
    Parse(ParseError),
    Semantic {
        message: String,
        line: usize,
        column: usize,
    },
}

impl ServerCompileError {
    fn semantic(setting: &ServerSetting, message: impl Into<String>) -> Self {
        Self::Semantic {
            message: message.into(),
            line: setting.line,
            column: setting.column,
        }
    }
}

impl fmt::Display for ServerCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(
                formatter,
                "Server REL parse error at {}:{}: {}",
                error.line, error.column, error.message
            ),
            Self::Semantic {
                message,
                line,
                column,
            } => write!(
                formatter,
                "Server REL semantic error at {line}:{column}: {message}"
            ),
        }
    }
}

impl std::error::Error for ServerCompileError {}

impl From<ParseError> for ServerCompileError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Parses the structural `server.server` source while reusing the common REL
/// parser for imports and helper functions.
pub fn parse_server_source(source: &str) -> Result<ServerProgram, ParseError> {
    let tokens = Lexer::new(source).tokenize().map_err(|error| ParseError {
        message: error.message,
        line: error.line,
        column: error.column,
    })?;
    ServerParser::new(source, tokens).parse()
}

/// Compiles the current Server REL front-end representation.
///
/// This performs structural semantic checks that are already stable. Final
/// `ServerPolicy`, FORCE precedence, Runtime ENV merging and MiddlewarePlan
/// lowering intentionally remain later RELC passes.
pub fn compile_server_source(source: &str) -> Result<ServerProgram, ServerCompileError> {
    let program = parse_server_source(source)?;
    validate_server_program(&program)?;
    Ok(program)
}

fn validate_server_program(program: &ServerProgram) -> Result<(), ServerCompileError> {
    let mut reserved = BTreeMap::<String, (usize, usize)>::new();
    for setting in &program.settings {
        let normalized = setting.name.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "status" | "listener" | "env" | "middleware"
        ) {
            if reserved
                .insert(normalized.clone(), (setting.line, setting.column))
                .is_some()
            {
                return Err(ServerCompileError::semantic(
                    setting,
                    format!("duplicate `{}` Server REL section/setting", setting.name),
                ));
            }
        }

        match normalized.as_str() {
            "status" => validate_status(setting)?,
            "listener" | "middleware" => require_block(setting)?,
            "env" => validate_env(setting)?,
            _ => {}
        }
    }
    Ok(())
}

fn validate_status(setting: &ServerSetting) -> Result<(), ServerCompileError> {
    let ServerSettingBody::Value(ServerValue::Ident(status)) = &setting.body else {
        return Err(ServerCompileError::semantic(
            setting,
            "`status` must be an identifier such as `online` or `maintenance`",
        ));
    };
    if !matches!(
        status.to_ascii_lowercase().as_str(),
        "online" | "maintenance" | "draining" | "readonly" | "offline"
    ) {
        return Err(ServerCompileError::semantic(
            setting,
            format!("unsupported server status `{status}`"),
        ));
    }
    Ok(())
}

fn require_block(setting: &ServerSetting) -> Result<(), ServerCompileError> {
    if matches!(&setting.body, ServerSettingBody::Block(_)) {
        Ok(())
    } else {
        Err(ServerCompileError::semantic(
            setting,
            format!("`{}` must use a configuration block", setting.name),
        ))
    }
}

fn validate_env(setting: &ServerSetting) -> Result<(), ServerCompileError> {
    let ServerSettingBody::Block(entries) = &setting.body else {
        return Err(ServerCompileError::semantic(
            setting,
            "`env` must use a configuration block",
        ));
    };

    let mut names = BTreeMap::<&str, (usize, usize)>::new();
    for entry in entries {
        if names
            .insert(entry.name.as_str(), (entry.line, entry.column))
            .is_some()
        {
            return Err(ServerCompileError::semantic(
                entry,
                format!("duplicate Runtime ENV default `{}`", entry.name),
            ));
        }
        if !matches!(&entry.body, ServerSettingBody::Value(_)) {
            return Err(ServerCompileError::semantic(
                entry,
                "Runtime ENV defaults must have a literal/identifier value",
            ));
        }
    }
    Ok(())
}

struct ServerParser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> ServerParser<'a> {
    fn new(source: &'a str, tokens: Vec<Token>) -> Self {
        Self {
            source,
            tokens,
            pos: 0,
        }
    }

    fn parse(mut self) -> Result<ServerProgram, ParseError> {
        let imports = self.parse_shared_import_prefix()?;
        self.expect_ident_value("server")?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;

        let mut settings = Vec::new();
        let mut functions = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.starts_helper_function() {
                let function = self.parse_shared_helper_function()?;
                if functions
                    .iter()
                    .any(|existing: &FunctionDef| existing.name == function.name)
                {
                    return Err(self.error_here(&format!(
                        "duplicate Server REL helper function {:?}",
                        function.name
                    )));
                }
                functions.push(function);
                continue;
            }

            if self.is_force_keyword() {
                self.advance();
                if self.check(&TokenKind::LBrace) {
                    self.advance();
                    let mut forced = self.parse_setting_block(true)?;
                    self.expect(TokenKind::RBrace)?;
                    settings.append(&mut forced);
                } else {
                    settings.push(self.parse_named_setting(true)?);
                }
                continue;
            }

            settings.push(self.parse_named_setting(false)?);
        }

        self.expect(TokenKind::RBrace)?;
        if !self.check(&TokenKind::Eof) {
            return Err(self.error_here(
                "unexpected tokens after the root `server NAME { ... }` declaration",
            ));
        }

        Ok(ServerProgram {
            imports,
            name,
            settings,
            functions,
        })
    }

    /// Parses leading `:import[...]` directives through the existing Module REL
    /// parser instead of maintaining a second import grammar here.
    fn parse_shared_import_prefix(&mut self) -> Result<Vec<ImportTarget>, ParseError> {
        if !self.is_import_directive() {
            return Ok(Vec::new());
        }

        let start = self.current().start;
        let mut end = start;
        while self.is_import_directive() {
            if !self
                .tokens
                .get(self.pos + 2)
                .map(|token| matches!(&token.kind, TokenKind::LBracket))
                .unwrap_or(false)
            {
                return Err(self.error_here("expected `[` after `:import` in Server REL"));
            }
            let close = self.find_matching(self.pos + 2, TokenKind::LBracket, TokenKind::RBracket)?;
            end = self.tokens[close].end;
            self.pos = close + 1;
        }

        let mut fragment = self.source[start..end].to_string();
        fragment.push_str("\nfunction __rbe_server_import_probe() { return null; }");
        let tokens = Lexer::new(&fragment).tokenize().map_err(|error| ParseError {
            message: error.message,
            line: error.line,
            column: error.column,
        })?;
        let module = Parser::new(tokens).parse_module_file()?;
        Ok(module.imports)
    }

    /// Parses one helper function through the shared Module REL function parser.
    fn parse_shared_helper_function(&mut self) -> Result<FunctionDef, ParseError> {
        let start_index = self.pos;
        let start_token = self.tokens[start_index].clone();
        let mut body_start = None;
        for index in start_index..self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::LBrace => {
                    body_start = Some(index);
                    break;
                }
                TokenKind::Eof => break,
                _ => {}
            }
        }
        let body_start = body_start.ok_or_else(|| {
            self.error_at(
                &start_token,
                "unterminated Server REL helper function declaration",
            )
        })?;
        let body_end = self.find_matching(body_start, TokenKind::LBrace, TokenKind::RBrace)?;
        let end_offset = self.tokens[body_end].end;
        let fragment = &self.source[start_token.start..end_offset];

        let tokens = Lexer::new(fragment).tokenize().map_err(|error| {
            remap_fragment_error(
                ParseError {
                    message: error.message,
                    line: error.line,
                    column: error.column,
                },
                &start_token,
            )
        })?;
        let module = Parser::new(tokens)
            .parse_module_file()
            .map_err(|error| remap_fragment_error(error, &start_token))?;
        if module.functions.len() != 1 || !module.imports.is_empty() || !module.exports.is_empty() {
            return Err(self.error_at(
                &start_token,
                "Server REL helper fragment did not produce exactly one local function",
            ));
        }

        self.pos = body_end + 1;
        module.functions.into_iter().next().ok_or_else(|| {
            self.error_at(
                &start_token,
                "Server REL helper function did not produce an AST",
            )
        })
    }

    fn parse_setting_block(
        &mut self,
        inherited_force: bool,
    ) -> Result<Vec<ServerSetting>, ParseError> {
        let mut settings = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.starts_helper_function() {
                return Err(self.error_here(
                    "helper functions belong to the root Server REL body, not a policy block",
                ));
            }
            if self.is_force_keyword() {
                self.advance();
                if self.check(&TokenKind::LBrace) {
                    self.advance();
                    let mut forced = self.parse_setting_block(true)?;
                    self.expect(TokenKind::RBrace)?;
                    settings.append(&mut forced);
                } else {
                    settings.push(self.parse_named_setting(true)?);
                }
            } else {
                settings.push(self.parse_named_setting(inherited_force)?);
            }
        }
        Ok(settings)
    }

    fn parse_named_setting(&mut self, forced: bool) -> Result<ServerSetting, ParseError> {
        let token = self.current().clone();
        let name = self.expect_ident()?;
        let body = if self.check(&TokenKind::Semicolon) {
            self.advance();
            ServerSettingBody::Flag
        } else if self.check(&TokenKind::LBrace) {
            self.advance();
            let settings = self.parse_setting_block(forced)?;
            self.expect(TokenKind::RBrace)?;
            ServerSettingBody::Block(settings)
        } else {
            let value = self.parse_value()?;
            self.expect(TokenKind::Semicolon)?;
            ServerSettingBody::Value(value)
        };

        Ok(ServerSetting {
            name,
            forced,
            body,
            line: token.line,
            column: token.column,
        })
    }

    fn parse_value(&mut self) -> Result<ServerValue, ParseError> {
        match self.current().kind.clone() {
            TokenKind::String(value) => {
                self.advance();
                Ok(ServerValue::String(value))
            }
            TokenKind::Number(value) => self.parse_number_or_quantity(value, false),
            TokenKind::Minus => {
                self.advance();
                let TokenKind::Number(value) = self.current().kind.clone() else {
                    return Err(self.error_here("expected number after `-` in Server REL value"));
                };
                self.parse_number_or_quantity(value, true)
            }
            TokenKind::True => {
                self.advance();
                Ok(ServerValue::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(ServerValue::Bool(false))
            }
            TokenKind::Null => {
                self.advance();
                Ok(ServerValue::Null)
            }
            TokenKind::Ident(value) => {
                self.advance();
                Ok(ServerValue::Ident(value))
            }
            TokenKind::LBracket => self.parse_array(),
            _ => Err(self.error_here(
                "expected Server REL value (string, number, quantity, boolean, null, identifier, or array)",
            )),
        }
    }

    fn parse_number_or_quantity(
        &mut self,
        value: f64,
        negative: bool,
    ) -> Result<ServerValue, ParseError> {
        let number = self.current().clone();
        self.advance();
        let value = if negative { -value } else { value };
        let attached_unit = self.tokens.get(self.pos).and_then(|token| {
            if number.end == token.start {
                if let TokenKind::Ident(unit) = &token.kind {
                    return Some(unit.clone());
                }
            }
            None
        });
        if let Some(unit) = attached_unit {
            self.advance();
            return Ok(ServerValue::Quantity { value, unit });
        }
        Ok(ServerValue::Number(value))
    }

    fn parse_array(&mut self) -> Result<ServerValue, ParseError> {
        self.expect(TokenKind::LBracket)?;
        let mut values = Vec::new();
        while !self.check(&TokenKind::RBracket) {
            values.push(self.parse_value()?);
            if self.check(&TokenKind::RBracket) {
                break;
            }
            self.expect(TokenKind::Comma)?;
            if self.check(&TokenKind::RBracket) {
                return Err(self.error_here("trailing commas are not allowed in Server REL arrays"));
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(ServerValue::Array(values))
    }

    fn starts_helper_function(&self) -> bool {
        self.check(&TokenKind::Function)
            || (self.check(&TokenKind::Async)
                && matches!(
                    self.tokens.get(self.pos + 1).map(|token| &token.kind),
                    Some(TokenKind::Function)
                ))
    }

    fn is_import_directive(&self) -> bool {
        self.check(&TokenKind::Colon)
            && matches!(
                self.tokens.get(self.pos + 1).map(|token| &token.kind),
                Some(TokenKind::Import)
            )
    }

    fn is_force_keyword(&self) -> bool {
        matches!(
            &self.current().kind,
            TokenKind::Ident(name) if name.eq_ignore_ascii_case("force")
        )
    }

    fn find_matching(
        &self,
        start: usize,
        open: TokenKind,
        close: TokenKind,
    ) -> Result<usize, ParseError> {
        if !self
            .tokens
            .get(start)
            .map(|token| same_token_variant(&token.kind, &open))
            .unwrap_or(false)
        {
            return Err(self.error_here("internal Server REL delimiter scan started at wrong token"));
        }

        let mut depth = 0usize;
        for index in start..self.tokens.len() {
            let kind = &self.tokens[index].kind;
            if same_token_variant(kind, &open) {
                depth += 1;
            } else if same_token_variant(kind, &close) {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
            } else if matches!(kind, TokenKind::Eof) {
                break;
            }
        }
        Err(self.error_at(&self.tokens[start], "unterminated Server REL delimiter block"))
    }

    fn expect_ident_value(&mut self, expected: &str) -> Result<(), ParseError> {
        match self.advance().kind {
            TokenKind::Ident(value) if value == expected => Ok(()),
            other => Err(self.error_here(&format!(
                "expected `{expected}` in Server REL root declaration, got {other:?}"
            ))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.advance().kind {
            TokenKind::Ident(value) => Ok(value),
            other => Err(self.error_here(&format!("expected identifier, got {other:?}"))),
        }
    }

    fn expect(&mut self, expected: TokenKind) -> Result<(), ParseError> {
        if self.check(&expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.error_here(&format!("expected {expected:?}")))
        }
    }

    fn check(&self, expected: &TokenKind) -> bool {
        same_token_variant(&self.current().kind, expected)
    }

    fn current(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .expect("lexer always emits EOF")
    }

    fn advance(&mut self) -> Token {
        let token = self.current().clone();
        if !matches!(token.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        token
    }

    fn error_here(&self, message: &str) -> ParseError {
        self.error_at(self.current(), message)
    }

    fn error_at(&self, token: &Token, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            line: token.line,
            column: token.column,
        }
    }
}

fn remap_fragment_error(mut error: ParseError, start: &Token) -> ParseError {
    if error.line == 1 {
        error.column += start.column.saturating_sub(1);
    }
    error.line += start.line.saturating_sub(1);
    error
}

fn same_token_variant(left: &TokenKind, right: &TokenKind) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_server_rel_shape() {
        let program = compile_server_source(
            r#"
            :import[json]
            server Main {
                status online;

                listener {
                    host "0.0.0.0";
                    port 7044;
                }

                force requestLimit 2mb;

                env {
                    APP_NAME "RBE";
                    REGION local;
                    SESSION_TTL 86400;
                }

                middleware {
                    correlationId;
                    realIp;
                    json {
                        limit 2mb;
                        strict true;
                    }
                    compression {
                        threshold 1kb;
                        algorithms [br, gzip, zstd];
                    }
                }

                function normalize(value) {
                    if (value == null) {
                        return "unknown";
                    } else {
                        return value;
                    }
                }
            }
            "#,
        )
        .unwrap();

        assert_eq!(program.name, "Main");
        assert_eq!(program.imports.len(), 1);
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "normalize");
        assert!(program.setting("listener").is_some());
        assert!(program.setting("env").is_some());
        assert!(program.setting("middleware").is_some());

        let request_limit = program.setting("requestLimit").unwrap();
        assert!(request_limit.forced);
        assert_eq!(
            request_limit.body,
            ServerSettingBody::Value(ServerValue::Quantity {
                value: 2.0,
                unit: "mb".to_string(),
            })
        );
    }

    #[test]
    fn force_block_marks_all_contained_settings() {
        let program = compile_server_source(
            r#"
            server Main {
                force {
                    timeout 30s;
                    listener {
                        port 7044;
                    }
                }
            }
            "#,
        )
        .unwrap();

        assert!(program.setting("timeout").unwrap().forced);
        let listener = program.setting("listener").unwrap();
        assert!(listener.forced);
        let ServerSettingBody::Block(entries) = &listener.body else {
            panic!("listener should be a block");
        };
        assert!(entries[0].forced);
    }

    #[test]
    fn helper_functions_use_shared_rel_function_grammar() {
        let program = parse_server_source(
            r#"
            server Main {
                async function choose(value) {
                    if (!value || value == null) {
                        return { ok: false };
                    } else {
                        return { ok: true, value: value };
                    }
                }
            }
            "#,
        )
        .unwrap();
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "choose");
    }

    #[test]
    fn rejects_duplicate_helper_functions() {
        let error = parse_server_source(
            r#"
            server Main {
                function same() { return true; }
                function same() { return false; }
            }
            "#,
        )
        .unwrap_err();
        assert!(error.message.contains("duplicate Server REL helper"));
    }

    #[test]
    fn compiler_rejects_unknown_server_status() {
        let error = compile_server_source("server Main { status banana; }").unwrap_err();
        assert!(error.to_string().contains("unsupported server status"));
    }

    #[test]
    fn compiler_rejects_non_block_env() {
        let error = compile_server_source("server Main { env true; }").unwrap_err();
        assert!(error.to_string().contains("`env` must use a configuration block"));
    }

    #[test]
    fn quantity_unit_must_be_attached() {
        let error = parse_server_source("server Main { timeout 30 s; }").unwrap_err();
        assert!(error.message.contains("expected Semicolon"));
    }

    #[test]
    fn rejects_nested_server_root_after_main_server() {
        let error = parse_server_source(
            "server Main { status online; } server Other { status offline; }",
        )
        .unwrap_err();
        assert!(error.message.contains("unexpected tokens after"));
    }
}
