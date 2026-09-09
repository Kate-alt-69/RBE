//! Compiler foundation for the root `server.server` REL source.
//!
//! Server REL owns whole-server composition and policy. Embedded REL sources are
//! extracted before policy parsing so their contents are later compiled by the
//! route/module/service compiler that owns that source role.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde_json::{Number, Value};

pub const NATIVE_MIDDLEWARE: &[&str] = &[
    "correlationId",
    "realIp",
    "forwarded",
    "requestTiming",
    "requestLog",
    "json",
    "text",
    "form",
    "multipart",
    "rawBody",
    "cookies",
    "cors",
    "compression",
    "securityHeaders",
    "csp",
    "hsts",
    "rateLimit",
    "ipBan",
    "timeout",
    "cache",
    "etag",
    "auth",
    "errorHandler",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Online,
    Maintenance,
    Draining,
    Readonly,
    Offline,
}

impl ServerStatus {
    fn parse(value: &str, token: &Token) -> Result<Self, ServerRelError> {
        match value {
            "online" => Ok(Self::Online),
            "maintenance" => Ok(Self::Maintenance),
            "draining" => Ok(Self::Draining),
            "readonly" => Ok(Self::Readonly),
            "offline" => Ok(Self::Offline),
            _ => Err(ServerRelError::at(
                "SRV1004",
                token,
                format!(
                    "unknown server status {value:?}; expected online, maintenance, draining, readonly, or offline"
                ),
            )),
        }
    }
}

impl Default for ServerStatus {
    fn default() -> Self {
        Self::Online
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MiddlewarePolicy {
    pub name: String,
    pub options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedSourceKind {
    Route,
    Module,
    Service,
}

impl EmbeddedSourceKind {
    fn parse(value: &str, line: usize) -> Result<Self, ServerRelError> {
        match value {
            "route" => Ok(Self::Route),
            "module" => Ok(Self::Module),
            "service" => Ok(Self::Service),
            _ => Err(ServerRelError::new(
                "SRV1010",
                line,
                1,
                format!("unsupported embedded REL source kind {value:?}"),
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Module => "module",
            Self::Service => "service",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSource {
    pub kind: EmbeddedSourceKind,
    pub name: String,
    pub attributes: BTreeMap<String, String>,
    pub source: String,
    pub source_id: String,
    pub start_line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerPolicy {
    pub name: String,
    pub status: ServerStatus,
    /// Public typed runtime ENV defaults supplied by Server REL.
    pub environment: BTreeMap<String, Value>,
    /// ENV values locked by Server REL `force` policy.
    pub forced_environment: BTreeMap<String, Value>,
    /// Flattened normal policy defaults such as `api.port`.
    pub defaults: BTreeMap<String, Value>,
    /// Flattened locked policy values such as `security.trustedProxyHeaders`.
    pub forced: BTreeMap<String, Value>,
    /// Ordered native middleware plan.
    pub middleware: Vec<MiddlewarePolicy>,
    /// Literal sources extracted from the root server source.
    pub embedded_sources: Vec<EmbeddedSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRelError {
    pub code: &'static str,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl ServerRelError {
    fn new(
        code: &'static str,
        line: usize,
        column: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            line,
            column,
            message: message.into(),
        }
    }

    fn at(code: &'static str, token: &Token, message: impl Into<String>) -> Self {
        Self::new(code, token.line, token.column, message)
    }
}

impl fmt::Display for ServerRelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}:{}: {}",
            self.code, self.line, self.column, self.message
        )
    }
}

impl std::error::Error for ServerRelError {}

/// Compile one root Server REL source into a deterministic policy object.
pub fn compile_server_source(source: &str) -> Result<ServerPolicy, ServerRelError> {
    let (policy_source, embedded_sources) = extract_embedded_sources(source)?;
    let tokens = tokenize(&policy_source)?;
    Parser::new(tokens, embedded_sources).parse()
}

/// Extract literal REL sources while retaining line count in the remaining
/// Server REL text. The returned virtual source ids are stable and are suitable
/// for RELC duplicate-identity diagnostics.
pub fn extract_embedded_sources(
    source: &str,
) -> Result<(String, Vec<EmbeddedSource>), ServerRelError> {
    let lines: Vec<&str> = source.lines().collect();
    let mut policy = String::new();
    let mut embedded = Vec::new();
    let mut identities = HashSet::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line_number = index + 1;
        let trimmed = lines[index].trim();
        let Some(header) = trimmed
            .strip_prefix("[file-start:")
            .and_then(|value| value.strip_suffix(']'))
        else {
            policy.push_str(lines[index]);
            policy.push('\n');
            index += 1;
            continue;
        };

        let (kind, name, attributes) = parse_embedded_header(header, line_number)?;
        let end_marker = format!("[file-end:{}]", kind.label());
        let start_line = line_number;
        policy.push('\n');
        index += 1;

        let mut body = String::new();
        let mut found_end = false;
        while index < lines.len() {
            let current_number = index + 1;
            let current = lines[index];
            let current_trimmed = current.trim();
            if current_trimmed == end_marker {
                policy.push('\n');
                found_end = true;
                index += 1;
                break;
            }
            if current_trimmed.starts_with("[file-start:") {
                return Err(ServerRelError::new(
                    "SRV1011",
                    current_number,
                    1,
                    "embedded REL blocks cannot be nested",
                ));
            }
            body.push_str(current);
            body.push('\n');
            policy.push('\n');
            index += 1;
        }

        if !found_end {
            return Err(ServerRelError::new(
                "SRV1012",
                start_line,
                1,
                format!("embedded source is missing closing marker {end_marker}"),
            ));
        }

        let source_id = format!("server.server#{}:{}", kind.label(), name);
        if !identities.insert(source_id.clone()) {
            return Err(ServerRelError::new(
                "SRV1013",
                start_line,
                1,
                format!("duplicate embedded source identity {source_id}"),
            ));
        }

        embedded.push(EmbeddedSource {
            kind,
            name,
            attributes,
            source: body,
            source_id,
            start_line,
        });
    }

    Ok((policy, embedded))
}

fn parse_embedded_header(
    header: &str,
    line: usize,
) -> Result<(EmbeddedSourceKind, String, BTreeMap<String, String>), ServerRelError> {
    let mut pieces = split_header_fields(header, line)?;
    let identity = pieces
        .next()
        .ok_or_else(|| ServerRelError::new("SRV1014", line, 1, "empty embedded source header"))?;
    let (kind, name) = identity.split_once('.').ok_or_else(|| {
        ServerRelError::new(
            "SRV1014",
            line,
            1,
            "embedded source must use KIND.NAME identity",
        )
    })?;
    let kind = EmbeddedSourceKind::parse(kind, line)?;
    if name.trim().is_empty() {
        return Err(ServerRelError::new(
            "SRV1014",
            line,
            1,
            "embedded source name must not be empty",
        ));
    }

    let mut attributes = BTreeMap::new();
    for field in pieces {
        let (key, value) = field.split_once('=').ok_or_else(|| {
            ServerRelError::new(
                "SRV1015",
                line,
                1,
                format!("embedded source attribute {field:?} must use key=value"),
            )
        })?;
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if attributes.insert(key.to_string(), value.to_string()).is_some() {
            return Err(ServerRelError::new(
                "SRV1015",
                line,
                1,
                format!("duplicate embedded source attribute {key:?}"),
            ));
        }
    }

    Ok((kind, name.to_string(), attributes))
}

fn split_header_fields<'a>(
    header: &'a str,
    line: usize,
) -> Result<impl Iterator<Item = &'a str>, ServerRelError> {
    if header.matches('"').count() % 2 != 0 {
        return Err(ServerRelError::new(
            "SRV1015",
            line,
            1,
            "unterminated quote in embedded source header",
        ));
    }

    let mut fields = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (index, character) in header.char_indices() {
        match character {
            '"' => quoted = !quoted,
            value if value.is_whitespace() && !quoted => {
                if start < index {
                    fields.push(&header[start..index]);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < header.len() {
        fields.push(&header[start..]);
    }
    Ok(fields.into_iter())
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Atom(String),
    String(String),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Equal,
    Symbol(char),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    line: usize,
    column: usize,
}

fn tokenize(source: &str) -> Result<Vec<Token>, ServerRelError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 1usize;
    let mut column = 1usize;

    while index < chars.len() {
        let current = chars[index];
        if current.is_whitespace() {
            if current == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
            index += 1;
            continue;
        }

        if current == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
                column += 1;
            }
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'*') {
            let start_line = line;
            let start_column = column;
            index += 2;
            column += 2;
            let mut closed = false;
            while index < chars.len() {
                if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                    index += 2;
                    column += 2;
                    closed = true;
                    break;
                }
                if chars[index] == '\n' {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
                index += 1;
            }
            if !closed {
                return Err(ServerRelError::new(
                    "SRV1001",
                    start_line,
                    start_column,
                    "unterminated block comment",
                ));
            }
            continue;
        }

        let token_line = line;
        let token_column = column;
        let punctuation = match current {
            '{' => Some(TokenKind::LBrace),
            '}' => Some(TokenKind::RBrace),
            '[' => Some(TokenKind::LBracket),
            ']' => Some(TokenKind::RBracket),
            ';' => Some(TokenKind::Semicolon),
            ',' => Some(TokenKind::Comma),
            '=' => Some(TokenKind::Equal),
            _ => None,
        };
        if let Some(kind) = punctuation {
            tokens.push(Token {
                kind,
                line: token_line,
                column: token_column,
            });
            index += 1;
            column += 1;
            continue;
        }

        if current == '"' || current == '\'' {
            let quote = current;
            index += 1;
            column += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < chars.len() {
                let character = chars[index];
                if character == quote {
                    index += 1;
                    column += 1;
                    closed = true;
                    break;
                }
                if character == '\\' {
                    let escaped = chars.get(index + 1).copied().ok_or_else(|| {
                        ServerRelError::new(
                            "SRV1001",
                            token_line,
                            token_column,
                            "unterminated string escape",
                        )
                    })?;
                    let resolved = match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        '\'' => '\'',
                        other => other,
                    };
                    value.push(resolved);
                    index += 2;
                    column += 2;
                    continue;
                }
                if character == '\n' {
                    return Err(ServerRelError::new(
                        "SRV1001",
                        token_line,
                        token_column,
                        "string literal cannot contain an unescaped newline",
                    ));
                }
                value.push(character);
                index += 1;
                column += 1;
            }
            if !closed {
                return Err(ServerRelError::new(
                    "SRV1001",
                    token_line,
                    token_column,
                    "unterminated string literal",
                ));
            }
            tokens.push(Token {
                kind: TokenKind::String(value),
                line: token_line,
                column: token_column,
            });
            continue;
        }

        if is_atom_character(current) {
            let mut value = String::new();
            while index < chars.len() && is_atom_character(chars[index]) {
                value.push(chars[index]);
                index += 1;
                column += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Atom(value),
                line: token_line,
                column: token_column,
            });
            continue;
        }

        tokens.push(Token {
            kind: TokenKind::Symbol(current),
            line: token_line,
            column: token_column,
        });
        index += 1;
        column += 1;
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        line,
        column,
    });
    Ok(tokens)
}

fn is_atom_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(character, '_' | '-' | '.' | '/' | '$' | '@')
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    embedded_sources: Vec<EmbeddedSource>,
}

impl Parser {
    fn new(tokens: Vec<Token>, embedded_sources: Vec<EmbeddedSource>) -> Self {
        Self {
            tokens,
            index: 0,
            embedded_sources,
        }
    }

    fn parse(mut self) -> Result<ServerPolicy, ServerRelError> {
        self.expect_atom("server")?;
        let name = self.take_atom("expected server name after `server`")?;
        self.expect(TokenKind::LBrace, "expected `{` after server name")?;

        let mut policy = ServerPolicy {
            name,
            status: ServerStatus::Online,
            environment: BTreeMap::new(),
            forced_environment: BTreeMap::new(),
            defaults: BTreeMap::new(),
            forced: BTreeMap::new(),
            middleware: Vec::new(),
            embedded_sources: self.embedded_sources,
        };

        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ServerRelError::at(
                    "SRV1002",
                    self.current(),
                    "server block is missing closing `}`",
                ));
            }
            let key_token = self.current().clone();
            let key = self.take_atom("expected Server REL declaration")?;
            match key.as_str() {
                "status" => {
                    let status_token = self.current().clone();
                    let status = self.take_atom("expected server status")?;
                    policy.status = ServerStatus::parse(&status, &status_token)?;
                    self.expect(TokenKind::Semicolon, "expected `;` after server status")?;
                }
                "env" => {
                    self.parse_value_block("", &mut policy.environment)?;
                }
                "force" => {
                    self.parse_force_block(&mut policy)?;
                }
                "middleware" => {
                    policy.middleware = self.parse_middleware_block()?;
                }
                "function" | "class" => self.skip_declaration_body(&key_token)?,
                "async" if self.peek_atom("function") => {
                    self.advance();
                    self.skip_declaration_body(&key_token)?;
                }
                "profile" => {
                    return Err(ServerRelError::at(
                        "SRV1009",
                        &key_token,
                        "profile blocks are reserved but are not runtime-enabled yet",
                    ));
                }
                _ => {
                    if self.check(&TokenKind::LBrace) {
                        self.parse_value_block(&key, &mut policy.defaults)?;
                    } else {
                        let value = self.parse_value()?;
                        self.expect(
                            TokenKind::Semicolon,
                            "expected `;` after Server REL policy value",
                        )?;
                        insert_unique(&mut policy.defaults, key, value, &key_token)?;
                    }
                }
            }
        }
        self.advance();
        if !self.check(&TokenKind::Eof) {
            return Err(ServerRelError::at(
                "SRV1002",
                self.current(),
                "unexpected tokens after root server block",
            ));
        }
        Ok(policy)
    }

    fn parse_force_block(&mut self, policy: &mut ServerPolicy) -> Result<(), ServerRelError> {
        self.expect(TokenKind::LBrace, "expected `{` after `force`")?;
        while !self.check(&TokenKind::RBrace) {
            let key_token = self.current().clone();
            let key = self.take_atom("expected forced policy key")?;
            if key == "env" && self.check(&TokenKind::LBrace) {
                self.parse_value_block("", &mut policy.forced_environment)?;
                continue;
            }
            if self.check(&TokenKind::LBrace) {
                self.parse_value_block(&key, &mut policy.forced)?;
                continue;
            }
            let value = self.parse_value()?;
            self.expect(TokenKind::Semicolon, "expected `;` after forced value")?;
            insert_unique(&mut policy.forced, key, value, &key_token)?;
        }
        self.advance();
        Ok(())
    }

    fn parse_value_block(
        &mut self,
        prefix: &str,
        output: &mut BTreeMap<String, Value>,
    ) -> Result<(), ServerRelError> {
        self.expect(TokenKind::LBrace, "expected `{` to begin policy block")?;
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ServerRelError::at(
                    "SRV1002",
                    self.current(),
                    "policy block is missing closing `}`",
                ));
            }
            let key_token = self.current().clone();
            let key = self.take_atom("expected policy key")?;
            let full_key = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            if self.check(&TokenKind::LBrace) {
                self.parse_value_block(&full_key, output)?;
                continue;
            }
            if self.check(&TokenKind::Equal) {
                self.advance();
            }
            let value = self.parse_value()?;
            self.expect(TokenKind::Semicolon, "expected `;` after policy value")?;
            insert_unique(output, full_key, value, &key_token)?;
        }
        self.advance();
        Ok(())
    }

    fn parse_middleware_block(&mut self) -> Result<Vec<MiddlewarePolicy>, ServerRelError> {
        self.expect(TokenKind::LBrace, "expected `{` after `middleware`")?;
        let mut middleware = Vec::new();
        let mut names = HashSet::new();
        while !self.check(&TokenKind::RBrace) {
            let name_token = self.current().clone();
            let name = self.take_atom("expected native middleware name")?;
            if !NATIVE_MIDDLEWARE.contains(&name.as_str()) {
                return Err(ServerRelError::at(
                    "SRV1005",
                    &name_token,
                    format!("unknown native middleware {name:?}"),
                ));
            }
            if !names.insert(name.clone()) {
                return Err(ServerRelError::at(
                    "SRV1006",
                    &name_token,
                    format!("duplicate native middleware {name:?}"),
                ));
            }
            let options = if self.check(&TokenKind::Semicolon) {
                self.advance();
                BTreeMap::new()
            } else {
                let mut options = BTreeMap::new();
                self.parse_value_block("", &mut options)?;
                options
            };
            middleware.push(MiddlewarePolicy { name, options });
        }
        self.advance();
        Ok(middleware)
    }

    fn parse_value(&mut self) -> Result<Value, ServerRelError> {
        let token = self.current().clone();
        match token.kind {
            TokenKind::String(value) => {
                self.advance();
                Ok(Value::String(value))
            }
            TokenKind::Atom(value) => {
                self.advance();
                match value.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => Ok(parse_atom_value(value)),
                }
            }
            TokenKind::LBracket => self.parse_array(),
            _ => Err(ServerRelError::at(
                "SRV1003",
                &token,
                "expected Server REL literal value",
            )),
        }
    }

    fn parse_array(&mut self) -> Result<Value, ServerRelError> {
        self.advance();
        let mut values = Vec::new();
        while !self.check(&TokenKind::RBracket) {
            values.push(self.parse_value()?);
            if self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    return Err(ServerRelError::at(
                        "SRV1003",
                        self.current(),
                        "trailing commas are not allowed in Server REL arrays",
                    ));
                }
            } else if !self.check(&TokenKind::RBracket) {
                return Err(ServerRelError::at(
                    "SRV1003",
                    self.current(),
                    "expected `,` or `]` in Server REL array",
                ));
            }
        }
        self.advance();
        Ok(Value::Array(values))
    }

    /// Policy compilation currently ignores helper/class bodies after validating
    /// their braces. They remain Server REL source and can be handed to the
    /// shared executable REL path as that linker stage lands.
    fn skip_declaration_body(&mut self, start: &Token) -> Result<(), ServerRelError> {
        while !self.check(&TokenKind::LBrace) {
            if self.check(&TokenKind::Eof) || self.check(&TokenKind::RBrace) {
                return Err(ServerRelError::at(
                    "SRV1007",
                    start,
                    "REL declaration is missing a body",
                ));
            }
            self.advance();
        }
        let mut depth = 0usize;
        loop {
            match self.current().kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        self.advance();
                        return Ok(());
                    }
                }
                TokenKind::Eof => {
                    return Err(ServerRelError::at(
                        "SRV1007",
                        start,
                        "REL declaration is missing closing `}`",
                    ));
                }
                _ => {}
            }
            self.advance();
        }
    }

    fn expect_atom(&mut self, expected: &str) -> Result<(), ServerRelError> {
        let token = self.current().clone();
        match &token.kind {
            TokenKind::Atom(value) if value == expected => {
                self.advance();
                Ok(())
            }
            _ => Err(ServerRelError::at(
                "SRV1002",
                &token,
                format!("expected `{expected}`"),
            )),
        }
    }

    fn take_atom(&mut self, message: &str) -> Result<String, ServerRelError> {
        let token = self.current().clone();
        match token.kind {
            TokenKind::Atom(value) => {
                self.advance();
                Ok(value)
            }
            _ => Err(ServerRelError::at("SRV1002", &token, message)),
        }
    }

    fn expect(&mut self, expected: TokenKind, message: &str) -> Result<(), ServerRelError> {
        if self.check(&expected) {
            self.advance();
            Ok(())
        } else {
            Err(ServerRelError::at("SRV1002", self.current(), message))
        }
    }

    fn peek_atom(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.index + 1).map(|token| &token.kind),
            Some(TokenKind::Atom(value)) if value == expected
        )
    }

    fn check(&self, expected: &TokenKind) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(expected)
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn advance(&mut self) {
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
    }
}

fn parse_atom_value(value: String) -> Value {
    if let Ok(number) = value.parse::<i64>() {
        return Value::Number(Number::from(number));
    }
    if let Ok(number) = value.parse::<u64>() {
        return Value::Number(Number::from(number));
    }
    if let Ok(number) = value.parse::<f64>() {
        if let Some(number) = Number::from_f64(number) {
            return Value::Number(number);
        }
    }
    Value::String(value)
}

fn insert_unique(
    output: &mut BTreeMap<String, Value>,
    key: String,
    value: Value,
    token: &Token,
) -> Result<(), ServerRelError> {
    if output.insert(key.clone(), value).is_some() {
        return Err(ServerRelError::at(
            "SRV1008",
            token,
            format!("duplicate Server REL policy key {key:?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_server_policy_and_native_middleware() {
        let policy = compile_server_source(
            r#"
            server Main {
                status maintenance;
                env {
                    APP_NAME "RBE";
                    SESSION_TTL 86400;
                    DEBUG false;
                }
                api {
                    host "127.0.0.1";
                    port 8080;
                }
                force {
                    api { port 9090; }
                    env { REGION "locked"; }
                }
                middleware {
                    correlationId;
                    json {
                        limit 2mb;
                        strict true;
                    }
                    compression {
                        threshold 1kb;
                        algorithms [br, gzip, zstd];
                    }
                }
            }
            "#,
        )
        .expect("server policy should compile");

        assert_eq!(policy.name, "Main");
        assert_eq!(policy.status, ServerStatus::Maintenance);
        assert_eq!(policy.environment["APP_NAME"], Value::String("RBE".into()));
        assert_eq!(policy.environment["SESSION_TTL"], Value::from(86400));
        assert_eq!(policy.defaults["api.port"], Value::from(8080));
        assert_eq!(policy.forced["api.port"], Value::from(9090));
        assert_eq!(
            policy.forced_environment["REGION"],
            Value::String("locked".into())
        );
        assert_eq!(policy.middleware.len(), 3);
        assert_eq!(policy.middleware[0].name, "correlationId");
        assert_eq!(
            policy.middleware[1].options["limit"],
            Value::String("2mb".into())
        );
    }

    #[test]
    fn extracts_embedded_rel_sources_with_stable_ids() {
        let policy = compile_server_source(
            r#"
            [file-start:module.Auth]
            :import[ENV]
            export function appName() { return ENV.get("APP_NAME"); }
            [file-end:module]

            [file-start:route.Health path="/health"]
            class Route { get() { return { ok: true }; } }
            [file-end:route]

            server Main { status online; }
            "#,
        )
        .expect("embedded sources should compile");

        assert_eq!(policy.embedded_sources.len(), 2);
        assert_eq!(
            policy.embedded_sources[0].source_id,
            "server.server#module:Auth"
        );
        assert_eq!(
            policy.embedded_sources[1].attributes["path"],
            "/health"
        );
        assert!(policy.embedded_sources[0].source.contains(":import[ENV]"));
    }

    #[test]
    fn rejects_duplicate_embedded_source_identity() {
        let error = compile_server_source(
            r#"
            [file-start:module.Auth]
            export function one() { return 1; }
            [file-end:module]
            [file-start:module.Auth]
            export function two() { return 2; }
            [file-end:module]
            server Main {}
            "#,
        )
        .expect_err("duplicate source identity should fail");
        assert_eq!(error.code, "SRV1013");
    }

    #[test]
    fn rejects_unknown_native_middleware() {
        let error = compile_server_source(
            "server Main { middleware { definitelyNotNative; } }",
        )
        .expect_err("unknown middleware should fail");
        assert_eq!(error.code, "SRV1005");
    }

    #[test]
    fn rejects_unknown_server_status() {
        let error = compile_server_source("server Main { status sleepy; }")
            .expect_err("unknown status should fail");
        assert_eq!(error.code, "SRV1004");
    }

    #[test]
    fn permits_rel_helpers_without_misreading_their_bodies_as_policy() {
        let policy = compile_server_source(
            r#"
            server Main {
                function normalize(value) {
                    if (value) { return value; }
                    return "default";
                }
                class Helpers {
                    run(value) { return normalize(value); }
                }
                status online;
            }
            "#,
        )
        .expect("helper declarations should not break policy compilation");
        assert_eq!(policy.status, ServerStatus::Online);
    }
}
