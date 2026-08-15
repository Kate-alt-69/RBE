//! Static semantic analysis for `.route` files.
//!
//! The parser answers "is this syntactically valid?". The analyzer answers
//! "can this program make sense under the route language's name-resolution
//! rules?" before the interpreter/transpiler gets involved.
//!
//! Diagnostics are deliberately lightweight: errors block AOT artifact
//! generation for that route, while warnings are informational (for example
//! an imported capability or local function that is never referenced).

use std::collections::HashMap;

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
        Self {
            severity: Severity::Error,
            message: message.into(),
        }
    }

    fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SymbolKind {
    Local,
    Module,
    DirectFunction,
    Function,
}

#[derive(Clone)]
struct Symbol {
    kind: SymbolKind,
    used: bool,
}

type Scope = HashMap<String, Symbol>;

pub fn analyze(file: &RouteFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut globals = Scope::new();

    for import in &file.imports {
        let name = binding_name(import);
        let kind = match import {
            ImportTarget::Builtin(_) | ImportTarget::Custom(_) => SymbolKind::Module,
            ImportTarget::BuiltinFunction { .. } | ImportTarget::CustomFunction { .. } => {
                SymbolKind::DirectFunction
            }
        };

        if globals.insert(
            name.clone(),
            Symbol {
                kind,
                used: false,
            },
        )
        .is_some()
        {
            diagnostics.push(Diagnostic::error(format!(
                "duplicate import binding `{name}`"
            )));
        }
    }

    for function in &file.functions {
        if globals
            .insert(
                function.name.clone(),
                Symbol {
                    kind: SymbolKind::Function,
                    used: false,
                },
            )
            .is_some()
        {
            diagnostics.push(Diagnostic::error(format!(
                "duplicate top-level function `{}`",
                function.name
            )));
        }
    }

    for function in &file.functions {
        analyze_function(function, &globals, &mut diagnostics);
    }
    for method in &file.methods {
        analyze_method(method, &globals, &mut diagnostics);
    }

    for (name, symbol) in globals {
        if !symbol.used {
            match symbol.kind {
                SymbolKind::Module | SymbolKind::DirectFunction => diagnostics.push(
                    Diagnostic::warning(format!("import `{name}` is never used")),
                ),
                SymbolKind::Function => diagnostics.push(Diagnostic::warning(format!(
                    "function `{name}` is never called"
                ))),
                SymbolKind::Local => {}
            }
        }
    }

    diagnostics
}

fn analyze_function(function: &FunctionDef, globals: &Scope, diagnostics: &mut Vec<Diagnostic>) {
    let mut scope = globals.clone();
    for param in &function.params {
        if scope.insert(
            param.clone(),
            Symbol {
                kind: SymbolKind::Local,
                used: false,
            },
        )
        .is_some()
        {
            diagnostics.push(Diagnostic::error(format!(
                "function `{}` parameter `{param}` shadows an imported or top-level symbol",
                function.name
            )));
        }
    }
    analyze_statements(&function.body, &mut scope, diagnostics, &function.name);
    warn_unused_locals(&scope, diagnostics, Some(&function.name));
}

fn analyze_method(method: &MethodDef, globals: &Scope, diagnostics: &mut Vec<Diagnostic>) {
    let mut scope = globals.clone();
    if let Some(param) = &method.param_name {
        if scope.insert(
            param.clone(),
            Symbol {
                kind: SymbolKind::Local,
                used: false,
            },
        )
        .is_some()
        {
            diagnostics.push(Diagnostic::error(format!(
                "route `{}` parameter `{param}` shadows an imported or top-level symbol",
                method.verb
            )));
        }
    }
    analyze_statements(&method.body, &mut scope, diagnostics, &method.verb);
    warn_unused_locals(&scope, diagnostics, Some(&method.verb));
}

fn analyze_statements(
    statements: &[Statement],
    scope: &mut Scope,
    diagnostics: &mut Vec<Diagnostic>,
    owner: &str,
) {
    for statement in statements {
        match statement {
            Statement::Const { name, value } => {
                analyze_expr(value, scope, diagnostics);
                if scope.contains_key(name) {
                    diagnostics.push(Diagnostic::error(format!(
                        "`{owner}` redeclares `{name}` in the same scope"
                    )));
                } else {
                    scope.insert(
                        name.clone(),
                        Symbol {
                            kind: SymbolKind::Local,
                            used: false,
                        },
                    );
                }
            }
            Statement::Return(expr) | Statement::Expr(expr) => {
                analyze_expr(expr, scope, diagnostics)
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                analyze_expr(condition, scope, diagnostics);
                let mut then_scope = scope.clone();
                analyze_statements(then_body, &mut then_scope, diagnostics, owner);
                let mut else_scope = scope.clone();
                analyze_statements(else_body, &mut else_scope, diagnostics, owner);
            }
        }
    }
}

fn analyze_expr(expr: &Expr, scope: &mut Scope, diagnostics: &mut Vec<Diagnostic>) {
    match expr {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => {}
        Expr::Ident(name) => match scope.get_mut(name) {
            Some(symbol) => {
                symbol.used = true;
                if matches!(symbol.kind, SymbolKind::Module | SymbolKind::DirectFunction | SymbolKind::Function) {
                    diagnostics.push(Diagnostic::error(format!(
                        "`{name}` is callable metadata, not a value; call it instead"
                    )));
                }
            }
            None => diagnostics.push(Diagnostic::error(format!("`{name}` is not defined"))),
        },
        Expr::Member(base, _) => {
            if let Expr::Ident(name) = base.as_ref() {
                if let Some(symbol) = scope.get_mut(name) {
                    if symbol.kind == SymbolKind::Module {
                        symbol.used = true;
                    }
                }
            }
            analyze_expr(base, scope, diagnostics);
        }
        Expr::Call(callee, args) => {
            if let Expr::Ident(name) = callee.as_ref() {
                match scope.get_mut(name) {
                    Some(symbol) => {
                        symbol.used = true;
                        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::DirectFunction) {
                            diagnostics.push(Diagnostic::error(format!(
                                "`{name}` is not callable"
                            )));
                        }
                    }
                    None => diagnostics.push(Diagnostic::error(format!(
                        "function `{name}` is not defined"
                    ))),
                }
            } else if let Expr::Member(base, _) = callee.as_ref() {
                analyze_expr(base, scope, diagnostics);
                if let Expr::Ident(name) = base.as_ref() {
                    match scope.get_mut(name) {
                        Some(symbol) if symbol.kind == SymbolKind::Module => symbol.used = true,
                        Some(_) => {}
                        None => diagnostics.push(Diagnostic::error(format!(
                            "module `{name}` is not imported"
                        ))),
                    }
                }
            } else {
                analyze_expr(callee, scope, diagnostics);
            }
            for arg in args {
                analyze_expr(arg, scope, diagnostics);
            }
        }
        Expr::Object(fields) => {
            for (_, value) in fields {
                analyze_expr(value, scope, diagnostics);
            }
        }
        Expr::Array(items) => {
            for item in items {
                analyze_expr(item, scope, diagnostics);
            }
        }
        Expr::UnaryNot(inner) => analyze_expr(inner, scope, diagnostics),
        Expr::Binary { left, right, .. } => {
            analyze_expr(left, scope, diagnostics);
            analyze_expr(right, scope, diagnostics);
        }
    }
}

fn warn_unused_locals(scope: &Scope, diagnostics: &mut Vec<Diagnostic>, owner: Option<&str>) {
    for (name, symbol) in scope {
        if symbol.kind == SymbolKind::Local && !symbol.used && name != "_req" {
            let owner = owner.map(|value| format!(" in `{value}`")).unwrap_or_default();
            diagnostics.push(Diagnostic::warning(format!(
                "local `{name}` is never used{owner}"
            )));
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
        let file = parse(
            r#"
            :import[net]
            class Route {
                get(req) {
                    const value = missing;
                    return req.path;
                }
            }
            "#,
        );
        let diagnostics = analyze(&file);
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Error && d.message.contains("missing")));
        assert!(diagnostics.iter().any(|d| d.severity == Severity::Warning && d.message.contains("net")));
    }

    #[test]
    fn accepts_function_and_direct_import_calls() {
        let file = parse(
            r#"
            :import[net.ping]
            function wrap(value) { return { value: value }; }
            class Route {
                get(req) {
                    const pong = ping();
                    return wrap(pong);
                }
            }
            "#,
        );
        assert!(analyze(&file).iter().all(|d| d.severity != Severity::Error));
    }
}
