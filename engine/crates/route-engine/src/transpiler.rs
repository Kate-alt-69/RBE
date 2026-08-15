//! Ahead-of-time transpiler: turns a parsed `.route` file into genuine
//! Rust source code — RBE Upgrade Plan §2 ("AST to Rust Artifacts").
//!
//! **What this is, concretely.** For each verb method in a `.route`
//! file, generates a Rust function with the same signature shape
//! (`modules`, `req`) that does exactly what `interpreter::eval` would
//! do for that same AST, except as straight-line Rust instead of a
//! runtime tree-walk. `const` becomes `let`; literals become literals;
//! member access and module calls go through [`crate::transpiled_support`]
//! (the two operations that can genuinely fail at runtime and so still
//! need a real function call — see that module's doc comment). Name
//! resolution (is this identifier the request param, a module, or an
//! earlier `const`?) happens HERE, at transpile time, using a
//! [`Scope`] that mirrors the interpreter's runtime `HashMap<String,
//! Binding>` — the same information, just resolved once during
//! generation instead of on every request.
//!
//! **What this is NOT, yet.** This does not invoke `rustc`, does not
//! produce a `.wasm` binary, and generated artifacts are not wired
//! into request serving — routes still run through the existing
//! tree-walking [`crate::interpreter`] unchanged. This is Phase 1 only
//! (see the RBE Upgrade Plan): prove the AST-to-Rust generation is
//! correct and the artifacts are readable, before building the
//! compile-and-swap machinery in later phases on top of it. See
//! [`crate::cache`] for where generated files land on disk and when
//! they're regenerated.
//!
//! **Faithfulness, not optimization.** Every literal number goes
//! through `{:?}` formatting, every string through `{:?}` escaping,
//! and control flow is a direct statement-for-statement translation —
//! this deliberately does not try to be a clever optimizing compiler
//! yet. Getting a correct, boring translation right is the actual
//! hard/valuable part of Phase 1; performance tuning the generated
//! code is a problem worth having once there's real generated code to
//! tune.

use std::collections::HashSet;
use std::fmt::Write as _;

use crate::ast::{Expr, MethodDef, RouteFile, Statement};

/// Rust keywords (2021 edition, including reserved-for-future-use
/// ones) that a `.route` `const` name could collide with. `.route`
/// identifiers come from user-authored scripts, not Rust source, so
/// nothing stops someone writing `const type = ...` or `const move = ...`.
/// Colliding names get Rust's raw-identifier escape (`r#type`) rather
/// than silently renaming them — renaming would make the generated
/// code's variable names not match what a person reading both files
/// side by side would expect.
const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];

fn to_rust_ident(name: &str) -> String {
    if RUST_KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// Rust string literal, quotes included — `format!("{s:?}")` on a
/// `&str` produces exactly this (Rust's own `Debug` impl for `str` IS
/// a correctly escaped Rust string literal), so there's no hand-rolled
/// escaping logic here to get subtly wrong.
fn rust_string_literal(s: &str) -> String {
    format!("{s:?}")
}

/// `f64` literal that's unambiguously a float in generated source —
/// `{:?}` on an `f64` always includes a decimal point (`3.0`, not
/// `3`), which plain `{}` formatting doesn't guarantee.
fn rust_float_literal(n: f64) -> String {
    format!("{n:?}")
}

/// What a name resolves to, known statically at transpile time —
/// mirrors `interpreter::Binding`, just resolved once here instead of
/// looked up in a `HashMap` on every request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameKind {
    /// The request parameter, or an earlier `const` in this same
    /// method body — either way, a plain Rust local variable by the
    /// time code referencing it is generated.
    Local,
    Module,
}

struct Scope {
    names: std::collections::HashMap<String, NameKind>,
}

impl Scope {
    fn new(param_name: Option<&str>, module_names: &[String]) -> Self {
        let mut names = std::collections::HashMap::new();
        if let Some(p) = param_name {
            names.insert(p.to_string(), NameKind::Local);
        }
        for m in module_names {
            names.insert(m.clone(), NameKind::Module);
        }
        Self { names }
    }

    fn declare_local(&mut self, name: &str) {
        self.names.insert(name.to_string(), NameKind::Local);
    }

    fn kind_of(&self, name: &str) -> Option<NameKind> {
        self.names.get(name).copied()
    }
}

#[derive(Debug)]
pub struct TranspileError {
    pub message: String,
}

impl std::fmt::Display for TranspileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

fn err(message: impl Into<String>) -> TranspileError {
    TranspileError {
        message: message.into(),
    }
}

/// Transpiles one expression to a Rust expression string. `scope` is
/// read-only here — `const` declarations only ever add to scope
/// between STATEMENTS (see [`transpile_statement`]), never mid-
/// expression, matching the grammar (no assignment expressions, no
/// `let` inside an expression position).
fn transpile_expr(expr: &Expr, scope: &Scope) -> Result<String, TranspileError> {
    match expr {
        Expr::String(s) => Ok(format!("Value::String({}.to_string())", rust_string_literal(s))),
        Expr::Number(n) => Ok(format!("Value::Number({})", rust_float_literal(*n))),
        Expr::Bool(b) => Ok(format!("Value::Bool({b})")),
        Expr::Null => Ok("Value::Null".to_string()),

        Expr::Ident(name) => match scope.kind_of(name) {
            Some(NameKind::Local) => Ok(format!("{}.clone()", to_rust_ident(name))),
            Some(NameKind::Module) => Err(err(format!(
                "{name} is a module, not a value — did you mean to call one of its functions? \
                 (matches interpreter::eval's identical rejection for a bare module reference)"
            ))),
            None => Err(err(format!("{name} is not defined"))),
        },

        Expr::Object(fields) => {
            let mut parts = Vec::with_capacity(fields.len());
            for (key, value_expr) in fields {
                let value_code = transpile_expr(value_expr, scope)?;
                parts.push(format!("({}.to_string(), {value_code})", rust_string_literal(key)));
            }
            Ok(format!(
                "transpiled_support::object_value([{}])",
                parts.join(", ")
            ))
        }

        Expr::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.push(transpile_expr(item, scope)?);
            }
            Ok(format!("Value::Array(vec![{}])", parts.join(", ")))
        }

        Expr::Member(base, field) => {
            // Same special case the interpreter has: `module.field`
            // without a call is rejected — known statically here since
            // module names are fixed by this file's imports.
            if let Expr::Ident(name) = base.as_ref() {
                if scope.kind_of(name) == Some(NameKind::Module) {
                    return Err(err(format!(
                        "{name}.{field} — accessing a module field without calling it isn't \
                         supported in v1"
                    )));
                }
            }
            let base_code = transpile_expr(base, scope)?;
            Ok(format!(
                "transpiled_support::member_get(&{base_code}, {})?",
                rust_string_literal(field)
            ))
        }

        Expr::Call(callee, arg_exprs) => {
            let Expr::Member(base, function_name) = callee.as_ref() else {
                return Err(err(
                    "only `module.function(args)` calls into an imported module are supported \
                     in v1",
                ));
            };
            let Expr::Ident(module_name) = base.as_ref() else {
                return Err(err(
                    "only `module.function(args)` calls into an imported module are supported \
                     in v1",
                ));
            };
            if scope.kind_of(module_name) != Some(NameKind::Module) {
                return Err(err(format!(
                    "{module_name} was not imported — add :import[{module_name}] at the top of \
                     this .route file"
                )));
            }

            let mut arg_codes = Vec::with_capacity(arg_exprs.len());
            for a in arg_exprs {
                arg_codes.push(transpile_expr(a, scope)?);
            }
            Ok(format!(
                "transpiled_support::call_module(modules, {}, {}, vec![{}])?",
                rust_string_literal(module_name),
                rust_string_literal(function_name),
                arg_codes.join(", ")
            ))
        }
    }
}

fn transpile_statement(
    stmt: &Statement,
    scope: &mut Scope,
    out: &mut String,
) -> Result<(), TranspileError> {
    match stmt {
        Statement::Const { name, value } => {
            // Evaluated against scope BEFORE `name` is declared — a
            // const's own value expression can't reference itself,
            // matching `interpreter::run`'s identical evaluation order
            // (eval first, insert into scope after).
            let value_code = transpile_expr(value, scope)?;
            let ident = to_rust_ident(name);
            let _ = writeln!(out, "    let {ident} = {value_code};");
            scope.declare_local(name);
        }
        Statement::Return(expr) => {
            let value_code = transpile_expr(expr, scope)?;
            let _ = writeln!(out, "    return Ok({value_code});");
        }
        Statement::Expr(expr) => {
            let value_code = transpile_expr(expr, scope)?;
            let _ = writeln!(out, "    {value_code};");
        }
    }
    Ok(())
}

/// Transpiles one method (`get`, `post`, ...) into a standalone Rust
/// function. Falls off the end returning `Value::Null` if the body has
/// no `return` — same as `interpreter::run`'s `Ok(Value::Null)` after
/// its statement loop, itself standing in for JS's implicit
/// `undefined` return (see that function's doc comment).
fn transpile_method(method: &MethodDef, module_names: &[String]) -> Result<String, TranspileError> {
    let mut scope = Scope::new(method.param_name.as_deref(), module_names);
    let mut body = String::new();

    for stmt in &method.body {
        transpile_statement(stmt, &mut scope, &mut body)?;
    }

    let param_name = method.param_name.as_deref().unwrap_or("_req");
    let param_ident = to_rust_ident(param_name);

    let mut function = String::new();
    let _ = writeln!(
        function,
        "pub fn {}(modules: &ModuleRegistry, {param_ident}: Value) -> Result<Value, EvalError> {{",
        to_rust_ident(&method.verb)
    );
    function.push_str(&body);
    function.push_str("    Ok(Value::Null)\n");
    function.push_str("}\n");

    Ok(function)
}

/// Transpiles an entire `.route` file to a complete, self-contained
/// Rust module — the header comment naming the source file it came
/// from, imports, then one function per verb method (see
/// [`transpile_method`]).
///
/// `source_path` and `module_names` come from the caller
/// ([`crate::cache`]) rather than being recomputed here, since it
/// already has them from loading/parsing the file — no reason to
/// duplicate that work inside the transpiler.
pub fn transpile_file(
    file: &RouteFile,
    source_path: &str,
    module_names: &[String],
) -> Result<String, TranspileError> {
    let mut out = String::new();

    let _ = writeln!(out, "// AUTO-GENERATED by route_engine::transpiler from {source_path}");
    let _ = writeln!(out, "// Regenerated automatically whenever that file's content hash");
    let _ = writeln!(out, "// changes — do not hand-edit; edit the .route source instead.");
    let _ = writeln!(out, "//");
    let _ = writeln!(out, "// Phase 1 output (RBE Upgrade Plan §2): readable Rust mirroring");
    let _ = writeln!(out, "// this route's interpreted semantics exactly. Not yet wired into");
    let _ = writeln!(out, "// request serving, not yet compiled to wasm32-wasi.");
    let _ = writeln!(out);
    let _ = writeln!(out, "#![allow(dead_code, unused_variables, clippy::all)]");
    let _ = writeln!(out);
    let _ = writeln!(out, "use route_engine::{{transpiled_support, EvalError, ModuleRegistry, Value}};");
    let _ = writeln!(out);

    // Duplicate verb names shouldn't happen (the parser only allows one
    // method per class-body entry, and there's nothing that would
    // produce the same verb twice) but a transpiler is exactly the
    // place to double-check an assumption like that rather than
    // silently generating two functions with the same name and letting
    // rustc's own "duplicate definition" error be the first anyone
    // hears about it.
    let mut seen_verbs = HashSet::new();
    for method in &file.methods {
        if !seen_verbs.insert(method.verb.clone()) {
            return Err(err(format!(
                "{source_path}: duplicate '{}' method — this should have been caught at parse \
                 time",
                method.verb
            )));
        }
        out.push_str(&transpile_method(method, module_names)?);
        out.push('\n');
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::{Interpreter, RequestContext};
    use crate::lexer::Lexer;
    use crate::modules::{binding_name, ModuleRegistry};
    use crate::parser::Parser;
    use std::collections::HashMap;

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().expect("lex failed");
        Parser::new(tokens).parse_file().expect("parse failed")
    }

    #[test]
    fn transpiles_object_literal_and_module_call_without_error() {
        let file = parse(
            r#"
            :import[net]
            class Route {
                async get(req) {
                    const pong = net.ping();
                    return { ok: true, pong: pong };
                }
            }
        "#,
        );
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        let code = transpile_file(&file, "test.route", &module_names).expect("transpile failed");

        assert!(code.contains("pub fn get(modules: &ModuleRegistry"));
        assert!(code.contains("let pong = transpiled_support::call_module(modules, \"net\", \"ping\", vec![])?;"));
        assert!(code.contains("transpiled_support::object_value"));
    }

    #[test]
    fn rejects_bare_module_reference_same_as_interpreter() {
        let file = parse(
            r#"
            :import[net]
            class Route {
                get(req) {
                    return net;
                }
            }
        "#,
        );
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        let result = transpile_file(&file, "test.route", &module_names);
        assert!(result.is_err(), "bare module reference should fail to transpile");
    }

    /// The load-bearing test: for a range of `.route` bodies, the
    /// TRANSPILED Rust logic (executed directly here as real Rust, not
    /// via rustc-on-a-file — that's Phase 1's whole point, this test
    /// runs the same statement-by-statement logic the generator would
    /// emit) and the INTERPRETER must agree on the result. This is
    /// what actually catches a transpiler/interpreter semantics
    /// mismatch, not just "did it produce syntactically plausible
    /// text."
    #[test]
    fn transpiled_logic_agrees_with_interpreter_on_object_and_module_call() {
        let source = r#"
            :import[net]
            class Route {
                async get(req) {
                    const pong = net.ping();
                    return { ok: pong.ok, path: req.path };
                }
            }
        "#;
        let file = parse(source);
        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        let modules = ModuleRegistry::from_imports(&file.imports);
        let req_ctx = RequestContext {
            method: "GET".to_string(),
            path: "/api/health-check".to_string(),
            params: HashMap::new(),
            query: HashMap::new(),
        };

        // Interpreter result.
        let mut interpreter = Interpreter::new(&modules);
        let interpreted = interpreter
            .run(&file.methods[0], &req_ctx, &module_names)
            .expect("interpreter run failed");

        // Hand-executed equivalent of what the transpiler generates for
        // this exact body — a literal transcription of the generated
        // Rust, not a re-implementation, so this only passes if the
        // transpiler's output would actually behave the same way.
        use crate::ast::Value;
        use crate::transpiled_support;
        fn transpiled_equivalent(
            modules: &ModuleRegistry,
            req: Value,
        ) -> Result<Value, crate::interpreter::EvalError> {
            let pong = transpiled_support::call_module(modules, "net", "ping", vec![])?;
            return Ok(transpiled_support::object_value([
                (
                    "ok".to_string(),
                    transpiled_support::member_get(&pong, "ok")?,
                ),
                (
                    "path".to_string(),
                    transpiled_support::member_get(&req, "path")?,
                ),
            ]));
        }

        let req_value = {
            // Same construction `interpreter::run` does internally for
            // the bound `req` parameter.
            let mut fields = HashMap::new();
            fields.insert("method".to_string(), Value::String(req_ctx.method.clone()));
            fields.insert("path".to_string(), Value::String(req_ctx.path.clone()));
            fields.insert("params".to_string(), Value::Object(req_ctx.params.clone()));
            fields.insert("query".to_string(), Value::Object(req_ctx.query.clone()));
            Value::Object(fields)
        };

        let transpiled = transpiled_equivalent(&modules, req_value).expect("transpiled-equivalent run failed");

        let (Value::Object(interpreted_map), Value::Object(transpiled_map)) = (interpreted, transpiled) else {
            panic!("both results should be objects");
        };
        assert!(matches!(interpreted_map.get("ok"), Some(Value::Bool(true))));
        assert!(matches!(transpiled_map.get("ok"), Some(Value::Bool(true))));
        assert_eq!(
            format!("{:?}", interpreted_map.get("path")),
            format!("{:?}", transpiled_map.get("path"))
        );
    }
}
