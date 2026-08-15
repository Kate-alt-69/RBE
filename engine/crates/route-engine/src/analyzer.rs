//! Static semantic analysis for `.route` files.
//!
//! The parser answers "is this syntactically valid?". The analyzer answers
//! "can this program make sense under the route language's name-resolution
//! rules?" before the interpreter/transpiler gets involved.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement};
use crate::modules::{binding_name, route_capability_allowed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub symbol: Option<String>,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Self { Self { severity: Severity::Error, message: message.into(), symbol: None } }
    fn error_symbol(message: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self { severity: Severity::Error, message: message.into(), symbol: Some(symbol.into()) }
    }
    fn warning(message: impl Into<String>) -> Self { Self { severity: Severity::Warning, message: message.into(), symbol: None } }
    fn warning_symbol(message: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self { severity: Severity::Warning, message: message.into(), symbol: Some(symbol.into()) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SymbolKind { Local, Module, DirectFunction, Function }

#[derive(Clone)]
struct Symbol { kind: SymbolKind, used: bool }

type Scope = HashMap<String, Symbol>;

fn import_base(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => target.as_ref(),
        _ => import,
    }
}

fn import_source_key(import: &ImportTarget) -> String {
    match import_base(import) {
        ImportTarget::Builtin(module) => format!("builtin:{module}"),
        ImportTarget::BuiltinFunction { module, function } => format!("builtin:{module}.{function}"),
        ImportTarget::Custom(path) => format!("custom:{path}"),
        ImportTarget::CustomFunction { path, function } => format!("custom:{path}.{function}"),
        ImportTarget::Aliased { .. } => unreachable!("import_base removes aliased import wrappers"),
    }
}

pub fn analyze(file: &RouteFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut globals = Scope::new();
    let mut imported_sources = HashSet::new();

    for import in &file.imports {
        let name = binding_name(import);
        let source_key = import_source_key(import);
        if !imported_sources.insert(source_key.clone()) {
            diagnostics.push(Diagnostic::error_symbol(
                format!("duplicate import source `{source_key}`; a capability may only be imported once per file"),
                name.clone(),
            ));
        }

        let kind = match import_base(import) {
            ImportTarget::Builtin(module) => {
                if !route_capability_allowed(module) {
                    diagnostics.push(Diagnostic::error_symbol(
                        format!("capability `{module}` is not available to `.route` files"),
                        name.clone(),
                    ));
                }
                SymbolKind::Module
            }
            ImportTarget::BuiltinFunction { module, .. } => {
                if !route_capability_allowed(module) {
                    diagnostics.push(Diagnostic::error_symbol(
                        format!("capability `{module}` is not available to `.route` files"),
                        name.clone(),
                    ));
                }
                SymbolKind::DirectFunction
            }
            ImportTarget::Custom(_) | ImportTarget::CustomFunction { .. } => SymbolKind::Module,
            ImportTarget::Aliased { .. } => unreachable!("import_base removes aliased import wrappers"),
        };
        if globals.insert(name.clone(), Symbol { kind, used: false }).is_some() {
            diagnostics.push(Diagnostic::error_symbol(format!("duplicate import binding `{name}`"), name));
        }
    }

    for function in &file.functions {
        if globals.insert(function.name.clone(), Symbol { kind: SymbolKind::Function, used: false }).is_some() {
            diagnostics.push(Diagnostic::error_symbol(format!("duplicate top-level function `{}`", function.name), function.name.clone()));
        }
    }

    let mut used_globals = HashSet::new();
    for function in &file.functions {
        analyze_function(function, &globals, &mut used_globals, &mut diagnostics);
    }
    for method in &file.methods {
        analyze_method(method, &globals, &mut used_globals, &mut diagnostics);
    }

    for (name, symbol) in globals {
        if used_globals.contains(&name) { continue; }
        match symbol.kind {
            SymbolKind::Module | SymbolKind::DirectFunction => {
                diagnostics.push(Diagnostic::warning_symbol(format!("import `{name}` is never used"), name));
            }
            SymbolKind::Function => {
                diagnostics.push(Diagnostic::warning_symbol(format!("function `{name}` is never called"), name));
            }
            SymbolKind::Local => {}
        }
    }

    diagnostics
}

fn analyze_function(function: &FunctionDef, globals: &Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>) {
    let mut scope = globals.clone();
    for param in &function.params {
        if scope.contains_key(param) {
            diagnostics.push(Diagnostic::error_symbol(format!("function `{}` parameter `{param}` shadows an imported or top-level symbol", function.name), param));
        } else {
            scope.insert(param.clone(), Symbol { kind: SymbolKind::Local, used: false });
        }
    }
    analyze_statements(&function.body, &mut scope, used_globals, diagnostics, &function.name);
    warn_unused_locals(&scope, diagnostics, Some(&function.name));
}

fn analyze_method(method: &MethodDef, globals: &Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>) {
    let mut scope = globals.clone();
    if let Some(param) = &method.param_name {
        if scope.contains_key(param) {
            diagnostics.push(Diagnostic::error_symbol(format!("route `{}` parameter `{param}` shadows an imported or top-level symbol", method.verb), param));
        } else {
            scope.insert(param.clone(), Symbol { kind: SymbolKind::Local, used: false });
        }
    }
    analyze_statements(&method.body, &mut scope, used_globals, diagnostics, &method.verb);
    warn_unused_locals(&scope, diagnostics, Some(&method.verb));
}

fn analyze_statements(statements: &[Statement], scope: &mut Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>, owner: &str) {
    for statement in statements {
        match statement {
            Statement::Const { name, value } => {
                analyze_expr(value, scope, used_globals, diagnostics);
                if scope.contains_key(name) {
                    diagnostics.push(Diagnostic::error_symbol(format!("`{owner}` redeclares `{name}` in the same scope"), name));
                } else {
                    scope.insert(name.clone(), Symbol { kind: SymbolKind::Local, used: false });
                }
            }
            Statement::Return(expr) | Statement::Expr(expr) => analyze_expr(expr, scope, used_globals, diagnostics),
            Statement::If { condition, then_body, else_body } => {
                analyze_expr(condition, scope, used_globals, diagnostics);
                let mut then_scope = scope.clone();
                analyze_statements(then_body, &mut then_scope, used_globals, diagnostics, owner);
                let mut else_scope = scope.clone();
                analyze_statements(else_body, &mut else_scope, used_globals, diagnostics, owner);
            }
        }
    }
}

fn mark_identifier(name: &str, scope: &mut Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>, allow_module_value: bool) {
    match scope.get_mut(name) {
        Some(symbol) => {
            symbol.used = true;
            match symbol.kind {
                SymbolKind::Local => {}
                SymbolKind::Module if allow_module_value => { used_globals.insert(name.to_string()); }
                SymbolKind::Module => diagnostics.push(Diagnostic::error_symbol(format!("`{name}` is a module and must be accessed through an exported capability"), name)),
                SymbolKind::DirectFunction | SymbolKind::Function => {
                    used_globals.insert(name.to_string());
                    diagnostics.push(Diagnostic::error_symbol(format!("`{name}` is callable metadata, not a value; call it instead"), name));
                }
            }
        }
        None => diagnostics.push(Diagnostic::error_symbol(format!("`{name}` is not defined"), name)),
    }
}

fn analyze_expr(expr: &Expr, scope: &mut Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>) {
    match expr {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => {}
        Expr::Ident(name) => mark_identifier(name, scope, used_globals, diagnostics, false),
        Expr::Member(base, _) => {
            if let Expr::Ident(name) = base.as_ref() {
                match scope.get_mut(name) {
                    Some(symbol) if symbol.kind == SymbolKind::Module => {
                        symbol.used = true;
                        used_globals.insert(name.clone());
                    }
                    Some(_) => mark_identifier(name, scope, used_globals, diagnostics, false),
                    None => diagnostics.push(Diagnostic::error_symbol(format!("module `{name}` is not imported"), name)),
                }
            } else {
                analyze_expr(base, scope, used_globals, diagnostics);
            }
        }
        Expr::Call(callee, args) => {
            match callee.as_ref() {
                Expr::Ident(name) => match scope.get_mut(name) {
                    Some(symbol) => {
                        symbol.used = true;
                        match symbol.kind {
                            SymbolKind::Function | SymbolKind::DirectFunction => { used_globals.insert(name.clone()); }
                            SymbolKind::Local | SymbolKind::Module => diagnostics.push(Diagnostic::error_symbol(format!("`{name}` is not callable"), name)),
                        }
                    }
                    None => diagnostics.push(Diagnostic::error_symbol(format!("function `{name}` is not defined"), name)),
                },
                Expr::Member(base, _) => {
                    if let Expr::Ident(name) = base.as_ref() {
                        match scope.get_mut(name) {
                            Some(symbol) if symbol.kind == SymbolKind::Module => {
                                symbol.used = true;
                                used_globals.insert(name.clone());
                            }
                            Some(_) => mark_identifier(name, scope, used_globals, diagnostics, false),
                            None => diagnostics.push(Diagnostic::error_symbol(format!("module `{name}` is not imported"), name)),
                        }
                    } else {
                        analyze_expr(base, scope, used_globals, diagnostics);
                    }
                }
                _ => analyze_expr(callee, scope, used_globals, diagnostics),
            }
            for arg in args { analyze_expr(arg, scope, used_globals, diagnostics); }
        }
        Expr::Object(fields) => for (_, value) in fields { analyze_expr(value, scope, used_globals, diagnostics); },
        Expr::Array(items) => for item in items { analyze_expr(item, scope, used_globals, diagnostics); },
        Expr::UnaryNot(inner) => analyze_expr(inner, scope, used_globals, diagnostics),
        Expr::Binary { left, right, .. } => {
            analyze_expr(left, scope, used_globals, diagnostics);
            analyze_expr(right, scope, used_globals, diagnostics);
        }
    }
}

fn warn_unused_locals(scope: &Scope, diagnostics: &mut Vec<Diagnostic>, owner: Option<&str>) {
    for (name, symbol) in scope {
        if symbol.kind == SymbolKind::Local && !symbol.used && name != "_req" {
            let owner = owner.map(|value| format!(" in `{value}`")).unwrap_or_default();
            diagnostics.push(Diagnostic::warning_symbol(format!("local `{name}` is never used{owner}"), name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    #[test]
    fn catches_missing_name_and_reports_unused_import() {
        let file = parse(r#"
            :import[net]
            class Route { get(req) {
                const value = missing;
                return req.path;
            }}
        "#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Error && d.message.contains("missing")));
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Warning && d.message.contains("net")));
        assert!(diagnostics.iter().any(|d| d.symbol.as_deref() == Some("missing")));
    }

    #[test]
    fn rejects_module_only_route_capability() {
        let file = parse(r#":import[env] class Route { get(req) { return req.path; } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| {
            d.severity == Severity::Error
                && d.symbol.as_deref() == Some("env")
                && d.message.contains("not available to `.route` files")
        }));
    }

    #[test]
    fn rejects_duplicate_import_source_even_with_aliases() {
        let file = parse(r#":import[net as first, net as second] class Route { get(req) { return first.ping(); } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| {
            d.severity == Severity::Error
                && d.message.contains("duplicate import source")
                && d.symbol.as_deref() == Some("second")
        }));
    }

    #[test]
    fn aliased_capability_preserves_local_binding() {
        let target = ImportTarget::Aliased {
            target: Box::new(ImportTarget::Builtin("net".into())),
            alias: "network".into(),
        };
        let file = RouteFile {
            imports: vec![target],
            functions: Vec::new(),
            class_name: "Route".into(),
            methods: vec![MethodDef {
                verb: "get".into(),
                param_name: Some("req".into()),
                body: vec![Statement::Return(Expr::Call(
                    Box::new(Expr::Member(Box::new(Expr::Ident("network".into())), "ping".into())),
                    Vec::new(),
                ))],
            }],
        };
        assert!(analyze(&file).iter().all(|d| d.severity != Severity::Error));
    }

    #[test]
    fn member_access_marks_request_as_used() {
        let file = parse(r#"class Route { get(req) { return req.path; } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().all(|d| !d.message.contains("req")));
    }

    #[test]
    fn used_import_is_not_reported_unused() {
        let file = parse(r#":import[net] class Route { get(req) { return net.ping(); } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().all(|d| !d.message.contains("import `net` is never used")));
        assert!(diagnostics.iter().all(|d| d.severity != Severity::Error));
    }

    #[test]
    fn accepts_function_and_direct_import_calls() {
        let file = parse(r#":import[net.ping] function wrap(value) { return { value: value }; } class Route { get(req) { const pong = ping(); return wrap(pong); } }"#);
        assert!(analyze(&file).iter().all(|d| d.severity != Severity::Error));
    }
}
