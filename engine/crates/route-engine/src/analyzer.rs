//! Static semantic analysis for `.route` files.
//!
//! The parser answers "is this syntactically valid?". The analyzer answers
//! "can this program make sense under the route language's name-resolution
//! rules?" before the interpreter/transpiler gets involved.

use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement};
use crate::modules::binding_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Self {
        Self { severity: Severity::Error, message: message.into() }
    }
    fn warning(message: impl Into<String>) -> Self {
        Self { severity: Severity::Warning, message: message.into() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SymbolKind { Local, Module, DirectFunction, Function }

#[derive(Clone)]
struct Symbol { kind: SymbolKind, used: bool }

type Scope = HashMap<String, Symbol>;

pub fn analyze(file: &RouteFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut globals = Scope::new();

    for import in &file.imports {
        let name = binding_name(import);
        let kind = match import {
            ImportTarget::Builtin(_) | ImportTarget::Custom(_) => SymbolKind::Module,
            ImportTarget::BuiltinFunction { .. } | ImportTarget::CustomFunction { .. } => SymbolKind::DirectFunction,
        };
        if globals.insert(name.clone(), Symbol { kind, used: false }).is_some() {
            diagnostics.push(Diagnostic::error(format!("duplicate import binding `{name}`")));
        }
    }

    for function in &file.functions {
        if globals.insert(function.name.clone(), Symbol { kind: SymbolKind::Function, used: false }).is_some() {
            diagnostics.push(Diagnostic::error(format!("duplicate top-level function `{}`", function.name)));
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
                diagnostics.push(Diagnostic::warning(format!("import `{name}` is never used")));
            }
            SymbolKind::Function => {
                diagnostics.push(Diagnostic::warning(format!("function `{name}` is never called")));
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
            diagnostics.push(Diagnostic::error(format!("function `{}` parameter `{param}` shadows an imported or top-level symbol", function.name)));
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
            diagnostics.push(Diagnostic::error(format!("route `{}` parameter `{param}` shadows an imported or top-level symbol", method.verb)));
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
                    diagnostics.push(Diagnostic::error(format!("`{owner}` redeclares `{name}` in the same scope")));
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

fn analyze_expr(expr: &Expr, scope: &mut Scope, used_globals: &mut HashSet<String>, diagnostics: &mut Vec<Diagnostic>) {
    match expr {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => {}
        Expr::Ident(name) => match scope.get_mut(name) {
            Some(symbol) => {
                symbol.used = true;
                if symbol.kind != SymbolKind::Local {
                    used_globals.insert(name.clone());
                    diagnostics.push(Diagnostic::error(format!("`{name}` is callable metadata, not a value; call it instead")));
                }
            }
            None => diagnostics.push(Diagnostic::error(format!("`{name}` is not defined"))),
        },
        Expr::Member(base, _) => {
            if let Expr::Ident(name) = base.as_ref() {
                match scope.get_mut(name) {
                    Some(symbol) if symbol.kind == SymbolKind::Module => {
                        symbol.used = true;
                        used_globals.insert(name.clone());
                    }
                    Some(_) => {}
                    None => diagnostics.push(Diagnostic::error(format!("module `{name}` is not imported"))),
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
                            SymbolKind::Local | SymbolKind::Module => diagnostics.push(Diagnostic::error(format!("`{name}` is not callable"))),
                        }
                    }
                    None => diagnostics.push(Diagnostic::error(format!("function `{name}` is not defined"))),
                },
                Expr::Member(base, _) => {
                    if let Expr::Ident(name) = base.as_ref() {
                        match scope.get_mut(name) {
                            Some(symbol) if symbol.kind == SymbolKind::Module => {
                                symbol.used = true;
                                used_globals.insert(name.clone());
                            }
                            Some(_) => diagnostics.push(Diagnostic::error(format!("`{name}` is not a module"))),
                            None => diagnostics.push(Diagnostic::error(format!("module `{name}` is not imported"))),
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
            diagnostics.push(Diagnostic::warning(format!("local `{name}` is never used{owner}")));
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
