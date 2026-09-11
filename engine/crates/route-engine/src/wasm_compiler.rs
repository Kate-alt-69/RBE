//! Native REL `.route` -> WebAssembly lowering.
//!
//! This compiler deliberately exposes a small executable subset first instead
//! of disguising interpreter execution as WASM. Unsupported REL constructs are
//! classified as an explicit interpreter fallback. Native artifacts use the
//! RBE worker ABI and return JSON bytes through `rbe.output_write`.

use core_lib::{
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};
use sha2::{Digest, Sha256};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::ast::{Expr, ImportTarget, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 3;
const MAX_STATIC_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const WASM_PAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteWasmInput {
    None,
    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`
    /// value. This keeps strings/null/objects/arrays semantically identical.
    JsonBody,
}

#[derive(Debug, Clone)]
pub struct RouteWasmArtifact {
    pub verb: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub input: RouteWasmInput,
}

#[derive(Debug, Clone)]
pub enum RouteWasmCompilation {
    Native(RouteWasmArtifact),
    InterpreterFallback { reason: String },
}

impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

/// Compile the currently supported native route subset.
///
/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no helper functions. In addition to literals and req.body passthrough, ABI v3
/// permits one directly imported `http.get/post/request` function with fully
/// static JSON arguments. Namespace imports stay interpreter-only so native
/// grants remain operation-exact rather than widening authority.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let http_import =
        match file.imports.as_slice() {
            [] => None,
            [import] => match direct_http_import(import) {
                Some(import) => Some(import),
                None => return fallback(
                    "native Route-WASM v3 only supports one direct http.get/post/request import",
                ),
            },
            _ => {
                return fallback(
                    "native Route-WASM v3 supports at most one direct host capability import",
                )
            }
        };
    if !file.functions.is_empty() {
        return fallback("route helper functions are not WASM-native yet");
    }
    if file.methods.len() != 1 {
        return fallback("native route compilation currently requires exactly one HTTP method");
    }

    let method = &file.methods[0];
    let [Statement::Return(expr)] = method.body.as_slice() else {
        return fallback("native route body currently requires one literal return statement");
    };
    let (bytes, input) = if let Some((binding, operation)) = http_import.as_ref() {
        let Some(args) = static_direct_call(binding, expr) else {
            return fallback(
                "native public HTTP calls require the imported function as the return value with static JSON arguments",
            );
        };
        let payload = match serde_json::to_vec(&args) {
            Ok(payload) => payload,
            Err(error) => return fallback(format!("encode native HTTP arguments: {error}")),
        };
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return fallback("native HTTP argument payload exceeds the capability envelope");
        }
        (
            encode_public_http_module(operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some(value) = static_json(expr) {
        let output = match serde_json::to_vec(&value) {
            Ok(output) => output,
            Err(error) => {
                return fallback(format!("static route result could not be encoded: {error}"))
            }
        };
        if output.len() > MAX_STATIC_OUTPUT_BYTES {
            return fallback("static route result exceeds the WASM execution output limit");
        }
        (encode_static_json_module(&output), RouteWasmInput::None)
    } else if returns_request_body(method.param_name.as_deref(), expr) {
        (encode_input_echo_module(), RouteWasmInput::JsonBody)
    } else {
        return fallback("route return value is outside the native Route-WASM v3 subset");
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
        input,
    })
}

fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {
    RouteWasmCompilation::InterpreterFallback {
        reason: reason.into(),
    }
}

fn direct_http_import(import: &ImportTarget) -> Option<(String, String)> {
    let (binding, module, function) = match import {
        ImportTarget::BuiltinFunction { module, function } => {
            (function.clone(), module.as_str(), function.as_str())
        }
        ImportTarget::Aliased { target, alias } => match target.as_ref() {
            ImportTarget::BuiltinFunction { module, function } => {
                (alias.clone(), module.as_str(), function.as_str())
            }
            _ => return None,
        },
        _ => return None,
    };
    (module == "http" && matches!(function, "get" | "post" | "request"))
        .then(|| (binding, function.to_string()))
}

fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {
    let Expr::Call(target, args) = expr else {
        return None;
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == binding) {
        return None;
    }
    args.iter().map(static_json).collect()
}

fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {
    let Some(parameter) = parameter else {
        return false;
    };
    matches!(
        expr,
        Expr::Member(target, field)
            if field == "body" && matches!(target.as_ref(), Expr::Ident(name) if name == parameter)
    )
}

fn static_json(expr: &Expr) -> Option<serde_json::Value> {
    match expr {
        Expr::String(value) => Some(serde_json::Value::String(value.clone())),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(serde_json::Value::Number),
        Expr::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Array(values) => values
            .iter()
            .map(static_json)
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Expr::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), static_json(value)?);
            }
            Some(serde_json::Value::Object(object))
        }
        Expr::Ident(_)
        | Expr::Member(_, _)
        | Expr::Call(_, _)
        | Expr::UnaryNot(_)
        | Expr::Binary { .. } => None,
    }
}

fn encode_public_http_module(operation: &str, payload: &[u8]) -> Vec<u8> {
    const NETWORK_CAPABILITY_KIND: i32 = 1;
    let target = PUBLIC_HTTP_TARGET.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = operation_offset + operation.len();
    let response_offset = (payload_offset + payload.len() + 15) & !15usize;
    let memory_bytes = response_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES;

    let mut types = TypeSection::new();
    types.ty().function(
        [
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
        ],
        [ValType::I32],
    );
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "capability_call", EntityType::Function(0));
    imports.import("rbe", "capability_response_len", EntityType::Function(1));
    imports.import("rbe", "capability_response_read", EntityType::Function(2));
    imports.import("rbe", "output_write", EntityType::Function(2));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let pages = memory_bytes.max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // Four capability/output imports occupy function indices 0..=3.
    exports.export("run", ExportKind::Func, 4);

    let mut run = Function::new([(1, ValType::I32)]);
    run.instructions()
        .i32_const(NETWORK_CAPABILITY_KIND)
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .i32_const(payload.len() as i32)
        .call(0)
        .drop()
        .call(1)
        .local_set(0)
        .i32_const(response_offset as i32)
        .local_get(0)
        .call(2)
        .drop()
        .i32_const(response_offset as i32)
        .local_get(0)
        .call(3)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(
        0,
        &ConstExpr::i32_const(target_offset as i32),
        target.iter().copied(),
    );
    data.active(
        0,
        &ConstExpr::i32_const(operation_offset as i32),
        operation.iter().copied(),
    );
    data.active(
        0,
        &ConstExpr::i32_const(payload_offset as i32),
        payload.iter().copied(),
    );

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

fn encode_input_echo_module() -> Vec<u8> {
    // Route-WASM ABI v2 body passthrough: input bytes are already the JSON
    // encoding of req.body, so the guest only needs bounded input/output copy.
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "input_len", EntityType::Function(0));
    imports.import("rbe", "input_read", EntityType::Function(1));
    imports.import("rbe", "output_write", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(0);

    let pages = CONTAINER_MAX_EXECUTION_INPUT_BYTES
        .max(1)
        .div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // input_len/input_read/output_write occupy function indices 0..=2.
    exports.export("run", ExportKind::Func, 3);

    let mut run = Function::new([(1, ValType::I32)]);
    run.instructions()
        .call(0)
        .local_set(0)
        .i32_const(0)
        .local_get(0)
        .call(1)
        .drop()
        .i32_const(0)
        .local_get(0)
        .call(2)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code);
    module.finish()
}

fn encode_static_json_module(output: &[u8]) -> Vec<u8> {
    // Function type 0: rbe.output_write(i32 ptr, i32 len) -> i32.
    // Function type 1: run() -> i32.
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function([], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import("rbe", "output_write", EntityType::Function(0));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let pages = output.len().max(1).div_ceil(WASM_PAGE_BYTES) as u64;
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    // output_write is imported function index 0; run is defined function index 1.
    exports.export("run", ExportKind::Func, 1);

    let mut run = Function::new([]);
    run.instructions()
        .i32_const(0)
        .i32_const(output.len() as i32)
        .call(0)
        .drop()
        .i32_const(0)
        .end();
    let mut code = CodeSection::new();
    code.function(&run);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), output.iter().copied());

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use execution_engine::{CapabilityHost, ExecutionLimits, WasmExecutor};

    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    #[test]
    fn compiles_literal_route_to_valid_deterministic_wasm() {
        let route = parse("class Route { get(req) { return { ok: true, count: 7 }; } }");
        let RouteWasmCompilation::Native(first) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        let RouteWasmCompilation::Native(second) = compile_route(&route) else {
            panic!("literal route should be native");
        };
        assert_eq!(first.verb, "get");
        assert_eq!(first.input, RouteWasmInput::None);
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(&first.bytes[..4], b"\0asm");
        wasmparser::validate(&first.bytes).unwrap();
    }

    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct static http.get should lower to native capability ABI");
        };
        assert_eq!(artifact.input, RouteWasmInput::None);
        wasmparser::validate(&artifact.bytes).unwrap();
        assert!(artifact
            .bytes
            .windows(PUBLIC_HTTP_TARGET.len())
            .any(|window| window == PUBLIC_HTTP_TARGET.as_bytes()));
        assert!(artifact.bytes.windows(3).any(|window| window == b"get"));
        assert!(artifact
            .bytes
            .windows(b"https://example.com/data".len())
            .any(|window| window == b"https://example.com/data"));
    }

    #[test]
    fn generated_http_wasm_round_trips_through_real_capability_host_abi() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let expected =
            br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#;
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data"])
            );
            Ok(
                br#"{"status":200,"ok":true,"headers":{},"body":"ok","contentType":"text/plain"}"#
                    .to_vec(),
            )
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, expected);
        assert!(result.fuel_consumed > 0);
    }

    #[test]
    fn generated_http_wasm_fails_closed_when_host_denies_capability() {
        let route = parse(
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("direct HTTP route should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, core_lib::ContainerCapabilityKind::Network);
            assert_eq!(request.target, PUBLIC_HTTP_TARGET);
            assert_eq!(request.operation, "get");
            Err("CAPABILITY_DENIED: test denial".into())
        });
        let error = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .expect_err("host denial must fail the whole WASM execution");
        let message = error.to_string();
        assert!(message.contains("WASM ABI violation"));
        assert!(message.contains("CAPABILITY_DENIED"));
    }

    #[test]
    fn aliased_static_http_get_is_native() {
        let route = parse(
            r#":import[http.get as fetch]
               class Route { get(req) { return fetch("https://example.com/"); } }"#,
        );
        assert!(compile_route(&route).is_native());
    }

    #[test]
    fn http_namespace_import_stays_interpreter_fallback() {
        let route = parse(
            r#":import[http]
               class Route { get(req) { return http.get("https://example.com/"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("namespace import must not widen native Network authority");
        };
        assert!(reason.contains("one direct http.get/post/request import"));
    }

    #[test]
    fn dynamic_http_argument_stays_interpreter_fallback() {
        let route = parse(
            r#":import[http.get]
               class Route { post(req) { return get(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("dynamic HTTP argument is outside the v3 native subset");
        };
        assert!(reason.contains("static JSON arguments"));
    }

    #[test]
    fn request_body_passthrough_is_native_v3_input() {
        let route = parse("class Route { post(req) { return req.body; } }");
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("req.body passthrough should lower through Route-WASM v3 input");
        };
        assert_eq!(artifact.input, RouteWasmInput::JsonBody);
        wasmparser::validate(&artifact.bytes).unwrap();
    }

    #[test]
    fn other_runtime_expression_is_explicit_interpreter_fallback() {
        let route = parse("class Route { post(req) { return req.query; } }");
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("unsupported dynamic route must remain interpreter fallback");
        };
        assert!(reason.contains("outside the native Route-WASM v3 subset"));
    }

    #[test]
    fn multiple_methods_are_not_collapsed_into_one_run_export() {
        let route = parse("class Route { get(req) { return true; } post(req) { return false; } }");
        assert!(!compile_route(&route).is_native());
    }
}
