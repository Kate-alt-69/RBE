from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if old not in text:
        raise SystemExit(f"missing patch anchor in {path}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


WASM = "engine/crates/route-engine/src/wasm_compiler.rs"
DISCOVERY = "engine/crates/route-engine/src/discovery.rs"
DOC = "doc/field-manager.md"

replace_once(
    WASM,
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
/// Generation 8 lets one linked Module storage.write descriptor receive the
/// evaluator-visible req.body unchanged. The WASM guest builds the capability
/// JSON envelope; direct Route-to-Storage remains unreachable and ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 8;
''',
    '''pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
/// Generation 9 keeps FieldManager resolution host-side and lets native Routes
/// consume the already-resolved `req.fields` object or one `field.<name>()`
/// value through the existing bounded input ABI. No second field resolver or
/// guest-side JSON parser is introduced; ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 9;
''',
)

replace_once(
    WASM,
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteWasmInput {
    None,
    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`
    /// value. This keeps strings/null/objects/arrays semantically identical.
    JsonBody,
    /// Raw JSON req.body used by a guest-built capability payload. The limit is
    /// compiler-derived after accounting for the static capability envelope.
    JsonBodyCapability {
        max_bytes: usize,
    },
}
''',
    '''#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteWasmInput {
    None,
    /// Invocation input is the JSON encoding of the evaluator-visible `req.body`
    /// value. This keeps strings/null/objects/arrays semantically identical.
    JsonBody,
    /// Invocation input is the JSON encoding of the pre-resolved `req.fields`
    /// object produced by the one host-side FieldManager pipeline.
    JsonFields,
    /// Invocation input is one pre-resolved FieldManager value. The field name
    /// is compiler-owned metadata and is never selected by request data.
    JsonField { name: String },
    /// Raw JSON req.body used by a guest-built capability payload. The limit is
    /// compiler-derived after accounting for the static capability envelope.
    JsonBodyCapability {
        max_bytes: usize,
    },
}
''',
)

replace_once(
    WASM,
    '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler generation 8 keeps ABI v3 and permits
/// one immutable linked Module function supplied by RELC. Linked host calls are
/// normally static JSON; generation 8 additionally permits one Module-owned
/// `storage.write` call whose `data[]` value receives `req.body` unchanged.
/// Namespace imports, wider Module bodies, and other dynamic host arguments
/// remain interpreter-only.
''',
    '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler generation 9 keeps ABI v3 and permits
/// one immutable linked Module function supplied by RELC. Linked host calls are
/// normally static JSON; dynamic native inputs include unchanged `req.body`,
/// pre-resolved `req.fields`, and one pre-resolved `field.<name>()` value.
/// Wider Module bodies and other dynamic host arguments remain interpreter-only.
''',
)

replace_once(
    WASM,
    '''    let mut host_import = None;
    let mut linked_import = None;
    match file.imports.as_slice() {
        [] => {}
        [import] => {
            if let Some(found) = direct_capability_import(import) {
                host_import = Some(found);
            } else if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
                let binding = binding_name(import);
                let Some(linked) = links.module_functions.get(&binding) else {
                    return fallback(
                        "native linked Module function has no immutable RELC link context",
                    );
                };
                linked_import = Some((binding, linked));
            } else {
                return fallback(
                    format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} only supports one direct http.get/post/request import or one linked Module function import"),
                );
            }
        }
        _ => {
            return fallback(
                format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} supports at most one direct or linked host-call import"),
            )
        }
    }
''',
    '''    let mut host_import = None;
    let mut linked_import = None;
    let runtime_imports = file
        .imports
        .iter()
        .filter(|import| !is_field_import(import))
        .cloned()
        .collect::<Vec<_>>();
    match runtime_imports.as_slice() {
        [] => {}
        [import] => {
            if let Some(found) = direct_capability_import(import) {
                host_import = Some(found);
            } else if matches!(base_import(import), ImportTarget::CustomFunction { .. }) {
                let binding = binding_name(import);
                let Some(linked) = links.module_functions.get(&binding) else {
                    return fallback(
                        "native linked Module function has no immutable RELC link context",
                    );
                };
                linked_import = Some((binding, linked));
            } else {
                return fallback(
                    format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} only supports FieldManager namespaces plus one direct http.get/post/request import or one linked Module function import"),
                );
            }
        }
        _ => {
            return fallback(
                format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} supports FieldManager namespaces plus at most one direct or linked host-call import"),
            )
        }
    }
''',
)

replace_once(
    WASM,
    '''fn base_import(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => base_import(target),
        other => other,
    }
}
''',
    '''fn base_import(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => base_import(target),
        other => other,
    }
}

fn is_field_import(import: &ImportTarget) -> bool {
    match base_import(import) {
        ImportTarget::Builtin(module) => module == "field",
        ImportTarget::BuiltinFunction { module, .. } => module == "field",
        _ => false,
    }
}
''',
)

replace_once(
    WASM,
    '''    } else if returns_request_body(method.param_name.as_deref(), expr) {
        (encode_input_echo_module(), RouteWasmInput::JsonBody)
    } else {
        return fallback("route return value is outside the native Route-WASM v3 subset");
    };
''',
    '''    } else if returns_request_body(method.param_name.as_deref(), expr) {
        (encode_input_echo_module(), RouteWasmInput::JsonBody)
    } else if returns_request_fields(method.param_name.as_deref(), expr) {
        (encode_input_echo_module(), RouteWasmInput::JsonFields)
    } else if let Some(name) = returns_field_resolver(expr) {
        (
            encode_input_echo_module(),
            RouteWasmInput::JsonField { name },
        )
    } else {
        return fallback("route return value is outside the native Route-WASM v3 subset");
    };
''',
)

replace_once(
    WASM,
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
''',
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

fn returns_request_fields(parameter: Option<&str>, expr: &Expr) -> bool {
    let Some(parameter) = parameter else {
        return false;
    };
    matches!(
        expr,
        Expr::Member(target, field)
            if field == "fields" && matches!(target.as_ref(), Expr::Ident(name) if name == parameter)
    )
}

fn returns_field_resolver(expr: &Expr) -> Option<String> {
    let Expr::Call(target, args) = expr else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let Expr::Member(namespace, name) = target.as_ref() else {
        return None;
    };
    matches!(namespace.as_ref(), Expr::Ident(namespace) if namespace == "field")
        .then(|| name.clone())
}
''',
)

# Add compiler-level native FieldManager tests before the existing direct HTTP test.
replace_once(
    WASM,
    '''    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {
''',
    '''    #[test]
    fn field_namespace_does_not_force_static_route_out_of_native_wasm() {
        let route = parse(
            r#":import[field]
               fields { page = optional("page", type = int, default = 1); }
               class Route { get(req) { return { ok: true }; } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("FieldManager namespace should not disable a static native Route");
        };
        assert_eq!(artifact.input, RouteWasmInput::None);
        wasmparser::validate(&artifact.bytes).unwrap();
    }

    #[test]
    fn native_route_can_echo_pre_resolved_req_fields() {
        let route = parse(
            r#":import[field]
               fields { page = optional("page", type = int, default = 1); }
               class Route { get(req) { return req.fields; } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("req.fields should lower to bounded native input echo");
        };
        assert_eq!(artifact.input, RouteWasmInput::JsonFields);
        wasmparser::validate(&artifact.bytes).unwrap();
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                br#"{"page":7}"#,
                ExecutionLimits::default(),
                None,
            )
            .unwrap();
        assert_eq!(result.output, br#"{"page":7}"#);
    }

    #[test]
    fn native_route_can_echo_one_pre_resolved_field_value() {
        let route = parse(
            r#":import[field]
               fields { page = optional("page", type = int, default = 1); }
               class Route { get() { return field.page(); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("field.page() should lower to one pre-resolved native input");
        };
        assert_eq!(
            artifact.input,
            RouteWasmInput::JsonField {
                name: "page".into()
            }
        );
        wasmparser::validate(&artifact.bytes).unwrap();
        let result = WasmExecutor::new()
            .unwrap()
            .execute_with_input_and_capabilities(
                &artifact.bytes,
                b"7",
                ExecutionLimits::default(),
                None,
            )
            .unwrap();
        assert_eq!(result.output, b"7");
    }

    #[test]
    fn reusable_field_import_is_native_input_metadata_not_host_capability() {
        let route = parse(
            r#":import[field:awesomeness]
               class Route { get() { return field.awesomeness(); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("reusable FieldManager resolver should compile as pre-resolved input");
        };
        assert_eq!(
            artifact.input,
            RouteWasmInput::JsonField {
                name: "awesomeness".into()
            }
        );
    }

    #[test]
    fn direct_static_http_get_is_native_v3_capability_call() {
''',
)

# Discovery: encode one host-selected Value into bounded native input.
replace_once(
    DISCOVERY,
    '''fn route_value_response(path: &str, value: Value) -> Response {
    match rel_http_response(&value) {
        Ok(Some(response)) => response,
        Ok(None) => Json(value_to_json(&value)).into_response(),
        Err(error) => {
            tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
            append_runtime_error(path, &error);
            request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}
''',
    '''fn route_value_response(path: &str, value: Value) -> Response {
    match rel_http_response(&value) {
        Ok(Some(response)) => response,
        Ok(None) => Json(value_to_json(&value)).into_response(),
        Err(error) => {
            tracing::error!(error = %error, path = %path, "REL response descriptor rejected");
            append_runtime_error(path, &error);
            request_error(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    }
}

fn request_snapshot_member<'a>(request: Option<&'a Value>, member: &str) -> Option<&'a Value> {
    request.and_then(|request| match request {
        Value::Object(fields) => fields.get(member),
        _ => None,
    })
}

fn request_snapshot_field<'a>(request: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    request_snapshot_member(request, "fields").and_then(|fields| match fields {
        Value::Object(fields) => fields.get(name),
        _ => None,
    })
}

fn encode_native_route_input(
    path: &str,
    value: &Value,
    label: &str,
    limit: usize,
    too_large_message: &'static str,
) -> Result<Vec<u8>, Response> {
    let input = serde_json::to_vec(&value_to_json(value)).map_err(|error| {
        tracing::error!(error = %error, path = %path, input = label, "encode native Route-WASM input");
        request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "native route input could not be encoded",
        )
    })?;
    if input.len() > limit {
        return Err(request_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            too_large_message,
        ));
    }
    Ok(input)
}
''',
)

old_native = '''    let args = if takes_request {
        vec![request_snapshot.take().unwrap_or(Value::Null)]
    } else {
        Vec::new()
    };
    if let Some(plan) = native_plan.as_deref() {
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
            RouteWasmInput::JsonBodyCapability { max_bytes } => {
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
                            "encode native capability req.body input"
                        );
                        return request_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "native route capability input could not be encoded",
                        );
                    }
                };
                let limit = max_bytes.min(CONTAINER_MAX_EXECUTION_INPUT_BYTES);
                if input.len() > limit {
                    return request_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "native route body exceeds the capability payload limit",
                    );
                }
                input
            }
        };
        return execute_native_route(plan, image.as_ref(), &state, &path, input).await;
    }
'''

new_native = '''    if let Some(plan) = native_plan.as_deref() {
        let snapshot = request_snapshot.as_ref();
        let input = match &plan.artifact.input {
            RouteWasmInput::None => Vec::new(),
            RouteWasmInput::JsonBody => {
                let body = request_snapshot_member(snapshot, "body").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    body,
                    "req.body",
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route body exceeds the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return response,
                }
            }
            RouteWasmInput::JsonFields => {
                let fields = request_snapshot_member(snapshot, "fields").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    fields,
                    "req.fields",
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route fields exceed the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return response,
                }
            }
            RouteWasmInput::JsonField { name } => {
                let value = request_snapshot_field(snapshot, name).unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    value,
                    name,
                    CONTAINER_MAX_EXECUTION_INPUT_BYTES,
                    "native route field exceeds the Container execution input limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return response,
                }
            }
            RouteWasmInput::JsonBodyCapability { max_bytes } => {
                let body = request_snapshot_member(snapshot, "body").unwrap_or(&Value::Null);
                match encode_native_route_input(
                    &path,
                    body,
                    "capability req.body",
                    (*max_bytes).min(CONTAINER_MAX_EXECUTION_INPUT_BYTES),
                    "native route body exceeds the capability payload limit",
                ) {
                    Ok(input) => input,
                    Err(response) => return response,
                }
            }
        };
        return execute_native_route(plan, image.as_ref(), &state, &path, input).await;
    }
    let args = if takes_request {
        vec![request_snapshot.take().unwrap_or(Value::Null)]
    } else {
        Vec::new()
    };
'''
replace_once(DISCOVERY, old_native, new_native)

replace_once(
    DISCOVERY,
    '''        // FLD-002 keeps Field-backed Routes on the linked evaluator path. The
        // Field context is resolved before dispatch; native lowering can adopt
        // the same pre-resolved input contract in a later compiler generation.
        let native_plan = if field_plan.is_active() {
            None
        } else {
            image.route_wasm_artifact(id).map(|artifact| {
                Arc::new(NativeRoutePlan {
                    runtime_image: image.image_id.clone(),
                    source_id: id.clone(),
                    artifact: artifact.clone(),
                })
            })
        };
''',
    '''        // FLD-005 lets native Route-WASM consume the exact host-resolved
        // FieldManager context. Unsupported route shapes still have no artifact
        // and therefore fall back to the linked evaluator as before.
        let native_plan = image.route_wasm_artifact(id).map(|artifact| {
            Arc::new(NativeRoutePlan {
                runtime_image: image.image_id.clone(),
                source_id: id.clone(),
                artifact: artifact.clone(),
            })
        });
''',
)

# Update stale docs and document the new native subset.
replace_once(
    DOC,
    '''Field-backed Routes deliberately remain on the linked evaluator path in FLD-002. Native Route-WASM adoption must consume the same pre-resolved Field context; it must not invent a second resolution model.
''',
    '''Field resolution is always host-owned and runs before Route dispatch. FLD-005 allows the native Route-WASM subset to consume that same pre-resolved context; native execution never creates a second FieldManager resolver.
''',
)
replace_once(
    DOC,
    '''Inline and reusable FieldManager names share one per-Route namespace. A collision fails closed instead of silently shadowing one resolver. Field-backed Routes continue to use the linked evaluator path until native Route-WASM can consume the same pre-resolved Field context without creating a second resolution model.
''',
    '''Inline and reusable FieldManager names share one per-Route namespace. A collision fails closed instead of silently shadowing one resolver. Native-capable field-backed Routes consume the same pre-resolved context; unsupported REL shapes continue to fall back to the linked evaluator.
''',
)

Path(DOC).write_text(
    Path(DOC).read_text()
    + '''\n\n## Native Route-WASM field inputs (FLD-005)\n\nFieldManager resolution still happens exactly once on the host before dispatch. Route-WASM compiler generation 9 can now keep a field-backed Route native when the Route body is already inside the native subset and returns one of these shapes:\n\n```text\nreturn { ok: true };       // static output, even with fields declared\nreturn req.fields;         // entire pre-resolved field object\nreturn field.page();       // one pre-resolved inline/reusable field value\n```\n\nThe guest receives only bounded JSON bytes selected by compiler-owned metadata and echoes them through the existing ABI. It does not parse the request again, execute `.field` code, or decide which field to read. `field.required(...)`, transforms, multi-expression Route bodies, and other unsupported dynamic shapes still use the linked evaluator.\n'''
)
