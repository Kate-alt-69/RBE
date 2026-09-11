//! Native REL `.route` -> WebAssembly lowering.
//!
//! This compiler deliberately exposes a small executable subset first instead
//! of disguising interpreter execution as WASM. Unsupported REL constructs are
//! classified as an explicit interpreter fallback. Native artifacts use the
//! RBE worker ABI and return JSON bytes through `rbe.output_write`.

use core_lib::CONTAINER_MAX_EXECUTION_INPUT_BYTES;
use sha2::{Digest, Sha256};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::ast::{Expr, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 2;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 2;
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
/// The first slice is intentionally strict: one route method whose result is a
/// JSON-literal REL value and no imported/helper execution. This already
/// produces real WebAssembly executed by Wasmtime. Dynamic request expressions,
/// local functions and host capabilities remain explicit interpreter fallback
/// until their individual ABI lowering is implemented.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    if !file.imports.is_empty() {
        return fallback("route imports are not WASM-native yet");
    }
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
    let (bytes, input) = if let Some(value) = static_json(expr) {
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
        return fallback("route return value is outside the native Route-WASM v2 subset");
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
    fn request_body_passthrough_is_native_v2_input() {
        let route = parse("class Route { post(req) { return req.body; } }");
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("req.body passthrough should lower through Route-WASM v2 input");
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
        assert!(reason.contains("outside the native Route-WASM v2 subset"));
    }

    #[test]
    fn multiple_methods_are_not_collapsed_into_one_run_export() {
        let route = parse("class Route { get(req) { return true; } post(req) { return false; } }");
        assert!(!compile_route(&route).is_native());
    }
}
