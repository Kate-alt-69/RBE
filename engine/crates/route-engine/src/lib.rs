//! RBE's JavaScript-shaped `.route` language.
//!
//! `.route` files are parsed into a controlled Rust AST. They are not
//! arbitrary JavaScript and never invoke Node/Bun. The current runtime
//! interpreter is retained as a fallback while the AOT Rust artifact
//! pipeline is developed.

// The route cache and direct parser helpers are retained as internal building
// blocks for the AOT/diagnostic pipeline even when a particular build path does
// not currently call them directly.
#![allow(dead_code)]

mod analyzer;
mod ast;
mod discovery;
mod interpreter;
mod lexer;
mod module_eval;
mod module_runtime;
mod modules;
mod parser;
mod paths;
mod service_eval;
mod terminal;

pub mod cache;
pub mod transpiled_support;
pub mod transpiler;

pub use analyzer::{analyze, Diagnostic, Severity};
pub use ast::{
    BinaryOp, Expr, FunctionDef, ImportTarget, MethodDef, ModuleFile, RouteFile, ServiceProgram,
    Statement, Value,
};
pub use discovery::{build_routes, RouteCache};
pub use interpreter::{EvalError, Interpreter, RequestContext};
pub use module_eval::{ModuleEvalError, ModuleExecutor};
pub use module_runtime::{
    ModuleCompileError, ModuleCompileErrors, ModuleProgram, ServiceInterfaces,
};
pub use modules::{binding_name, route_capability_allowed, ModuleError, ModuleRegistry};
pub use parser::ParseError;
pub use paths::{binary_dir, default_api_dir, default_module_dir, resolve_custom_import};
pub use service_eval::ServiceProgramExecutor;

pub fn parse_service_source(source: &str) -> Result<ServiceProgram, ParseError> {
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .map_err(|error| ParseError {
            message: error.message,
            line: error.line,
            column: error.column,
        })?;
    parser::Parser::new(tokens).parse_service_file()
}

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
        let file = parse(
            r#"
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
        "#,
        );

        assert_eq!(file.imports.len(), 1);
        assert_eq!(file.functions.len(), 1);
        assert_eq!(file.functions[0].name, "makeResponse");
        assert_eq!(file.functions[0].params, vec!["value"]);
        assert_eq!(file.methods[0].verb, "get");
    }

    #[test]
    fn parses_module_exports() {
        let tokens = Lexer::new(
            r#":import[net]
            function hidden(value) { return value; }
            export async function visible(value) { return hidden(value); }"#,
        )
        .tokenize()
        .expect("lex failed");
        let file = Parser::new(tokens)
            .parse_module_file()
            .expect("module parse failed");
        assert_eq!(file.functions.len(), 2);
        assert_eq!(file.exports, vec!["visible"]);
    }

    #[test]
    fn parses_executable_service_program() {
        let file = parse_service_source(
            r#"
            :import[memory, json]
            :service[
                name = uac-cache
                restart = on-failure
                instances = 1
            ]
            function normalize(value) { return value; }
            export async function get(id) { return normalize(id); }
            class Service {
                start(ctx) { return true; }
                health() { return { ok: true }; }
                stop(ctx) { return true; }
            }
        "#,
        )
        .expect("service parse failed");
        assert_eq!(file.functions.len(), 2);
        assert_eq!(file.exports, vec!["get"]);
        assert_eq!(file.class_name.as_deref(), Some("Service"));
        assert_eq!(
            file.lifecycle
                .iter()
                .map(|method| method.verb.as_str())
                .collect::<Vec<_>>(),
            vec!["start", "health", "stop"]
        );
    }

    #[test]
    fn rejects_service_to_service_imports() {
        let error = parse_service_source(
            r#"
            :import[service:other]
            :service[name = current]
            export function run() { return true; }
        "#,
        )
        .expect_err("service-to-service import should fail");
        assert!(error.message.contains("service-to-service"));
    }

    #[test]
    fn rejects_malformed_service_body() {
        let error = parse_service_source(
            r#"
            :service[name = broken]
            export function run(value) { return value }
        "#,
        )
        .expect_err("missing semicolon should fail");
        assert!(error.message.contains("Semicolon") || error.message.contains("semicolon"));
    }

    #[test]
    fn parses_service_imports_for_modules() {
        let tokens = Lexer::new(
            r#":import[service:uac-cache as uac, service:search.find as lookup, "service:media"]
            export function run(value) { return value; }"#,
        )
        .tokenize()
        .expect("lex failed");
        let file = Parser::new(tokens)
            .parse_module_file()
            .expect("module parse failed");
        assert_eq!(file.imports.len(), 3);
        assert_eq!(binding_name(&file.imports[0]), "uac");
        assert_eq!(binding_name(&file.imports[1]), "lookup");
        assert_eq!(binding_name(&file.imports[2]), "media");
    }

    #[test]
    fn route_rejects_direct_service_imports() {
        let file = parse(
            r#"
            :import[service:uac-cache]
            class Route {
                get(req) { return { ok: true }; }
            }
        "#,
        );
        let diagnostics = analyze(&file);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "E3020"));
    }

    #[test]
    fn parses_multiple_import_entries_and_aliases() {
        let file = parse(
            r#"
            :import[response as resp, net, net.ping as ping]
            class Route {
                get(req) {
                    const pong = ping();
                    return resp.status(pong);
                }
            }
        "#,
        );

        assert_eq!(file.imports.len(), 3);
        assert_eq!(binding_name(&file.imports[0]), "resp");
        assert_eq!(binding_name(&file.imports[1]), "net");
        assert_eq!(binding_name(&file.imports[2]), "ping");
    }

    #[test]
    fn rejects_trailing_import_comma() {
        let tokens = Lexer::new(":import[net,]").tokenize().expect("lex failed");
        let error = Parser::new(tokens)
            .parse_file()
            .expect_err("trailing comma should fail");
        assert!(error.message.contains("trailing commas"));
    }

    #[test]
    fn rejects_missing_import_comma() {
        let tokens = Lexer::new(":import[net json]")
            .tokenize()
            .expect("lex failed");
        let error = Parser::new(tokens)
            .parse_file()
            .expect_err("missing comma should fail");
        assert!(error.message.contains("expected `,`"));
    }

    #[test]
    fn parses_conditionals_and_operators() {
        let file = parse(
            r#"
            class Route {
                get(req) {
                    if (!req.ok || req.status >= 400) {
                        return { ok: false };
                    } else {
                        return { ok: true };
                    }
                }
            }
        "#,
        );
        assert_eq!(file.methods.len(), 1);
    }

    #[test]
    fn interpreter_and_analyzer_agree_on_a_valid_route() {
        let file = parse(
            r#"
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
        "#,
        );

        assert!(analyze(&file)
            .iter()
            .all(|diagnostic| diagnostic.severity != Severity::Error));

        let module_names: Vec<String> = file.imports.iter().map(binding_name).collect();
        let modules = ModuleRegistry::from_imports(&file.imports);
        let req = RequestContext {
            method: "GET".into(),
            path: "/api/example".into(),
            params: HashMap::new(),
            query: HashMap::new(),
        };
        let mut interpreter = Interpreter::new(&modules).with_functions(&file.functions);
        let result = interpreter
            .run(&file.methods[0], &req, &module_names)
            .expect("run failed");
        let Value::Object(map) = result else {
            panic!("expected object");
        };
        assert!(matches!(map.get("ok"), Some(Value::Bool(true))));
    }

    #[test]
    fn missing_variable_is_an_error() {
        let file = parse(
            r#"
            class Route {
                get(req) {
                    return missing;
                }
            }
        "#,
        );
        assert!(analyze(&file)
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error));

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
