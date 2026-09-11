from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# Keep interpreter and Container-hosted public HTTP on the same capability
# envelope boundaries, not merely the same raw HTTP body policy.
network = Path("engine/crates/core/src/network_broker.rs")
replace_once(
    network,
    '''use serde_json::{Map, Value};''',
    '''use ipc_protocol::MAX_CAPABILITY_PAYLOAD_BYTES;
use serde_json::{Map, Value};''',
    "Network Broker capability envelope import",
)
replace_once(
    network,
    '''            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| http_error(format!("HTTP header name is invalid: {error}")))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| http_error(format!("HTTP header value is invalid: {error}")))?;
            headers.push((name, value));''',
    '''            if value.len() > PUBLIC_HTTP_MAX_HEADER_VALUE_BYTES {
                return Err(http_error(format!(
                    "HTTP header {name:?} exceeds the maximum value size"
                )));
            }
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| http_error(format!("HTTP header name is invalid: {error}")))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| http_error(format!("HTTP header value is invalid: {error}")))?;
            headers.push((name, value));''',
    "Network Broker outbound header value limit",
)
replace_once(
    network,
    '''pub async fn call_public_http(
    operation: &str,
    args: &[Value],
) -> Result<Value, PublicHttpError> {
    let call = parse_http_call(operation, args)?;''',
    '''pub async fn call_public_http(
    operation: &str,
    args: &[Value],
) -> Result<Value, PublicHttpError> {
    let request_envelope = serde_json::to_vec(args)
        .map_err(|error| http_error(format!("encode HTTP capability request: {error}")))?;
    if request_envelope.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err(http_error("HTTP capability request exceeds the capability envelope"));
    }
    let call = parse_http_call(operation, args)?;''',
    "Network Broker request envelope parity",
)
replace_once(
    network,
    '''    Ok(Value::Object(Map::from_iter([
        ("status".into(), Value::Number(status.as_u16().into())),
        ("ok".into(), Value::Bool(status.is_success())),
        ("headers".into(), Value::Object(response_headers)),
        (
            "body".into(),
            Value::String(String::from_utf8_lossy(&body).into_owned()),
        ),
        (
            "contentType".into(),
            content_type.map(Value::String).unwrap_or(Value::Null),
        ),
    ])))''',
    '''    let value = Value::Object(Map::from_iter([
        ("status".into(), Value::Number(status.as_u16().into())),
        ("ok".into(), Value::Bool(status.is_success())),
        ("headers".into(), Value::Object(response_headers)),
        (
            "body".into(),
            Value::String(String::from_utf8_lossy(&body).into_owned()),
        ),
        (
            "contentType".into(),
            content_type.map(Value::String).unwrap_or(Value::Null),
        ),
    ]));
    let response_envelope = serde_json::to_vec(&value)
        .map_err(|error| http_error(format!("encode HTTP capability response: {error}")))?;
    if response_envelope.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err(http_error("HTTP response exceeds the capability envelope"));
    }
    Ok(value)''',
    "Network Broker response envelope parity",
)
replace_once(
    network,
    '''    #[test]
    fn logical_target_is_fixed_not_a_socket_or_host() {''',
    '''    #[test]
    fn oversized_capability_request_envelope_fails_before_network_access() {
        let oversized = Value::String("x".repeat(MAX_CAPABILITY_PAYLOAD_BYTES));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(call_public_http("post", &[Value::String("https://example.com".into()), oversized]))
            .expect_err("capability envelope must fail before dispatch");
        assert!(error.message.contains("capability envelope"));
    }

    #[test]
    fn logical_target_is_fixed_not_a_socket_or_host() {''',
    "Network Broker envelope test",
)

compiler = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    compiler,
    '''use core_lib::CONTAINER_MAX_EXECUTION_INPUT_BYTES;''',
    '''use core_lib::{
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET,
};''',
    "native HTTP compiler constants",
)
replace_once(
    compiler,
    '''use crate::ast::{Expr, RouteFile, Statement};''',
    '''use crate::ast::{Expr, ImportTarget, RouteFile, Statement};''',
    "native HTTP ImportTarget",
)
replace_once(
    compiler,
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 2;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 2;''',
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 3;''',
    "Route-WASM ABI v3",
)
replace_once(
    compiler,
    '''/// The first slice is intentionally strict: one route method whose result is a
/// JSON-literal REL value and no imported/helper execution. This already
/// produces real WebAssembly executed by Wasmtime. Dynamic request expressions,
/// local functions and host capabilities remain explicit interpreter fallback
/// until their individual ABI lowering is implemented.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    if !file.imports.is_empty() {
        return fallback("route imports are not WASM-native yet");
    }
    if !file.functions.is_empty() {''',
    '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no helper functions. In addition to literals and req.body passthrough, ABI v3
/// permits one directly imported `http.get/post/request` function with fully
/// static JSON arguments. Namespace imports stay interpreter-only so native
/// grants remain operation-exact rather than widening authority.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let http_import = match file.imports.as_slice() {
        [] => None,
        [import] => match direct_http_import(import) {
            Some(import) => Some(import),
            None => {
                return fallback(
                    "native Route-WASM v3 only supports one direct http.get/post/request import",
                )
            }
        },
        _ => {
            return fallback(
                "native Route-WASM v3 supports at most one direct host capability import",
            )
        }
    };
    if !file.functions.is_empty() {''',
    "native HTTP compiler admission",
)
replace_once(
    compiler,
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
    };''',
    '''    let (bytes, input) = if let Some((binding, operation)) = http_import.as_ref() {
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
    };''',
    "native HTTP call lowering",
)
replace_once(
    compiler,
    '''fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {
    RouteWasmCompilation::InterpreterFallback {
        reason: reason.into(),
    }
}

fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {''',
    '''fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {
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

fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {''',
    "native HTTP import/call matchers",
)
replace_once(
    compiler,
    '''fn encode_input_echo_module() -> Vec<u8> {''',
    '''fn encode_public_http_module(operation: &str, payload: &[u8]) -> Vec<u8> {
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

fn encode_input_echo_module() -> Vec<u8> {''',
    "public HTTP WASM module",
)
replace_once(
    compiler,
    '''    #[test]
    fn request_body_passthrough_is_native_v2_input() {''',
    '''    #[test]
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
    fn request_body_passthrough_is_native_v3_input() {''',
    "native HTTP compiler tests",
)
replace_once(
    compiler,
    '''            panic!("req.body passthrough should lower through Route-WASM v2 input");''',
    '''            panic!("req.body passthrough should lower through Route-WASM v3 input");''',
    "req.body v3 test message",
)
replace_once(
    compiler,
    '''        assert!(reason.contains("outside the native Route-WASM v2 subset"));''',
    '''        assert!(reason.contains("outside the native Route-WASM v3 subset"));''',
    "v3 fallback reason test",
)

# Network work becomes visible to the scheduler whenever native grants include
# Network authority.
discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''    AppState, ContainerAuthorizedExecution, ContainerExecutionIdentity, ContainerWorkCost,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    '''    AppState, ContainerAuthorizedExecution, ContainerCapabilityKind,
    ContainerExecutionIdentity, ContainerWorkCost, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    "native Network cost import",
)
replace_once(
    discovery,
    '''    let identity = ContainerExecutionIdentity {
        runtime_image: &plan.runtime_image,
        source_id: plan.source_id.as_str(),
        environment: NATIVE_ROUTE_ENVIRONMENT_PROFILE,
    };
    let output = match state''',
    '''    let network_cost = u64::from(
        grants
            .iter()
            .any(|grant| grant.kind == ContainerCapabilityKind::Network),
    );
    let identity = ContainerExecutionIdentity {
        runtime_image: &plan.runtime_image,
        source_id: plan.source_id.as_str(),
        environment: NATIVE_ROUTE_ENVIRONMENT_PROFILE,
    };
    let output = match state''',
    "native Network scheduler cost",
)
replace_once(
    discovery,
    '''                io: 0,
                network: 0,
            },''',
    '''                io: 0,
                network: network_cost,
            },''',
    "native Network declared cost",
)

# Verify RELC, typed requirements, native artifact, and grant lowering agree for
# one direct least-privilege operation.
relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''    #[test]
    fn runtime_image_id_changes_when_settings_change() {''',
    '''    #[test]
    fn direct_http_get_links_native_artifact_with_exact_network_requirement() {
        let routes = vec![PhysicalRelSource::new(
            RelSourceKind::Route,
            "fetch",
            "api/fetch.route",
            r#":import[http.get]
               class Route { get(req) { return get("https://example.com/data"); } }"#,
        )];
        let image =
            compile_runtime_image("server Main {}", routes, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();
        assert!(image.route_wasm_artifact(route).is_some());
        let requirements = image.capability_requirements(route).unwrap();
        assert_eq!(
            requirements,
            &BTreeSet::from([RuntimeCapabilityRequirement::PublicHttp {
                operation: "get".into(),
            }])
        );
        let grants = image.container_capability_grants(route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, core_lib::ContainerCapabilityKind::Network);
        assert_eq!(grants[0].target, core_lib::PUBLIC_HTTP_TARGET);
        assert_eq!(grants[0].operations, vec!["get"]);
    }

    #[test]
    fn runtime_image_id_changes_when_settings_change() {''',
    "RELC native HTTP agreement test",
)

# ABI and security boundary documentation.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Route-WASM ABI v2 additionally defines invocation input for native `return req.body;` routes as the JSON encoding of the evaluator-visible body value; bodies above the Container execution-input ceiling fail with `413 Payload Too Large` rather than escaping to the interpreter.''',
    '''Route-WASM ABI v3 retains the JSON invocation input for native `return req.body;` routes and adds real Controller-authorized `Network/public-http` capability calls for one directly imported `http.get`, `http.post`, or `http.request` operation when all call arguments are static JSON. Namespace `http` imports and dynamic HTTP arguments remain explicit interpreter fallback, preventing native compilation from widening the operation grant. Bodies above the Container execution-input ceiling fail with `413 Payload Too Large` rather than escaping to the interpreter.''',
    "Route-WASM v3 documentation",
)
