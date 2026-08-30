//! Static semantic analysis for `.route` files.
//!
//! The parser answers "is this syntactically valid?". The analyzer answers
//! "can this program make sense under the route language's name-resolution
//! rules?" before the interpreter/transpiler gets involved.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement};
use crate::modules::{binding_name, builtin_function_exists, route_capability_allowed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub symbol: Option<String>,
}

impl Diagnostic {
    fn warning_symbol(message: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self { severity: Severity::Warning, code: "W0002", message: message.into(), symbol: Some(symbol.into()) }
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
    let mut route_verbs = HashSet::new();

    for import in &file.imports {
        let name = binding_name(import);
        let source_key = import_source_key(import);
        if !imported_sources.insert(source_key.clone()) {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E3001",
                message: format!("duplicate import source `{source_key}`; a capability may only be imported once per file"),
                symbol: Some(name.clone()),
            });
        }

        let kind = match import_base(import) {
            ImportTarget::Builtin(module) => {
                if !route_capability_allowed(module) {
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E3000",
                        message: format!("capability `{module}` is not available to `.route` files"),
                        symbol: Some(name.clone()),
                    });
                }
                SymbolKind::Module
            }
            ImportTarget::BuiltinFunction { module, function } => {
                if !builtin_function_exists(module, function) {
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E3010",
                        message: format!("{module}.{function} does not exist as a import — please remove `{module}.{function}` from the route file"),
                        symbol: Some(name.clone()),
                    });
                } else if !route_capability_allowed(module) {
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E3000",
                        message: format!("capability `{module}` is not available to `.route` files"),
                        symbol: Some(name.clone()),
                    });
                }
                SymbolKind::DirectFunction
            }
            ImportTarget::Custom(_) | ImportTarget::CustomFunction { .. } => SymbolKind::Module,
            ImportTarget::Aliased { .. } => unreachable!("import_base removes aliased import wrappers"),
        };
        if globals.insert(name.clone(), Symbol { kind, used: false }).is_some() {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2002",
                message: format!("duplicate import binding `{name}`"),
                symbol: Some(name),
            });
        }
    }

    for function in &file.functions {
        if globals.insert(function.name.clone(), Symbol { kind: SymbolKind::Function, used: false }).is_some() {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2002",
                message: format!("duplicate top-level function `{}`", function.name),
                symbol: Some(function.name.clone()),
            });
        }
    }

    for method in &file.methods {
        if !route_verbs.insert(method.verb.as_str()) {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E3012",
                message: format!(
                    "duplicate route method `{}`; each HTTP verb may only be defined once per file",
                    method.verb
                ),
                symbol: Some(method.verb.clone()),
            });
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
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2003",
                message: format!("function `{}` parameter `{param}` shadows an imported or top-level symbol", function.name),
                symbol: Some(param.clone()),
            });
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
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: "E2003",
                message: format!("route `{}` parameter `{param}` shadows an imported or top-level symbol", method.verb),
                symbol: Some(param.clone()),
            });
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
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E2004",
                        message: format!("`{owner}` redeclares `{name}` in the same scope"),
                        symbol: Some(name.clone()),
                    });
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
                SymbolKind::Module => diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: "E2005",
                    message: format!("`{name}` is a module and must be accessed through an exported capability"),
                    symbol: Some(name.to_string()),
                }),
                SymbolKind::DirectFunction | SymbolKind::Function => {
                    used_globals.insert(name.to_string());
                    diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E2006",
                        message: format!("`{name}` is callable metadata, not a value; call it instead"),
                        symbol: Some(name.to_string()),
                    });
                }
            }
        }
        None => diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E2001",
            message: format!("`{name}` is not defined"),
            symbol: Some(name.to_string()),
        }),
    }
}

fn analyze_member_call(module_name: &str, function_name: &str, source_name: Option<&str>, diagnostics: &mut Vec<Diagnostic>) {
    if matches!(module_name, "net" | "private" | "env") && !builtin_function_exists(module_name, function_name) {
        let source = source_name.unwrap_or("the route");
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: "E3011",
            message: format!("{module_name}.{function_name}() does not exist — please remove it from {source}"),
            symbol: Some(format!("{module_name}.{function_name}")),
        });
    }
}

fn analyze_expr(expr: &Expr, scope: &mut Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>) {
    match expr {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => {}
        Expr::Ident(name) => mark_identifier(name, scope, used_globals, diagnostics, false),
        Expr::Member(base, field) => {
            if let Expr::Ident(name) = base.as_ref() {
                match scope.get_mut(name) {
                    Some(symbol) if symbol.kind == SymbolKind::Module => {
                        symbol.used = true;
                        used_globals.insert(name.clone());
                    }
                    Some(_) => mark_identifier(name, scope, used_globals, diagnostics, false),
                    None => diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E2007",
                        message: format!("module `{name}` is not imported"),
                        symbol: Some(name.to_string()),
                    }),
                }
                // Bare member access is not a call; don't reject it here.
                let _ = field;
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
                            SymbolKind::Local | SymbolKind::Module => diagnostics.push(Diagnostic {
                                severity: Severity::Error,
                                code: "E2008",
                                message: format!("`{name}` is not callable"),
                                symbol: Some(name.to_string()),
                            }),
                        }
                    }
                    None => diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "E2009",
                        message: format!("function `{name}` is not defined"),
                        symbol: Some(name.to_string()),
                    }),
                },
                Expr::Member(base, function_name) => {
                    if let Expr::Ident(name) = base.as_ref() {
                        match scope.get_mut(name) {
                            Some(symbol) if symbol.kind == SymbolKind::Module => {
                                symbol.used = true;
                                used_globals.insert(name.clone());
                                analyze_member_call(name, function_name, None, diagnostics);
                            }
                            Some(_) => mark_identifier(name, scope, used_globals, diagnostics, false),
                            None => diagnostics.push(Diagnostic {
                                severity: Severity::Error,
                                code: "E2007",
                                message: format!("module `{name}` is not imported"),
                                symbol: Some(name.to_string()),
                            }),
                        }
                    } else {
                        analyze_expr(callee, scope, used_globals, diagnostics);
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
        assert!(diagnostics.iter().any(|d| d.code == "E2001" && d.message.contains("missing")));
        assert!(diagnostics.iter().any(|d| d.code == "W0002" && d.message.contains("net")));
        assert!(diagnostics.iter().any(|d| d.symbol.as_deref() == Some("missing")));
    }

    #[test]
    fn rejects_module_only_route_capability() {
        let file = parse(r#":import[env] class Route { get(req) { return req.path; } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| {
            d.code == "E3000"
                && d.symbol.as_deref() == Some("env")
                && d.message.contains("not available to `.route` files")
        }));
    }

    #[test]
    fn rejects_duplicate_import_source_even_with_aliases() {
        let file = parse(r#":import[net as first, net as second] class Route { get(req) { return first.ping(); } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| {
            d.code == "E3001"
                && d.message.contains("duplicate import source")
                && d.symbol.as_deref() == Some("second")
        }));
    }

    #[test]
    fn rejects_duplicate_route_methods() {
        let file = parse(
            r#"class Route {
                get(req) { return req.path; }
                get(req) { return req.method; }
            }"#,
        );
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| {
            d.code == "E3012"
                && d.message.contains("duplicate route method `get`")
                && d.symbol.as_deref() == Some("get")
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
    fn unknown_builtin_import_gets_e3010() {
        let file = parse(r#":import[vault.import] class Route { get(req) { return req.path; } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| d.code == "E3010" && d.message.contains("vault.import does not exist as a import")));
    }

    #[test]
    fn unknown_builtin_call_gets_e3011() {
        let file = parse(r#":import[net] class Route { get(req) { return net.health(); } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| d.code == "E3011" && d.message.contains("net.health() does not exist")));
    }

    #[test]
    fn private_health_is_valid() {
        let file = parse(r#":import[private] class Route { get(req) { return private.health(); } }"#);
        assert!(analyze(&file).iter().all(|d| d.severity != Severity::Error));
    }

    #[test]
    fn used_import_is_not_reported_unused() {
        let file = parse(r#":import[net] class Route { get(req) { return net.ping(); } }"#);
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().all(|d| !d.message.contains("import `net` is never used")));
        assert!(diagnostics.iter().all(|d| d.severity != Severity::Error));
    }
}
