from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# Route-WASM generation 8: dynamic Storage write payloads keep ABI v3. The
# HTTP layer supplies raw JSON req.body and the guest composes the authorized
# write descriptor envelope around it.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,\n    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,\n};""",
    """    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,\n    FunctionSection, ImportSection, MemArg, MemorySection, MemoryType, Module, TypeSection,\n    ValType,\n};""",
    "wasm MemArg import",
)

replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """/// Generation 7 adds exact Module-owned Environment Storage calls to the\n/// existing linked Module authority boundary. Direct Route-to-Storage remains\n/// unreachable and capability ABI v3 stays stable.\npub const ROUTE_WASM_COMPILER_VERSION: u32 = 7;""",
    """/// Generation 8 lets one linked Module storage.write descriptor receive the\n/// evaluator-visible req.body unchanged. The WASM guest builds the capability\n/// JSON envelope; direct Route-to-Storage remains unreachable and ABI v3 stays stable.\npub const ROUTE_WASM_COMPILER_VERSION: u32 = 8;""",
    "compiler generation 8",
)

replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """pub enum RouteWasmInput {\n    None,\n    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`\n    /// value. This keeps strings/null/objects/arrays semantically identical.\n    JsonBody,\n}""",
    """pub enum RouteWasmInput {\n    None,\n    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`\n    /// value. This keeps strings/null/objects/arrays semantically identical.\n    JsonBody,\n    /// Raw JSON req.body used by a guest-built capability payload. The limit is\n    /// compiler-derived after accounting for the static capability envelope.\n    JsonBodyCapability { max_bytes: usize },\n}""",
    "dynamic capability input mode",
)

old_linked_branch = r'''    } else if let Some((binding, linked)) = linked_import.as_ref() {
        let (kind, target, operation, payload) =
            match lower_linked_module_capability_call(binding, linked, expr) {
                Ok(call) => call,
                Err(reason) => return fallback(reason),
            };
        (
            encode_capability_call_module(kind, &target, &operation, &payload),
            RouteWasmInput::None,
        )
'''
new_linked_branch = r'''    } else if let Some((binding, linked)) = linked_import.as_ref() {
        let call = match lower_linked_module_capability_call(
            binding,
            linked,
            expr,
            method.param_name.as_deref(),
        ) {
            Ok(call) => call,
            Err(reason) => return fallback(reason),
        };
        match call.payload {
            LoweredCapabilityPayload::Static(payload) => (
                encode_capability_call_module(call.kind, &call.target, &call.operation, &payload),
                RouteWasmInput::None,
            ),
            LoweredCapabilityPayload::JsonBodyStorageWrite {
                prefix,
                suffix,
                max_body_bytes,
            } => (
                encode_json_body_storage_write_module(
                    call.kind,
                    &call.target,
                    &call.operation,
                    &prefix,
                    &suffix,
                ),
                RouteWasmInput::JsonBodyCapability {
                    max_bytes: max_body_bytes,
                },
            ),
        }
'''
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    old_linked_branch,
    new_linked_branch,
    "dynamic linked capability branch",
)

old_lower = r'''fn lower_linked_module_capability_call(
    route_binding: &str,
    linked: &LinkedModuleFunction,
    route_expr: &Expr,
) -> Result<(ContainerCapabilityKind, String, String, Vec<u8>), String> {
    let route_args = static_direct_call(route_binding, route_expr).ok_or_else(|| {
        "native linked Module calls require the imported function as the return value with static JSON arguments".to_string()
    })?;
    if route_args.len() != linked.function.params.len() {
        return Err(format!(
            "native linked Module call arity mismatch: Route supplied {}, Module function expects {}",
            route_args.len(),
            linked.function.params.len()
        ));
    }
    let [Statement::Return(module_expr)] = linked.function.body.as_slice() else {
        return Err(
            "native linked Module function must contain exactly one return statement".into(),
        );
    };
    let [module_import] = linked.imports.as_slice() else {
        return Err(
            "native linked Module function requires exactly one direct host capability import"
                .into(),
        );
    };
    let host = direct_linked_capability_import(module_import, &linked.owner).ok_or_else(|| {
        "native linked Module import must be one exact HTTP, Video, Service, or Storage function"
            .to_string()
    })?;
    let Expr::Call(target, host_args) = module_expr else {
        return Err("native linked Module return must directly call its host import".into());
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == &host.binding) {
        return Err("native linked Module return must call the imported host binding".into());
    }

    let bindings = linked
        .function
        .params
        .iter()
        .cloned()
        .zip(route_args)
        .collect::<BTreeMap<_, _>>();
    let args = if host.kind == ContainerCapabilityKind::Storage && host.operation == "write" {
        lower_storage_write_args(host_args, &bindings)?
    } else {
        host_args
            .iter()
            .map(|argument| static_json_with_bindings(argument, &bindings))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "native linked Module host arguments must resolve to static JSON values".to_string()
            })?
    };
    let payload = serde_json::to_vec(&args)
        .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
    if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err("native linked Module argument payload exceeds the capability envelope".into());
    }
    Ok((host.kind, host.target, host.operation, payload))
}
'''
new_lower = r'''#[derive(Debug, Clone)]
enum LoweredCapabilityPayload {
    Static(Vec<u8>),
    JsonBodyStorageWrite {
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        max_body_bytes: usize,
    },
}

#[derive(Debug, Clone)]
struct LoweredCapabilityCall {
    kind: ContainerCapabilityKind,
    target: String,
    operation: String,
    payload: LoweredCapabilityPayload,
}

fn lower_linked_module_capability_call(
    route_binding: &str,
    linked: &LinkedModuleFunction,
    route_expr: &Expr,
    request_parameter: Option<&str>,
) -> Result<LoweredCapabilityCall, String> {
    let Expr::Call(route_target, route_args) = route_expr else {
        return Err("native linked Module calls require the imported function as the return value".into());
    };
    if !matches!(route_target.as_ref(), Expr::Ident(name) if name == route_binding) {
        return Err("native linked Module return must call the imported Module binding".into());
    }
    if route_args.len() != linked.function.params.len() {
        return Err(format!(
            "native linked Module call arity mismatch: Route supplied {}, Module function expects {}",
            route_args.len(),
            linked.function.params.len()
        ));
    }
    let [Statement::Return(module_expr)] = linked.function.body.as_slice() else {
        return Err(
            "native linked Module function must contain exactly one return statement".into(),
        );
    };
    let [module_import] = linked.imports.as_slice() else {
        return Err(
            "native linked Module function requires exactly one direct host capability import"
                .into(),
        );
    };
    let host = direct_linked_capability_import(module_import, &linked.owner).ok_or_else(|| {
        "native linked Module import must be one exact HTTP, Video, Service, or Storage function"
            .to_string()
    })?;
    let Expr::Call(target, host_args) = module_expr else {
        return Err("native linked Module return must directly call its host import".into());
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == &host.binding) {
        return Err("native linked Module return must call the imported host binding".into());
    }

    if let Some(route_args) = route_args.iter().map(static_json).collect::<Option<Vec<_>>>() {
        let bindings = linked
            .function
            .params
            .iter()
            .cloned()
            .zip(route_args)
            .collect::<BTreeMap<_, _>>();
        let args = if host.kind == ContainerCapabilityKind::Storage && host.operation == "write" {
            lower_storage_write_args(host_args, &bindings)?
        } else {
            host_args
                .iter()
                .map(|argument| static_json_with_bindings(argument, &bindings))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    "native linked Module host arguments must resolve to static JSON values".to_string()
                })?
        };
        let payload = serde_json::to_vec(&args)
            .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err("native linked Module argument payload exceeds the capability envelope".into());
        }
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::Static(payload),
        });
    }

    if host.kind != ContainerCapabilityKind::Storage || host.operation != "write" {
        return Err(
            "dynamic linked Module capability arguments currently support only storage.write data[req.body]"
                .into(),
        );
    }
    if linked.function.params.len() != 1
        || route_args.len() != 1
        || !returns_request_body(request_parameter, &route_args[0])
    {
        return Err(
            "dynamic storage.write requires exactly one req.body Route argument passed unchanged into one Module parameter"
                .into(),
        );
    }
    let (prefix, suffix, max_body_bytes) =
        lower_dynamic_storage_write_args(host_args, &linked.function.params[0])?;
    Ok(LoweredCapabilityCall {
        kind: host.kind,
        target: host.target,
        operation: host.operation,
        payload: LoweredCapabilityPayload::JsonBodyStorageWrite {
            prefix,
            suffix,
            max_body_bytes,
        },
    })
}
'''
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    old_lower,
    new_lower,
    "dynamic linked capability lowering",
)

# Add dynamic descriptor normalization beside the static Storage lowering.
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text(encoding="utf-8")
anchor = "\nfn storage_descriptor_expr(expr: &Expr) -> Option<(&str, &Expr)> {"
pos = text.find(anchor)
if pos < 0:
    raise SystemExit("dynamic Storage helper insertion anchor missing")
helper = r'''

fn lower_dynamic_storage_write_args(
    args: &[Expr],
    dynamic_parameter: &str,
) -> Result<(Vec<u8>, Vec<u8>, usize), String> {
    if args.len() != 4 {
        return Err(
            "dynamic storage.write requires encode[], data[], write[], and level[] descriptors"
                .into(),
        );
    }
    let mut descriptors = BTreeMap::<String, &Expr>::new();
    for argument in args {
        let (kind, value) = storage_descriptor_expr(argument).ok_or_else(|| {
            "dynamic storage.write arguments must use descriptor brackets".to_string()
        })?;
        if descriptors.insert(kind.to_string(), value).is_some() {
            return Err(format!(
                "dynamic storage.write descriptor {kind}[] was provided more than once"
            ));
        }
    }
    let take = |values: &mut BTreeMap<String, &Expr>, name: &str| {
        values
            .remove(name)
            .ok_or_else(|| format!("dynamic storage.write is missing {name}[]"))
    };
    let mut values = descriptors;
    let encoding_expr = take(&mut values, "encode")?;
    let data_expr = take(&mut values, "data")?;
    let path_expr = take(&mut values, "write")?;
    let level_expr = take(&mut values, "level")?;

    if !matches!(data_expr, Expr::Ident(name) if name == dynamic_parameter) {
        return Err(
            "dynamic storage.write data[] must contain the unchanged Module parameter receiving req.body"
                .into(),
        );
    }
    let Some(serde_json::Value::String(path)) = static_json(path_expr) else {
        return Err("dynamic storage.write write[] must be a static symbolic $$/ path".into());
    };
    if !path.starts_with("$$/") {
        return Err("dynamic storage.write write[] must contain a symbolic $$/ path".into());
    }
    let Some(serde_json::Value::String(encoding)) = static_json(encoding_expr) else {
        return Err("dynamic storage.write encode[] must be a static string".into());
    };
    let level = match static_json(level_expr) {
        Some(serde_json::Value::Number(value)) => value
            .as_f64()
            .filter(|value| value.fract() == 0.0 && (1.0..=3.0).contains(value)),
        _ => None,
    }
    .ok_or_else(|| "dynamic storage.write level[] must be 1, 2, or 3".to_string())?
        as u64;

    let path_json = serde_json::to_string(&path)
        .map_err(|error| format!("encode dynamic storage.write path: {error}"))?;
    let encoding_json = serde_json::to_string(&encoding)
        .map_err(|error| format!("encode dynamic storage.write encoding: {error}"))?;
    let prefix = format!(r#"[{{"path":{path_json},"data":"#).into_bytes();
    let suffix = format!(r#", "encoding":{encoding_json},"level":{level}}}]"#)
        .into_bytes();
    let envelope_bytes = prefix
        .len()
        .checked_add(suffix.len())
        .ok_or_else(|| "dynamic storage.write envelope length overflow".to_string())?;
    let max_body_bytes = CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES
        .checked_sub(envelope_bytes)
        .ok_or_else(|| "dynamic storage.write static envelope exceeds capability limit".to_string())?;
    Ok((prefix, suffix, max_body_bytes))
}
'''
text = text[:pos] + helper + text[pos:]
path.write_text(text, encoding="utf-8")

# Add guest encoder before ordinary req.body echo encoder.
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text(encoding="utf-8")
anchor = "\nfn encode_input_echo_module() -> Vec<u8> {"
pos = text.find(anchor)
if pos < 0:
    raise SystemExit("dynamic guest encoder insertion anchor missing")
encoder = r'''

fn encode_json_body_storage_write_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
    prefix: &[u8],
    suffix: &[u8],
) -> Vec<u8> {
    let target = target.as_bytes();
    let operation = operation.as_bytes();
    let target_offset = 0usize;
    let operation_offset = target_offset + target.len();
    let payload_offset = (operation_offset + operation.len() + 15) & !15usize;
    let input_offset = payload_offset + prefix.len();
    let response_offset = (input_offset
        + CONTAINER_MAX_EXECUTION_INPUT_BYTES
        + suffix.len()
        + 15)
        & !15usize;
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
    let instructions = run.instructions();
    instructions
        .call(0)
        .local_set(0)
        .i32_const(input_offset as i32)
        .local_get(0)
        .call(1)
        .drop();
    for (index, byte) in suffix.iter().copied().enumerate() {
        instructions
            .i32_const(input_offset as i32)
            .local_get(0)
            .i32_add()
            .i32_const(index as i32)
            .i32_add()
            .i32_const(byte as i32)
            .i32_store8(MemArg {
                offset: 0,
                align: 0,
                memory_index: 0,
            });
    }
    instructions
        .i32_const(kind.abi_code())
        .i32_const(target_offset as i32)
        .i32_const(target.len() as i32)
        .i32_const(operation_offset as i32)
        .i32_const(operation.len() as i32)
        .i32_const(payload_offset as i32)
        .local_get(0)
        .i32_const((prefix.len() + suffix.len()) as i32)
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
        prefix.iter().copied(),
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
text = text[:pos] + encoder + text[pos:]
path.write_text(text, encoding="utf-8")

# Dynamic req.body input is still serialized at the HTTP boundary, but now the
# compiler supplies the exact body budget left after the static Storage envelope.
replace_once(
    "engine/crates/route-engine/src/discovery.rs",
    """            RouteWasmInput::JsonBody => {\n                let body = args\n                    .first()\n                    .and_then(|request| match request {\n                        Value::Object(fields) => fields.get(\"body\"),\n                        _ => None,\n                    })\n                    .unwrap_or(&Value::Null);\n                let input = match serde_json::to_vec(&value_to_json(body)) {\n                    Ok(input) => input,\n                    Err(error) => {\n                        tracing::error!(error = %error, path = %path, \"encode native req.body input\");\n                        return request_error(\n                            StatusCode::INTERNAL_SERVER_ERROR,\n                            \"native route input could not be encoded\",\n                        );\n                    }\n                };\n                if input.len() > CONTAINER_MAX_EXECUTION_INPUT_BYTES {\n                    return request_error(\n                        StatusCode::PAYLOAD_TOO_LARGE,\n                        \"native route body exceeds the Container execution input limit\",\n                    );\n                }\n                input\n            }\n""",
    """            RouteWasmInput::JsonBody => {\n                let body = args\n                    .first()\n                    .and_then(|request| match request {\n                        Value::Object(fields) => fields.get(\"body\"),\n                        _ => None,\n                    })\n                    .unwrap_or(&Value::Null);\n                let input = match serde_json::to_vec(&value_to_json(body)) {\n                    Ok(input) => input,\n                    Err(error) => {\n                        tracing::error!(error = %error, path = %path, \"encode native req.body input\");\n                        return request_error(\n                            StatusCode::INTERNAL_SERVER_ERROR,\n                            \"native route input could not be encoded\",\n                        );\n                    }\n                };\n                if input.len() > CONTAINER_MAX_EXECUTION_INPUT_BYTES {\n                    return request_error(\n                        StatusCode::PAYLOAD_TOO_LARGE,\n                        \"native route body exceeds the Container execution input limit\",\n                    );\n                }\n                input\n            }\n            RouteWasmInput::JsonBodyCapability { max_bytes } => {\n                let body = args\n                    .first()\n                    .and_then(|request| match request {\n                        Value::Object(fields) => fields.get(\"body\"),\n                        _ => None,\n                    })\n                    .unwrap_or(&Value::Null);\n                let input = match serde_json::to_vec(&value_to_json(body)) {\n                    Ok(input) => input,\n                    Err(error) => {\n                        tracing::error!(\n                            error = %error,\n                            path = %path,\n                            \"encode native capability req.body input\"\n                        );\n                        return request_error(\n                            StatusCode::INTERNAL_SERVER_ERROR,\n                            \"native route capability input could not be encoded\",\n                        );\n                    }\n                };\n                let limit = max_bytes.min(CONTAINER_MAX_EXECUTION_INPUT_BYTES);\n                if input.len() > limit {\n                    return request_error(\n                        StatusCode::PAYLOAD_TOO_LARGE,\n                        \"native route body exceeds the capability payload limit\",\n                    );\n                }\n                input\n            }\n""",
    "dynamic capability HTTP input",
)

# End-to-end WASM regression: req.body is not interpreted by the compiler. Raw
# JSON enters the guest and is inserted as the descriptor's data value.
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """    #[test]\n    fn linked_module_video_call_uses_canonical_module_owner() {""",
    r'''    #[test]
    fn linked_module_storage_write_accepts_dynamic_request_body() {
        let module = parse_module(
            r#":import[storage.write as writeFile]
               export function save(value) {
                   return writeFile(
                       encode["UTF8"],
                       data[value],
                       write[$$/generated/body.json],
                       level[1]
                   );
               }"#,
        );
        let links = link_module_function("save", "accounts.cache", &module, "save");
        let route = parse(
            r#":import["./module/accounts/cache".save]
               class Route { post(req) { return save(req.body); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("dynamic linked Storage write wrapper should compile natively");
        };
        let RouteWasmInput::JsonBodyCapability { max_bytes } = artifact.input else {
            panic!("dynamic Storage write must request raw JSON body input");
        };
        assert!(max_bytes > 0);
        wasmparser::validate(&artifact.bytes).unwrap();
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "write");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!([{
                    "path": "$$/generated/body.json",
                    "data": {"name": "Kate", "count": 2},
                    "encoding": "UTF8",
                    "level": 1
                }])
            );
            Ok(br#"{\"path\":\"$$/generated/body.json\",\"bytes\":25}"#.to_vec())
        });
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                br#"{"name":"Kate","count":2}"#,
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert!(!result.output.is_empty());
    }

    #[test]
    fn linked_module_video_call_uses_canonical_module_owner() {''',
    "dynamic Storage write WASM regression",
)
