from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# wasm_compiler.rs: raw req.body JSON now enters the guest. Generated WASM
# builds the one-argument JSON capability envelope itself before host crossing.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    """    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};""",
    """    FunctionSection, ImportSection, MemArg, MemorySection, MemoryType, Module, TypeSection,
    ValType,
};""",
    "wasm MemArg import",
)

text = replace_once(
    text,
    """/// Generation 15 lowers request-independent `const`/`if` control flow in
/// linked Module wrappers before one exact terminal host call. Static execution
/// stays host-call free and bounded; the selected capability still executes in
/// the generated WASM guest. ABI v3 stays unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 15;""",
    """/// Generation 16 moves dynamic one-value capability payload construction
/// into the WASM guest. The host supplies raw JSON `req.body`; guest code wraps
/// it as `[req.body]` before the exact capability call. ABI v3 stays unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 16;""",
    "compiler generation 16",
)

text = replace_once(
    text,
    """    /// Invocation input is a JSON argument array containing exactly one
    /// evaluator-visible `req.body` value. The guest forwards these bytes to an
    /// already-authorized host capability without parsing or widening them.
    JsonBodyCapabilityArgument,""",
    """    /// Invocation input is the raw JSON encoding of evaluator-visible
    /// `req.body`. The guest constructs the single-argument JSON array around
    /// these bytes before crossing the capability ABI.
    JsonBodyCapabilityValue,""",
    "dynamic capability input semantics",
)

text = replace_once(
    text,
    """/// generation v15 keeps ABI v3 and extends immutable linked Module lowering
/// through request-independent `const`, pure expression, and `if` prefixes that
/// resolve to one exact terminal host call. It retains generation 14's multiple
/// host imports/helpers and generation 13's per-method artifacts. Dynamic control
/// flow, namespace host calls, ambiguous bindings, and nested host-call chains
/// remain interpreter-only.""",
    """/// generation v16 keeps ABI v3 and moves dynamic single-body capability
/// argument construction into the guest. It retains generation 15's static
/// Module control flow and generation 13's per-method artifacts. Dynamic value
/// transforms, namespace host calls, ambiguous bindings, and nested host-call
/// chains remain interpreter-only.""",
    "compiler generation 16 contract",
)

text = text.replace("native Route-WASM v13 found ambiguous import binding", "native Route-WASM v16 found ambiguous import binding")
text = text.replace("native Route-WASM v13 supports only exact direct", "native Route-WASM v16 supports only exact direct")

text = replace_once(
    text,
    """            LoweredCapabilityPayload::JsonBodySingleArgument => (
                encode_input_capability_call_module(call.kind, &call.target, &call.operation),
                RouteWasmInput::JsonBodyCapabilityArgument,
            ),""",
    """            LoweredCapabilityPayload::JsonBodySingleArgument => (
                encode_input_capability_call_module(call.kind, &call.target, &call.operation),
                RouteWasmInput::JsonBodyCapabilityValue,
            ),""",
    "guest-built dynamic capability input variant",
)

old_encoder = r'''fn encode_input_capability_call_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
) -> Vec<u8> {
    // Invocation input is already the exact JSON argument array `[req.body]`.
    // Keeping JSON encoding at the HTTP boundary means the guest never needs a
    // JSON parser and cannot reinterpret or widen the capability request.
    let target = target.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = (operation_offset + operation.len() + 15) & !15usize;
    let response_offset = (payload_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES + 15) & !15usize;
    let memory_bytes = response_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES;

    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
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

    let mut imports = ImportSection::new();
    imports.import("rbe", "input_len", EntityType::Function(0));
    imports.import("rbe", "input_read", EntityType::Function(1));
    imports.import("rbe", "capability_call", EntityType::Function(2));
    imports.import("rbe", "capability_response_len", EntityType::Function(0));
    imports.import("rbe", "capability_response_read", EntityType::Function(1));
    imports.import("rbe", "output_write", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(0);

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
    exports.export("run", ExportKind::Func, 6);

    let mut run = Function::new([(2, ValType::I32)]);
    run.instructions()
        .call(0)
        .local_set(0)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .call(1)
        .drop()
        .i32_const(kind.abi_code())
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .call(2)
        .drop()
        .call(3)
        .local_set(1)
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(4)
        .drop()
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(5)
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
'''
new_encoder = r'''fn encode_input_capability_call_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
) -> Vec<u8> {
    // Invocation input is raw JSON for req.body. The guest owns capability
    // payload composition: it writes '[' + input + ']' and crosses the host ABI
    // only after constructing the exact one-argument JSON envelope itself.
    let target = target.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = (operation_offset + operation.len() + 15) & !15usize;
    let input_offset = payload_offset + 1;
    let response_offset = (payload_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES + 15) & !15usize;
    let memory_bytes = response_offset + CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES;

    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
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

    let mut imports = ImportSection::new();
    imports.import("rbe", "input_len", EntityType::Function(0));
    imports.import("rbe", "input_read", EntityType::Function(1));
    imports.import("rbe", "capability_call", EntityType::Function(2));
    imports.import("rbe", "capability_response_len", EntityType::Function(0));
    imports.import("rbe", "capability_response_read", EntityType::Function(1));
    imports.import("rbe", "output_write", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(0);

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
    exports.export("run", ExportKind::Func, 6);

    let mut run = Function::new([(2, ValType::I32)]);
    run.instructions()
        .call(0)
        .local_set(0)
        .i32_const(input_offset as i32)
        .local_get(0)
        .call(1)
        .drop()
        // Store the closing ']' immediately after the dynamic input bytes.
        .i32_const(input_offset as i32)
        .local_get(0)
        .i32_add()
        .i32_const(b']' as i32)
        .i32_store8(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        })
        .i32_const(kind.abi_code())
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .i32_const(2)
        .i32_add()
        .call(2)
        .drop()
        .call(3)
        .local_set(1)
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(4)
        .drop()
        .i32_const(response_offset as i32)
        .local_get(1)
        .call(5)
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
        [b'['],
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
'''
text = replace_once(text, old_encoder, new_encoder, "guest capability payload encoder")

# Existing dynamic linked capability test must now feed raw req.body JSON rather
# than a host-built argument array, proving the guest constructs the envelope.
text = replace_once(
    text,
    "assert_eq!(artifact.input, RouteWasmInput::JsonBodyCapabilityArgument);",
    "assert_eq!(artifact.input, RouteWasmInput::JsonBodyCapabilityValue);",
    "dynamic capability input assertion",
)
text = replace_once(
    text,
    '                br#"[\\"users/kate.json\\"]"#,.decode()' if False else '                br#"[\\"users/kate.json\\"]"#,',
    '                br#"\\"users/kate.json\\""#,',
    "dynamic capability raw body executor input",
)

anchor = """    #[test]
    fn linked_module_video_call_uses_canonical_module_owner() {"""
if anchor not in text:
    raise SystemExit("generation 16 guest payload test anchor missing")
new_test = r'''    #[test]
    fn dynamic_object_body_is_wrapped_into_capability_payload_inside_guest() {
        let module = parse_module(
            r#":import[storage.commit as commitEntry]
               export function save(value) { return commitEntry(value); }"#,
        );
        let links = link_module_function("save", "accounts.cache", &module, "save");
        let route = parse(
            r#":import["./module/accounts/cache".save]
               class Route { post(req) { return save(req.body); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("raw dynamic body should compile through guest payload construction");
        };
        assert_eq!(artifact.input, RouteWasmInput::JsonBodyCapabilityValue);
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "commit");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!([{ "id": "kate", "active": true }])
            );
            Ok(br#"{"ok":true}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                br#"{"id":"kate","active":true}"#,
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

'''
text = text.replace(anchor, new_test + anchor, 1)
path.write_text(text)


# ---------------------------------------------------------------------------
# discovery.rs: serialize only raw req.body for capability guest input. Host no
# longer composes the capability argument array.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/discovery.rs")
text = path.read_text()
text = replace_once(
    text,
    """            RouteWasmInput::JsonBodyCapabilityArgument => {
                let body = args
                    .first()
                    .and_then(|request| match request {
                        Value::Object(fields) => fields.get("body"),
                        _ => None,
                    })
                    .unwrap_or(&Value::Null);
                let input = match serde_json::to_vec(&vec![value_to_json(body)]) {
                    Ok(input) => input,
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            path = %path,
                            "encode native req.body capability argument"
                        );
                        return request_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "native route capability input could not be encoded",
                        );
                    }
                };
                let limit =
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES.min(CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES);
                if input.len() > limit {
                    return request_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "native route capability argument exceeds the Container capability input limit",
                    );
                }
                input
            }""",
    """            RouteWasmInput::JsonBodyCapabilityValue => {
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
                        tracing::error!(
                            error = %error,
                            path = %path,
                            "encode raw native req.body capability input"
                        );
                        return request_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "native route capability input could not be encoded",
                        );
                    }
                };
                // Guest adds '[' and ']' around raw JSON before capability_call.
                let limit = CONTAINER_MAX_EXECUTION_INPUT_BYTES.min(
                    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES.saturating_sub(2),
                );
                if input.len() > limit {
                    return request_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "native route capability value exceeds the Container capability input limit",
                    );
                }
                input
            }""",
    "discovery raw capability guest input",
)
path.write_text(text)
