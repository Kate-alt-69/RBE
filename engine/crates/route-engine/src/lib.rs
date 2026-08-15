//! RBE's JavaScript-shaped `.route` language.
//!
//! `.route` files are parsed into a controlled Rust AST. They are not
//! arbitrary JavaScript and never invoke Node/Bun. The current runtime
//! interpreter is retained as a fallback while the AOT Rust artifact
//! pipeline is developed.

mod analyzer;
mod ast;
mod discovery;
mod interpreter;
mod lexer;
mod modules;
mod parser;
mod paths;

pub mod cache;
pub mod transpiled_support;
pub mod transpiler;

pub use analyzer::{analyze, Diagnostic, Severity};
pub use ast::{BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, RouteFile, Statement, Value};
pub use discovery::{build_routes, RouteCache};
pub use interpreter::{EvalError, Interpreter, RequestContext};
pub use modules::{binding_name, ModuleError, ModuleRegistry};
pub use paths::{binary_dir, default_api_dir, default_module_dir, resolve_custom_import};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use std::collections::HashMap;

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().expect("lex failed");
        Parser::new(tokens).parse_file().expect("parse failed")
    }

    #[test]
    fn parses_functions_and_direct_imports() {
        let file = parse(r#"
            :import[net.ping]
            function makeResponse($[value]) {
                return { ok: true, value: $[value] };
            }
            class Route {
                async get($[req]) {
                    const pong = ping();
                    return makeResponse(pong);
                }
            }
        "#);

        assert_eq!(file.imports.len(), 1);
        assert_eq!(file.functions.len(), 1);
        assert_eq!(file.functions[0].name, "makeResponse");
        assert_eq!(file.functions[0].params, vec!["value"]);
        assert_eq!(file.methods[0].verb, "get");
    }

    #[test]
    fn parses_conditionals_and_operators() {
        let file = parse(r#"
            class Route {
                get(req) {
                    if (!req.ok || req.status >= 400) {
                        return { ok: false };
                    } else {
                        return { ok: true };
                    }
                }
            }
        "#);
        assert_eq!(file.methods.len(), 1);
    }

    #[test]
    fn interpreter_and_analyzer_agree_on_a_valid_route() {
        let file = parse(r#"
            :import[net.ping]
            function makeResponse($[value]) {
                return { ok: true, value: $[value] };
            }
            class Route {
                get($[req]) {
                    const pong = ping();
                    return makeResponse(pong);
                }
            }
        "#);

        assert!(analyze(&file).iter().all(|diagnostic| diagnostic.severity != Severity::Error));

        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        let modules = ModuleRegistry::from_imports(&file.imports);
        let req = RequestContext {
            method: "GET".into(),
            path: "/api/example".into(),
            params: HashMap::new(),
            query: HashMap::new(),
        };
        let mut interpreter = Interpreter::new(&modules).with_functions(&file.functions);
        let result = interpreter.run(&file.methods[0], &req, &module_names).expect("run failed");
        let Value::Object(map) = result else { panic!("expected object"); };
        assert!(matches!(map.get("ok"), Some(Value::Bool(true))));
    }

    #[test]
    fn missing_variable_is_an_error() {
        let file = parse(r#"
            class Route {
                get(req) {
                    return missing;
                }
            }
        "#);
        assert!(analyze(&file).iter().any(|diagnostic| diagnostic.severity == Severity::Error));

        let modules = ModuleRegistry::from_imports(&file.imports);
        let req = RequestContext {
            method: "GET".into(),
            path: "/api/whatever".into(),
            params: HashMap::new(),
            query: HashMap::new(),
        };
        let mut interpreter = Interpreter::new(&modules);
        let names: Vec<String> = file.imports.iter().map(binding_name).collect();
        assert!(interpreter.run(&file.methods[0], &req, &names).is_err());
    }
}
