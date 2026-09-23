//! Recursive-descent / precedence parser for the RBE `.route` language.
//! The parser is intentionally strict and reports line/column information.

use std::collections::HashMap;

use crate::ast::{
    BinaryOp, Expr, FieldBinding, FieldBindingMode, FieldDirective, FieldFile, FieldValueType,
    FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceClassDef, ServiceProgram,
    Statement, Value,
};
use crate::lexer::{Token, TokenKind};

const KNOWN_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];
const RESERVED_INLINE_FIELD_NAMES: &[&str] = &["required", "optional", "has", "dynamic"];

fn import_contains_service(import: &ImportTarget) -> bool {
    match import {
        ImportTarget::Service(_) | ImportTarget::ServiceFunction { .. } => true,
        ImportTarget::Aliased { target, .. } => import_contains_service(target),
        _ => false,
    }
}

fn import_is_builtin(import: &ImportTarget, expected: &str) -> bool {
    match import {
        ImportTarget::Builtin(name) | ImportTarget::BuiltinFunction { module: name, .. } => {
            name == expected
        }
        ImportTarget::Aliased { target, .. } => import_is_builtin(target, expected),
        _ => false,
    }
}

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

    pub fn parse_field_file(mut self) -> Result<FieldFile, ParseError> {
        let mut imports = Vec::new();
        let mut directive = None;

        while self.check(&TokenKind::Colon) {
            if self.is_import_directive() {
                imports.extend(self.parse_imports()?);
                continue;
            }
            if directive.is_some() {
                return Err(self.error_here("duplicate :field[...] declaration"));
            }
            directive = Some(self.parse_field_directive()?);
        }

        if directive.is_none() && self.is_ident("field") {
            directive = Some(self.parse_field_block_directive()?);
        }
        let directive = directive.ok_or_else(|| {
            self.error_here(".field source requires :field[...] or field { ... } metadata")
        })?;

        let mut bindings = Vec::new();
        let mut resolver = None;
        if self.is_ident("resolve") {
            self.advance();
            if self.check(&TokenKind::LBrace) {
                self.advance();
                while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
                    let name = self.expect_ident()?;
                    self.expect(TokenKind::Eq)?;
                    let binding = self.parse_field_binding(name)?;
                    self.expect(TokenKind::Semicolon)?;
                    if bindings
                        .iter()
                        .any(|existing: &FieldBinding| existing.name == binding.name)
                    {
                        return Err(self.error_here("duplicate FieldManager binding"));
                    }
                    bindings.push(binding);
                }
                self.expect(TokenKind::RBrace)?;
            } else if self.check(&TokenKind::LParen) {
                let params = self.parse_params()?;
                if params.len() > 2 {
                    return Err(self.error_here(
                        "FieldManager resolve(raw, context) accepts at most two parameters",
                    ));
                }
                let body = self.parse_block()?;
                resolver = Some(FunctionDef {
                    name: "resolve".into(),
                    params,
                    body,
                });
            } else {
                return Err(self.error_here("expected `{` or `(` after FieldManager resolve"));
            }
        }

        if !self.check(&TokenKind::Eof) {
            return Err(self.error_here("unexpected content after FieldManager resolver"));
        }
        if bindings.is_empty() && resolver.is_none() && directive.key.is_none() {
            return Err(self.error_here(
                "FieldManager source must declare a key, declarative resolve bindings, or resolve(raw, context)",
            ));
        }

        Ok(FieldFile {
            imports,
            directive,
            bindings,
            resolver,
        })
    }

    fn is_ident(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.pos).map(|token| &token.kind),
            Some(TokenKind::Ident(name)) if name == expected
        )
    }

    fn parse_field_directive(&mut self) -> Result<FieldDirective, ParseError> {
        self.expect(TokenKind::Colon)?;
        match self.advance().kind {
            TokenKind::Ident(name) if name == "field" => {}
            other => {
                return Err(
                    self.error_here(&format!("expected :field[...] declaration, got {other:?}"))
                );
            }
        }
        self.expect(TokenKind::LBracket)?;
        let mut directive = FieldDirective {
            source: "query".into(),
            key: None,
            optional: false,
            value_type: FieldValueType::String,
        };
        while !self.check(&TokenKind::RBracket) {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            self.apply_field_directive_option(&mut directive, &name)?;
            if self.check(&TokenKind::Comma) {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    return Err(self.error_here("trailing commas are not allowed in :field[...]"));
                }
            } else if !self.check(&TokenKind::RBracket) {
                return Err(self.error_here("expected `,` between :field[...] options"));
            }
        }
        self.expect(TokenKind::RBracket)?;
        Ok(directive)
    }

    fn parse_field_block_directive(&mut self) -> Result<FieldDirective, ParseError> {
        let name = self.expect_ident()?;
        debug_assert_eq!(name, "field");
        self.expect(TokenKind::LBrace)?;
        let mut directive = FieldDirective {
            source: "query".into(),
            key: None,
            optional: false,
            value_type: FieldValueType::String,
        };
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let option = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            self.apply_field_directive_option(&mut directive, &option)?;
            self.expect(TokenKind::Semicolon)?;
        }
        self.expect(TokenKind::RBrace)?;
        Ok(directive)
    }

    fn apply_field_directive_option(
        &mut self,
        directive: &mut FieldDirective,
        name: &str,
    ) -> Result<(), ParseError> {
        match name {
            "source" => {
                let source = self.expect_ident()?;
                if source != "query" {
                    return Err(
                        self.error_here("FieldManager source currently supports only `query`")
                    );
                }
                directive.source = source;
            }
            "key" => match self.advance().kind {
                TokenKind::String(value) if !value.is_empty() => directive.key = Some(value),
                other => {
                    return Err(self.error_here(&format!(
                        "FieldManager key must be a non-empty string, got {other:?}"
                    )));
                }
            },
            "optional" => {
                directive.optional = match self.advance().kind {
                    TokenKind::True => true,
                    TokenKind::False => false,
                    other => {
                        return Err(self.error_here(&format!(
                            "FieldManager optional must be true or false, got {other:?}"
                        )));
                    }
                };
            }
            "required" => {
                let required = match self.advance().kind {
                    TokenKind::True => true,
                    TokenKind::False => false,
                    other => {
                        return Err(self.error_here(&format!(
                            "FieldManager required must be true or false, got {other:?}"
                        )));
                    }
                };
                directive.optional = !required;
            }
            "type" => directive.value_type = self.parse_field_type()?,
            other => {
                return Err(
                    self.error_here(&format!("unknown FieldManager metadata option {other:?}"))
                );
            }
        }
        Ok(())
    }

    fn parse_field_type(&mut self) -> Result<FieldValueType, ParseError> {
        match self.expect_ident()?.as_str() {
            "string" => Ok(FieldValueType::String),
            "int" => Ok(FieldValueType::Int),
            "bool" => Ok(FieldValueType::Bool),
            other => Err(self.error_here(&format!(
                "unknown FieldManager type {other:?}; expected string, int, or bool"
            ))),
        }
    }

    fn parse_field_binding(&mut self, name: String) -> Result<FieldBinding, ParseError> {
        let mode = match self.expect_ident()?.as_str() {
            "required" => FieldBindingMode::Required,
            "optional" => FieldBindingMode::Optional,
            "dynamic" => FieldBindingMode::Dynamic,
            other => {
                return Err(self.error_here(&format!(
                    "unknown FieldManager resolver {other:?}; expected required, optional, or dynamic"
                )));
            }
        };
        self.expect(TokenKind::LParen)?;
        let lookup = match self.advance().kind {
            TokenKind::String(value) if !value.is_empty() => value,
            other => {
                return Err(self.error_here(&format!(
                    "FieldManager lookup key/prefix must be a non-empty string, got {other:?}"
                )));
            }
        };
        let mut value_type = FieldValueType::String;
        let mut default = None;
        let mut strip_prefix = false;
        while self.check(&TokenKind::Comma) {
            self.advance();
            let option = self.expect_ident()?;
            self.expect(TokenKind::Eq)?;
            match option.as_str() {
                "type" => value_type = self.parse_field_type()?,
                "default" => {
                    let expr = self.parse_expression()?;
                    default = Some(self.bound_constant_value(&expr)?);
                }
                "stripPrefix" => {
                    strip_prefix = match self.advance().kind {
                        TokenKind::True => true,
                        TokenKind::False => false,
                        other => {
                            return Err(self.error_here(&format!(
                                "stripPrefix must be true or false, got {other:?}"
                            )));
                        }
                    };
                }
                other => {
                    return Err(
                        self.error_here(&format!("unknown FieldManager resolver option {other:?}"))
                    );
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        if mode == FieldBindingMode::Required && default.is_some() {
            return Err(self.error_here("required(...) cannot declare a default"));
        }
        if mode == FieldBindingMode::Dynamic
            && (default.is_some() || value_type != FieldValueType::String)
        {
            return Err(self.error_here(
                "dynamic(...) supports stripPrefix only; dynamic values stay strings",
            ));
        }
        Ok(FieldBinding {
            name,
            lookup,
            mode,
            value_type,
            default,
            strip_prefix,
        })
    }

    pub fn parse_service_file(mut self) -> Result<ServiceProgram, ParseError> {
        let mut imports = Vec::new();
        while self.is_import_directive() {
            imports.extend(self.parse_imports()?);
        }

        self.parse_service_directive()?;

        let mut functions = Vec::new();
        let mut exports = Vec::new();
        let mut classes = Vec::new();
        let mut class_name = None;
        let mut lifecycle = Vec::new();

        while !self.check(&TokenKind::Eof) {
            if self.check(&TokenKind::Class) {
                let Some(TokenKind::Ident(next_name)) = self
                    .tokens
                    .get(self.pos + 1)
                    .map(|token| token.kind.clone())
                else {
                    return Err(self.error_here("expected class name after `class`"));
                };

                if next_name == "Service" {
                    if class_name.is_some() {
                        return Err(self.error_here("duplicate `class Service` in .service file"));
                    }
                    let (name, methods) = self.parse_service_class()?;
                    class_name = Some(name);
                    lifecycle = methods;
                } else {
                    let class = self.parse_service_namespace_class()?;
                    if classes
                        .iter()
                        .any(|existing: &ServiceClassDef| existing.name == class.name)
                    {
                        return Err(self.error_here(&format!(
                            "duplicate service-local class {:?}",
                            class.name
                        )));
                    }
                    classes.push(class);
                }
                continue;
            }

            let exported = self.is_export_keyword();
            if exported {
                self.advance();
            }
            if self.check(&TokenKind::Async) {
                self.advance();
            }
            if !self.check(&TokenKind::Function) {
                return Err(self.error_here(
                    "expected `function`, `export function`, or `class` in .service file",
                ));
            }
            let function = self.parse_function()?;
            if functions
                .iter()
                .any(|existing: &FunctionDef| existing.name == function.name)
            {
                return Err(
                    self.error_here(&format!("duplicate service function {:?}", function.name))
                );
            }
            if exported {
                if exports.iter().any(|name| name == &function.name) {
                    return Err(
                        self.error_here(&format!("duplicate service export {:?}", function.name))
                    );
                }
                exports.push(function.name.clone());
            }
            functions.push(function);
        }

        let quick_db_imported = imports
            .iter()
            .any(|import| import_is_builtin(import, "quickDB"));
        if !quick_db_imported
            && classes
                .iter()
                .any(|class| class.bindings.contains_key("set"))
        {
            return Err(self.error_here(
                "service-local classes using `const <= set => ...` require `:import[quickDB]`",
            ));
        }

        Ok(ServiceProgram {
            imports,
            functions,
            exports,
            class_name,
            lifecycle,
            classes,
        })
    }

    fn is_import_directive(&self) -> bool {
        self.check(&TokenKind::Colon)
            && matches!(
                self.tokens.get(self.pos + 1).map(|token| &token.kind),
                Some(TokenKind::Import)
            )
    }

    fn parse_service_directive(&mut self) -> Result<(), ParseError> {
        self.expect(TokenKind::Colon)?;
        match self.advance().kind {
            TokenKind::Ident(name) if name == "service" => {}
            other => {
                return Err(self.error_here(&format!(
                    "expected :service[...] declaration, got {other:?}"
                )));
            }
        }
        self.expect(TokenKind::LBracket)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.advance().kind {
                TokenKind::LBracket => depth = depth.saturating_add(1),
                TokenKind::RBracket => depth -= 1,
                TokenKind::Eof => {
                    return Err(self.error_here("unterminated :service[...] declaration"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_service_class(&mut self) -> Result<(String, Vec<MethodDef>), ParseError> {
        const LIFECYCLE: &[&str] = &["start", "event", "health", "stop"];

        self.expect(TokenKind::Class)?;
        let name = self.expect_ident()?;
        if name != "Service" {
            return Err(self.error_here(".service lifecycle class must be named `Service`"));
        }
        self.expect(TokenKind::LBrace)?;

        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.check(&TokenKind::Async) {
                self.advance();
            }
            let method_name = self.expect_ident()?;
            let lifecycle_name = method_name.to_ascii_lowercase();
            if !LIFECYCLE.contains(&lifecycle_name.as_str()) {
                return Err(self.error_here(&format!(
                "unsupported Service lifecycle method {method_name:?}; expected one of {LIFECYCLE:?}"
            )));
            }
            if methods
                .iter()
                .any(|method: &MethodDef| method.verb == lifecycle_name)
            {
                return Err(self.error_here(&format!(
                    "duplicate Service lifecycle method {lifecycle_name:?}"
                )));
            }
            let params = self.parse_params()?;
            if params.len() > 1 {
                return Err(self.error_here(
                    "Service lifecycle methods currently accept zero or one parameter",
                ));
            }
            let body = self.parse_block()?;
            methods.push(MethodDef {
                verb: lifecycle_name,
                param_name: params.into_iter().next(),
                body,
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok((name, methods))
    }

    fn parse_service_namespace_class(&mut self) -> Result<ServiceClassDef, ParseError> {
        self.expect(TokenKind::Class)?;
        let name = self.expect_ident()?;
        if name == "Service" {
            return Err(self.error_here("`Service` is reserved for the .service lifecycle class"));
        }
        self.expect(TokenKind::LBrace)?;

        let mut bindings = HashMap::new();
        let mut methods = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            if self.check(&TokenKind::Const) {
                self.advance();
                self.expect(TokenKind::LtEq)?;
                let binding = self.expect_ident()?;
                // `=>` intentionally reuses the existing Eq + Gt tokens so
                // this class-only declarative syntax does not alter ordinary
                // expression parsing.
                self.expect(TokenKind::Eq)?;
                self.expect(TokenKind::Gt)?;
                let expr = self.parse_expression()?;
                self.expect(TokenKind::Semicolon)?;
                let value = self.bound_constant_value(&expr)?;
                if bindings.insert(binding.clone(), value).is_some() {
                    return Err(self.error_here(&format!(
                        "duplicate bound constant {binding:?} in class {name:?}"
                    )));
                }
                continue;
            }

            if self.check(&TokenKind::Async) {
                self.advance();
            }
            let function = if self.check(&TokenKind::Function) {
                self.parse_function()?
            } else {
                let method_name = self.expect_ident()?;
                let params = self.parse_params()?;
                let body = self.parse_block()?;
                FunctionDef {
                    name: method_name,
                    params,
                    body,
                }
            };
            if methods
                .iter()
                .any(|existing: &FunctionDef| existing.name == function.name)
            {
                return Err(self.error_here(&format!(
                    "duplicate class method {:?} in class {name:?}",
                    function.name
                )));
            }
            methods.push(function);
        }
        self.expect(TokenKind::RBrace)?;

        Ok(ServiceClassDef {
            name,
            bindings,
            methods,
        })
    }

    fn bound_constant_value(&self, expr: &Expr) -> Result<Value, ParseError> {
        match expr {
            Expr::String(value) => Ok(Value::String(value.clone())),
            Expr::Number(value) => Ok(Value::Number(*value)),
            Expr::Bool(value) => Ok(Value::Bool(*value)),
            Expr::Null => Ok(Value::Null),
            Expr::Array(items) => items
                .iter()
                .map(|item| self.bound_constant_value(item))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Expr::Object(fields) => {
                let mut out = HashMap::new();
                for (key, value) in fields {
                    out.insert(key.clone(), self.bound_constant_value(value)?);
                }
                Ok(Value::Object(out))
            }
            _ => {
                Err(self
                    .error_here("class bound constants must be literal values, arrays, or objects"))
            }
        }
    }

    /// Parse a route while retaining recoverable statement errors. A valid
    /// AST is returned when the file structure can still be reconstructed;
    /// callers can render every recovered diagnostic instead of stopping at
    /// the first broken statement.
    pub fn parse_file_collecting(mut self) -> (Option<RouteFile>, Vec<ParseError>) {
        let mut errors = Vec::new();
        let mut imports = Vec::new();
        let mut field_bindings = Vec::new();
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

        if self.is_ident("fields") {
            match self.parse_route_fields_block() {
                Ok(bindings) => field_bindings = bindings,
                Err(error) => {
                    errors.push(error);
                    self.recover_top_level();
                }
            }
        }
        if self.is_ident("fields") {
            errors.push(self.error_here("duplicate route-local `fields { ... }` block"));
            self.recover_top_level();
        }
        if !field_bindings.is_empty()
            && !imports
                .iter()
                .any(|import| matches!(import, ImportTarget::Builtin(module) if module == "field"))
        {
            errors.push(self.error_here(
                "route-local `fields { ... }` requires the direct `:import[field]` namespace import",
            ));
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
                field_bindings,
                functions,
                class_name,
                methods,
            }),
            errors,
        )
    }

    fn parse_route_fields_block(&mut self) -> Result<Vec<FieldBinding>, ParseError> {
        let keyword = self.expect_ident()?;
        debug_assert_eq!(keyword, "fields");
        self.expect(TokenKind::LBrace)?;

        let mut bindings = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            let name = self.expect_ident()?;
            if RESERVED_INLINE_FIELD_NAMES.contains(&name.as_str()) {
                return Err(self.error_here(&format!(
                    "route-local FieldManager name {name:?} is reserved by the direct field namespace"
                )));
            }
            self.expect(TokenKind::Eq)?;
            let binding = self.parse_field_binding(name)?;
            self.expect(TokenKind::Semicolon)?;
            if bindings
                .iter()
                .any(|existing: &FieldBinding| existing.name == binding.name)
            {
                return Err(self.error_here("duplicate route-local FieldManager binding"));
            }
            bindings.push(binding);
        }
        self.expect(TokenKind::RBrace)?;
        if bindings.is_empty() {
            return Err(self.error_here("route-local `fields { ... }` cannot be empty"));
        }
        Ok(bindings)
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
                if self.is_from_keyword() {
                    self.advance();
                    let module = self.expect_ident()?;
                    Ok(ImportTarget::BuiltinSubLibrary {
                        module,
                        library: name,
                    })
                } else if name == "field" && self.check(&TokenKind::Colon) {
                    self.advance();
                    let field = self.expect_ident()?;
                    Ok(ImportTarget::BuiltinFunction {
                        module: "field".into(),
                        function: field,
                    })
                } else if name == "service" && self.check(&TokenKind::Colon) {
                    self.advance();
                    let service = self.expect_ident()?;
                    if self.check(&TokenKind::Dot) {
                        self.advance();
                        let function = self.expect_ident()?;
                        Ok(ImportTarget::ServiceFunction { service, function })
                    } else {
                        Ok(ImportTarget::Service(service))
                    }
                } else if self.check(&TokenKind::Dot) {
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
                if let Some(service) = path.strip_prefix("service:") {
                    if service.is_empty() {
                        return Err(self.error_here("service import name cannot be empty"));
                    }
                    if self.check(&TokenKind::Dot) {
                        self.advance();
                        let function = self.expect_ident()?;
                        Ok(ImportTarget::ServiceFunction {
                            service: service.to_string(),
                            function,
                        })
                    } else {
                        Ok(ImportTarget::Service(service.to_string()))
                    }
                } else if self.check(&TokenKind::RBracket)
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
            "expected builtin, module path, or service import inside :import[...], got {other:?}"
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

    fn is_from_keyword(&self) -> bool {
        matches!(self.tokens.get(self.pos).map(|token| &token.kind), Some(TokenKind::Ident(name)) if name == "from")
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
                TokenKind::LBracket => {
                    let Expr::Ident(descriptor) = &expr else {
                        break;
                    };
                    if !matches!(descriptor.as_str(), "encode" | "data" | "write" | "level") {
                        break;
                    }
                    let descriptor = descriptor.clone();
                    self.advance();
                    let value = self.parse_expression()?;
                    self.expect(TokenKind::RBracket)?;
                    expr = Expr::Object(vec![
                        ("__rbeStorageDescriptor".into(), Expr::String(descriptor)),
                        ("value".into(), value),
                    ]);
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
            TokenKind::ProjectPath(path) => Ok(Expr::String(path)),
            TokenKind::Dollar => {
                self.expect(TokenKind::LBracket)?;
                let name = self.expect_ident()?;
                self.expect(TokenKind::RBracket)?;
                Ok(Expr::Ident(name))
            }
            TokenKind::LParen => {
                let expr = self.parse_expression()?;
                self.expect(TokenKind::RParen)?;
                Ok(expr)
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
