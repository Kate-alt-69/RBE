from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


core = Path("engine/crates/core/src/lib.rs")
replace_once(
    core,
    '''pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost,
};''',
    '''pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost, MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    "Container input limit re-export",
)

compiler = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    compiler,
    '''use sha2::{Digest, Sha256};''',
    '''use core_lib::CONTAINER_MAX_EXECUTION_INPUT_BYTES;
use sha2::{Digest, Sha256};''',
    "native compiler Container input limit",
)
replace_once(
    compiler,
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 1;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 1;''',
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 2;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 2;''',
    "Route-WASM ABI v2",
)
replace_once(
    compiler,
    '''#[derive(Debug, Clone)]
pub struct RouteWasmArtifact {
    pub verb: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}''',
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}''',
    "Route-WASM input contract",
)
replace_once(
    compiler,
    '''    let Some(value) = static_json(expr) else {
        return fallback("route return value depends on runtime REL evaluation");
    };
    let output = match serde_json::to_vec(&value) {
        Ok(output) => output,
        Err(error) => {
            return fallback(format!("static route result could not be encoded: {error}"))
        }
    };
    if output.len() > MAX_STATIC_OUTPUT_BYTES {
        return fallback("static route result exceeds the WASM execution output limit");
    }

    let bytes = encode_static_json_module(&output);
    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
    })''',
    '''    let (bytes, input) = if let Some(value) = static_json(expr) {
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
    })''',
    "native request body lowering",
)
replace_once(
    compiler,
    '''fn static_json(expr: &Expr) -> Option<serde_json::Value> {''',
    '''fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {
    let Some(parameter) = parameter else {
        return false;
    };
    matches!(
        expr,
        Expr::Member(target, field)
            if field == "body" && matches!(target.as_ref(), Expr::Ident(name) if name == parameter)
    )
}

fn static_json(expr: &Expr) -> Option<serde_json::Value> {''',
    "request body matcher",
)
replace_once(
    compiler,
    '''fn encode_static_json_module(output: &[u8]) -> Vec<u8> {''',
    '''fn encode_input_echo_module() -> Vec<u8> {
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

    let pages = CONTAINER_MAX_EXECUTION_INPUT_BYTES.max(1).div_ceil(WASM_PAGE_BYTES) as u64;
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

fn encode_static_json_module(output: &[u8]) -> Vec<u8> {''',
    "input echo module",
)
replace_once(
    compiler,
    '''        assert_eq!(first.verb, "get");
        assert_eq!(first.bytes, second.bytes);''',
    '''        assert_eq!(first.verb, "get");
        assert_eq!(first.input, RouteWasmInput::None);
        assert_eq!(first.bytes, second.bytes);''',
    "static route input mode test",
)
replace_once(
    compiler,
    '''    #[test]
    fn runtime_expression_is_explicit_interpreter_fallback() {
        let route = parse("class Route { post(req) { return req.body; } }");
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("dynamic route must not pretend to be native WASM");
        };
        assert!(reason.contains("runtime REL evaluation"));
    }''',
    '''    #[test]
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
    }''',
    "Route-WASM v2 tests",
)

runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''use crate::wasm_compiler::{
    RouteWasmArtifact, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};''',
    '''use crate::wasm_compiler::{
    RouteWasmArtifact, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};''',
    "Runtime Image ABI anchor",
)
# No textual change is needed above; the replacement intentionally asserts the
# ABI constants remain part of image hashing after the compiler bump.

discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''use core_lib::{
    AppState, ContainerAuthorizedExecution, ContainerExecutionIdentity, ContainerWorkCost,
};''',
    '''use core_lib::{
    AppState, ContainerAuthorizedExecution, ContainerExecutionIdentity, ContainerWorkCost,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    "native input limit import",
)
replace_once(
    discovery,
    '''use crate::wasm_compiler::RouteWasmArtifact;''',
    '''use crate::wasm_compiler::{RouteWasmArtifact, RouteWasmInput};''',
    "Route-WASM input type import",
)
replace_once(
    discovery,
    '''async fn execute_native_route(
    plan: &NativeRoutePlan,
    image: &RuntimeImage,
    state: &AppState,
    path: &str,
) -> Response {''',
    '''async fn execute_native_route(
    plan: &NativeRoutePlan,
    image: &RuntimeImage,
    state: &AppState,
    path: &str,
    input: Vec<u8>,
) -> Response {''',
    "native route invocation input",
)
replace_once(
    discovery,
    '''            grants: Vec::new(),
            input: Vec::new(),''',
    '''            grants: Vec::new(),
            input,''',
    "Container native route input",
)
replace_once(
    discovery,
    '''    let args = if takes_request {''',
    '''    let args = if takes_request {''',
    "request args anchor",
)
replace_once(
    discovery,
    '''    if let Some(plan) = native_plan.as_deref() {
        return execute_native_route(plan, image.as_ref(), &state, &path).await;
    }''',
    '''    if let Some(plan) = native_plan.as_deref() {
        let input = match plan.artifact.input {
            RouteWasmInput::None => Vec::new(),
            RouteWasmInput::JsonBody => {
                let body = args
                    .first()
                    .and_then(|request| match request {
                        Value::Object(fields) => fields.get("body"),
                        _ => None,
                    })
                    .unwrap_or(&Value::Null);
                let input = match serde_json::to_vec(&value_to_json(body)) {
                    Ok(input) => input,
                    Err(error) => {
                        tracing::error!(error = %error, path = %path, "encode native req.body input");
                        return request_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "native route input could not be encoded",
                        );
                    }
                };
                if input.len() > CONTAINER_MAX_EXECUTION_INPUT_BYTES {
                    return request_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "native route body exceeds the Container execution input limit",
                    );
                }
                input
            }
        };
        return execute_native_route(plan, image.as_ref(), &state, &path, input).await;
    }''',
    "native body input preparation",
)

# Docs: ABI v2 and current dynamic native coverage.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Those bytes are the eligible payload for Container artifact registration.''',
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v2 additionally defines invocation input for native `return req.body;` routes as the JSON encoding of the evaluator-visible body value; bodies above the Container execution-input ceiling fail with `413 Payload Too Large` rather than escaping to the interpreter. Those bytes are the eligible payload for Container artifact registration.''',
    "Route-WASM v2 documentation",
)
