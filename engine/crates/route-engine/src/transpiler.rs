//! AOT `.route` -> Rust source generation.
//! This generates readable Rust artifacts but does not invoke rustc itself.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::ast::{BinaryOp, Expr, FunctionDef, MethodDef, RouteFile, Statement};
use crate::modules::ImportTarget;

#[derive(Debug)]
pub struct TranspileError { pub message: String }

impl std::fmt::Display for TranspileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.message) }
}

fn err(message: impl Into<String>) -> TranspileError { TranspileError { message: message.into() } }

const RUST_KEYWORDS: &[&str] = &[
    "as","break","const","continue","crate","else","enum","extern","false","fn","for",
    "if","impl","in","let","loop","match","mod","move","mut","pub","ref","return",
    "self","Self","static","struct","super","trait","true","type","unsafe","use",
    "where","while","async","await","dyn","abstract","become","box","do","final",
    "macro","override","priv","typeof","unsized","virtual","yield","try",
];

fn rust_ident(name: &str) -> String {
    if RUST_KEYWORDS.contains(&name) { format!("r#{name}") } else { name.to_string() }
}

#[derive(Clone, Copy)]
enum NameKind { Local, Module, DirectFunction, Function }

struct Scope {
    names: HashMap<String, NameKind>,
}

impl Scope {
    fn new(param: Option<&str>, imports: &[ImportTarget], functions: &[FunctionDef]) -> Self {
        let mut names = HashMap::new();
        if let Some(param) = param { names.insert(param.to_string(), NameKind::Local); }
        for import in imports {
            match import {
                ImportTarget::Builtin(name) | ImportTarget::Custom(name) => {
                    let binding = crate::modules::binding_name(import);
                    let _ = name;
                    names.insert(binding, NameKind::Module);
                }
                ImportTarget::BuiltinFunction { .. } | ImportTarget::CustomFunction { .. } => {
                    names.insert(crate::modules::binding_name(import), NameKind::DirectFunction);
                }
            }
        }
        for function in functions { names.insert(function.name.clone(), NameKind::Function); }
        Self { names }
    }
    fn declare(&mut self, name: &str) { self.names.insert(name.to_string(), NameKind::Local); }
    fn kind(&self, name: &str) -> Option<NameKind> { self.names.get(name).copied() }
}

fn expr_code(expr: &Expr, scope: &Scope) -> Result<String, TranspileError> {
    match expr {
        Expr::String(s) => Ok(format!("Value::String({s:?}.to_string())")),
        Expr::Number(n) => Ok(format!("Value::Number({n:?})")),
        Expr::Bool(v) => Ok(format!("Value::Bool({v})")),
        Expr::Null => Ok("Value::Null".into()),
        Expr::Ident(name) => match scope.kind(name) {
            Some(NameKind::Local) => Ok(format!("{}.clone()", rust_ident(name))),
            Some(NameKind::Module) => Err(err(format!("{name} is a module and must be called"))),
            Some(NameKind::DirectFunction) | Some(NameKind::Function) => {
                Err(err(format!("{name} is a function and must be called")))
            }
            None => Err(err(format!("{name} is not defined"))),
        },
        Expr::Member(base, field) => {
            if let Expr::Ident(name) = base.as_ref() {
                if matches!(scope.kind(name), Some(NameKind::Module)) {
                    return Err(err(format!("{name}.{field} must be called")));
                }
            }
            Ok(format!(
                "transpiled_support::member_get(&{}, {:?})?",
                expr_code(base, scope)?, field
            ))
        }
        Expr::Call(callee, args) => {
            let mut arg_code = Vec::new();
            for arg in args { arg_code.push(expr_code(arg, scope)?); }

            if let Expr::Ident(name) = callee.as_ref() {
                match scope.kind(name) {
                    Some(NameKind::DirectFunction) => return Ok(format!(
                        "transpiled_support::call_direct(modules, {:?}, vec![{}])?",
                        name, arg_code.join(", ")
                    )),
                    Some(NameKind::Function) => return Ok(format!(
                        "{}(modules, vec![{}])?",
                        rust_ident(name), arg_code.join(", ")
                    )),
                    _ => {}
                }
            }

            if let Expr::Member(base, function) = callee.as_ref() {
                if let Expr::Ident(module) = base.as_ref() {
                    if matches!(scope.kind(module), Some(NameKind::Module)) {
                        return Ok(format!(
                            "transpiled_support::call_module(modules, {:?}, {:?}, vec![{}])?",
                            module, function, arg_code.join(", ")
                        ));
                    }
                }
            }

            Err(err("unsupported call target; use a local function or imported capability"))
        }
        Expr::Object(fields) => {
            let mut items = Vec::new();
            for (key, value) in fields {
                items.push(format!("({:?}.to_string(), {})", key, expr_code(value, scope)?));
            }
            Ok(format!("transpiled_support::object_value([{}])", items.join(", ")))
        }
        Expr::Array(items) => {
            let items = items.iter().map(|v| expr_code(v, scope)).collect::<Result<Vec<_>, _>>()?;
            Ok(format!("Value::Array(vec![{}])", items.join(", ")))
        }
        Expr::UnaryNot(inner) => Ok(format!("transpiled_support::unary_not({})", expr_code(inner, scope)?)),
        Expr::Binary { left, op, right } => {
            let op_code = format!("crate::ast::BinaryOp::{op:?}");
            Ok(format!(
                "transpiled_support::binary({}, {}, {})?",
                op_code,
                expr_code(left, scope)?,
                expr_code(right, scope)?
            ))
        }
    }
}

fn emit_statements(
    body: &[Statement],
    scope: &mut Scope,
    out: &mut String,
) -> Result<(), TranspileError> {
    for stmt in body {
        match stmt {
            Statement::Const { name, value } => {
                let value = expr_code(value, scope)?;
                writeln!(out, "    let {} = {value};", rust_ident(name)).unwrap();
                scope.declare(name);
            }
            Statement::Return(expr) => {
                writeln!(out, "    return Ok({});", expr_code(expr, scope)?).unwrap();
            }
            Statement::Expr(expr) => {
                writeln!(out, "    {};", expr_code(expr, scope)?).unwrap();
            }
            Statement::If { condition, then_body, else_body } => {
                writeln!(out, "    if transpiled_support::truthy(&{}) {{", expr_code(condition, scope)?).unwrap();
                let mut branch_scope = Scope { names: scope.names.clone() };
                emit_statements(then_body, &mut branch_scope, out)?;
                if !else_body.is_empty() {
                    out.push_str("    } else {\n");
                    let mut else_scope = Scope { names: scope.names.clone() };
                    emit_statements(else_body, &mut else_scope, out)?;
                }
                out.push_str("    }\n");
            }
        }
    }
    Ok(())
}

fn emit_function(
    function: &FunctionDef,
    imports: &[ImportTarget],
    functions: &[FunctionDef],
) -> Result<String, TranspileError> {
    let mut scope = Scope::new(None, imports, functions);
    for param in &function.params { scope.declare(param); }

    let args = function.params.iter()
        .map(|p| format!("{}: Value", rust_ident(p)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    writeln!(out, "pub fn {}(modules: &ModuleRegistry, {})-> Result<Value, EvalError> {{", rust_ident(&function.name), args).unwrap();
    emit_statements(&function.body, &mut scope, &mut out)?;
    out.push_str("    Ok(Value::Null)\n}\n");
    Ok(out)
}

fn emit_method(
    method: &MethodDef,
    imports: &[ImportTarget],
    functions: &[FunctionDef],
) -> Result<String, TranspileError> {
    let mut scope = Scope::new(method.param_name.as_deref(), imports, functions);
    let param = method.param_name.clone().unwrap_or_else(|| "_req".into());

    let mut out = String::new();
    writeln!(out, "pub fn {}(modules: &ModuleRegistry, {}: Value) -> Result<Value, EvalError> {{", rust_ident(&method.verb), rust_ident(&param)).unwrap();
    emit_statements(&method.body, &mut scope, &mut out)?;
    out.push_str("    Ok(Value::Null)\n}\n");
    Ok(out)
}

pub fn transpile_file(
    file: &RouteFile,
    source_path: &str,
    _module_names: &[String],
) -> Result<String, TranspileError> {
    let mut out = String::new();
    writeln!(out, "// AUTO-GENERATED by RBE route-engine from {source_path}").unwrap();
    writeln!(out, "// Do not hand-edit; edit the .route source instead.").unwrap();
    writeln!(out, "#![allow(dead_code, unused_variables, clippy::all)]").unwrap();
    out.push_str("use route_engine::{transpiled_support, EvalError, ModuleRegistry, Value};\n\n");

    for function in &file.functions {
        out.push_str(&emit_function(function, &file.imports, &file.functions)?);
        out.push('\n');
    }
    for method in &file.methods {
        out.push_str(&emit_method(method, &file.imports, &file.functions)?);
        out.push('\n');
    }
    Ok(out)
}
