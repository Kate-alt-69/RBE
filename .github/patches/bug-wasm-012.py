from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    """/// Generation 14 lowers linked Module wrappers with multiple exact host
/// imports and pure local Module helpers. The returned host binding is selected
/// exactly; helper evaluation remains bounded and host-call free. ABI v3 stays
/// unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 14;""",
    """/// Generation 15 lowers request-independent `const`/`if` control flow in
/// linked Module wrappers before one exact terminal host call. Static execution
/// stays host-call free and bounded; the selected capability still executes in
/// the generated WASM guest. ABI v3 stays unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 15;""",
    "compiler generation 15",
)

text = replace_once(
    text,
    """/// generation v14 keeps ABI v3 and extends immutable linked Module lowering
/// to multiple exact host imports plus pure local Module helpers. It retains
/// generation 13's per-method artifacts, generation 12's exact Route import
/// binding, and bounded helper folding. Namespace host calls, ambiguous bindings,
/// dynamic transforms, wider host-call control flow, and nested host-call chains
/// remain interpreter-only.""",
    """/// generation v15 keeps ABI v3 and extends immutable linked Module lowering
/// through request-independent `const`, pure expression, and `if` prefixes that
/// resolve to one exact terminal host call. It retains generation 14's multiple
/// host imports/helpers and generation 13's per-method artifacts. Dynamic control
/// flow, namespace host calls, ambiguous bindings, and nested host-call chains
/// remain interpreter-only.""",
    "compiler generation 15 contract",
)

old = r'''    let [Statement::Return(module_expr)] = linked.function.body.as_slice() else {
        return Err(
            "native linked Module function must contain exactly one return statement".into(),
        );
    };
    let Expr::Call(target, host_args) = module_expr else {
        return Err("native linked Module return must directly call one exact host import".into());
    };
    let Expr::Ident(host_binding) = target.as_ref() else {
        return Err(
            "native linked Module host call must use an exact imported function binding; namespace host calls remain interpreter-only"
                .into(),
        );
    };

    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();
    for import in &linked.imports {
        let Some(host) = direct_linked_capability_import(import, &linked.owner) else {
            continue;
        };
        let binding = host.binding.clone();
        if host_imports.insert(binding.clone(), host).is_some() {
            return Err(format!(
                "native linked Module has ambiguous host import binding {binding:?}"
            ));
        }
    }
    let host = host_imports.get(host_binding).cloned().ok_or_else(|| {
        format!(
            "native linked Module return target {host_binding:?} is not one exact HTTP, Video, Service, or Storage import"
        )
    })?;

    let route_function_map = static_function_map(route_functions).ok_or_else(|| {
        "native capability lowering requires unique local Route helper names".to_string()
    })?;
    let route_scope = BTreeMap::<String, Value>::new();
    if let Some(route_args) = route_args
        .iter()
        .map(|argument| {
            static_eval_expr(
                argument,
                &route_scope,
                &route_function_map,
                MAX_STATIC_HELPER_CALL_DEPTH,
            )
        })
        .collect::<Option<Vec<_>>>()
    {
        let bindings = linked
            .function
            .params
            .iter()
            .cloned()
            .zip(route_args)
            .collect::<BTreeMap<_, _>>();
        let module_function_map = static_function_map(&linked.functions).ok_or_else(|| {
            "native linked Module helper lowering requires unique Module function names".to_string()
        })?;
        let args = host_args
            .iter()
            .map(|argument| {
                static_eval_expr(
                    argument,
                    &bindings,
                    &module_function_map,
                    MAX_STATIC_HELPER_CALL_DEPTH,
                )
                .map(|value| static_value_to_json(&value))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "native linked Module host arguments must resolve to request-independent REL values"
                    .to_string()
            })?;
        let payload = serde_json::to_vec(&args)
            .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err(
                "native linked Module argument payload exceeds the capability envelope".into(),
            );
        }
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::Static(payload),
        });
    }

    let dynamic_body_passthrough = linked.function.params.len() == 1
        && route_args.len() == 1
        && host_args.len() == 1
        && returns_request_body(request_parameter, &route_args[0])
        && matches!(
            &host_args[0],
            Expr::Ident(name) if name == &linked.function.params[0]
        );
    if dynamic_body_passthrough {
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::JsonBodySingleArgument,
        });
    }
'''
new = r'''    let mut host_imports = BTreeMap::<String, DirectCapabilityImport>::new();
    for import in &linked.imports {
        let Some(host) = direct_linked_capability_import(import, &linked.owner) else {
            continue;
        };
        let binding = host.binding.clone();
        if host_imports.insert(binding.clone(), host).is_some() {
            return Err(format!(
                "native linked Module has ambiguous host import binding {binding:?}"
            ));
        }
    }

    let route_function_map = static_function_map(route_functions).ok_or_else(|| {
        "native capability lowering requires unique local Route helper names".to_string()
    })?;
    let route_scope = BTreeMap::<String, Value>::new();
    if let Some(route_args) = route_args
        .iter()
        .map(|argument| {
            static_eval_expr(
                argument,
                &route_scope,
                &route_function_map,
                MAX_STATIC_HELPER_CALL_DEPTH,
            )
        })
        .collect::<Option<Vec<_>>>()
    {
        let mut bindings = linked
            .function
            .params
            .iter()
            .cloned()
            .zip(route_args)
            .collect::<BTreeMap<_, _>>();
        let module_function_map = static_function_map(&linked.functions).ok_or_else(|| {
            "native linked Module helper lowering requires unique Module function names".to_string()
        })?;
        let (host, args) = lower_static_module_host_call(
            &linked.function.body,
            &mut bindings,
            &module_function_map,
            &host_imports,
            MAX_STATIC_HELPER_CALL_DEPTH,
        )?
        .ok_or_else(|| {
            "native linked Module static control flow completed without one exact host return"
                .to_string()
        })?;
        let payload = serde_json::to_vec(&args)
            .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err(
                "native linked Module argument payload exceeds the capability envelope".into(),
            );
        }
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::Static(payload),
        });
    }

    let Some((host, host_args)) = direct_module_host_return(&linked.function.body, &host_imports)
    else {
        return Err(
            "dynamic linked Module lowering requires exactly one direct exact-host return; static const/if prefixes require request-independent Route arguments"
                .into(),
        );
    };
    let dynamic_body_passthrough = linked.function.params.len() == 1
        && route_args.len() == 1
        && host_args.len() == 1
        && returns_request_body(request_parameter, &route_args[0])
        && matches!(
            &host_args[0],
            Expr::Ident(name) if name == &linked.function.params[0]
        );
    if dynamic_body_passthrough {
        return Ok(LoweredCapabilityCall {
            kind: host.kind,
            target: host.target,
            operation: host.operation,
            payload: LoweredCapabilityPayload::JsonBodySingleArgument,
        });
    }
'''
text = replace_once(text, old, new, "generation 15 linked Module lowering")

anchor = """fn static_direct_call(
    binding: &str,"""
if anchor not in text:
    raise SystemExit("generation 15 helper insertion anchor missing")
helpers = r'''fn direct_module_host_return<'a>(
    body: &'a [Statement],
    host_imports: &BTreeMap<String, DirectCapabilityImport>,
) -> Option<(DirectCapabilityImport, &'a [Expr])> {
    let [Statement::Return(Expr::Call(target, args))] = body else {
        return None;
    };
    let Expr::Ident(binding) = target.as_ref() else {
        return None;
    };
    let host = host_imports.get(binding)?.clone();
    Some((host, args.as_slice()))
}

fn lower_static_module_host_call(
    body: &[Statement],
    scope: &mut BTreeMap<String, Value>,
    functions: &BTreeMap<String, FunctionDef>,
    host_imports: &BTreeMap<String, DirectCapabilityImport>,
    remaining_helper_depth: usize,
) -> Result<Option<(DirectCapabilityImport, Vec<serde_json::Value>)>, String> {
    for statement in body {
        match statement {
            Statement::Const { name, value } => {
                let value = static_eval_expr(value, scope, functions, remaining_helper_depth)
                    .ok_or_else(|| {
                        format!(
                            "native linked Module const {name:?} must be request-independent and host-call free"
                        )
                    })?;
                scope.insert(name.clone(), value);
            }
            Statement::Expr(expr) => {
                static_eval_expr(expr, scope, functions, remaining_helper_depth).ok_or_else(|| {
                    "native linked Module prefix expressions must be request-independent and host-call free"
                        .to_string()
                })?;
            }
            Statement::Return(expr) => {
                let Expr::Call(target, host_args) = expr else {
                    return Err(
                        "native linked Module terminal return must call one exact host import"
                            .into(),
                    );
                };
                let Expr::Ident(binding) = target.as_ref() else {
                    return Err(
                        "native linked Module host call must use an exact imported function binding; namespace host calls remain interpreter-only"
                            .into(),
                    );
                };
                let host = host_imports.get(binding).cloned().ok_or_else(|| {
                    format!(
                        "native linked Module return target {binding:?} is not one exact HTTP, Video, Service, or Storage import"
                    )
                })?;
                let args = host_args
                    .iter()
                    .map(|argument| {
                        static_eval_expr(argument, scope, functions, remaining_helper_depth)
                            .map(|value| static_value_to_json(&value))
                    })
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        "native linked Module host arguments must resolve to request-independent REL values"
                            .to_string()
                    })?;
                return Ok(Some((host, args)));
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let condition = static_eval_expr(
                    condition,
                    scope,
                    functions,
                    remaining_helper_depth,
                )
                .ok_or_else(|| {
                    "native linked Module control flow must be request-independent and host-call free"
                        .to_string()
                })?;
                let branch = if condition.truthy() {
                    then_body
                } else {
                    else_body
                };
                if let Some(call) = lower_static_module_host_call(
                    branch,
                    scope,
                    functions,
                    host_imports,
                    remaining_helper_depth,
                )? {
                    return Ok(Some(call));
                }
            }
        }
    }
    Ok(None)
}

'''
text = text.replace(anchor, helpers + anchor, 1)

anchor = """    #[test]
    fn linked_module_multiple_exact_host_imports_select_returned_binding() {"""
if anchor not in text:
    raise SystemExit("generation 15 tests anchor missing")
new_tests = r'''    #[test]
    fn linked_module_static_const_prefix_lowers_before_host_call() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               function suffix() { return ".json"; }
               export function load(name) {
                   const path = "users/" + name + suffix();
                   return readEntry(path);
               }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { get(req) { return load("kate"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("static Module const prefix should lower natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "read");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users/kate.json"])
            );
            Ok(br#"{"found":true}"#.to_vec())
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
        assert_eq!(result.output, br#"{"found":true}"#);
    }

    #[test]
    fn linked_module_static_if_selects_exact_host_binding() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               :import[storage.list as listEntries]
               export function query(mode) {
                   if (mode === "list") {
                       return listEntries("users");
                   }
                   return readEntry("users/kate.json");
               }"#,
        );
        let links = link_module_function("query", "accounts.cache", &module, "query");
        let route = parse(
            r#":import["./module/accounts/cache".query]
               class Route { get(req) { return query("list"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("static Module if should resolve one exact host call");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Storage);
            assert_eq!(request.target, "storage:accounts.cache");
            assert_eq!(request.operation, "list");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["users"])
            );
            Ok(br#"{"entries":[]}"#.to_vec())
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
        assert_eq!(result.output, br#"{"entries":[]}"#);
    }

    #[test]
    fn linked_module_dynamic_control_flow_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               :import[storage.list as listEntries]
               export function query(mode) {
                   if (mode === "list") { return listEntries("users"); }
                   return readEntry("users/kate.json");
               }"#,
        );
        let links = link_module_function("query", "accounts.cache", &module, "query");
        let route = parse(
            r#":import["./module/accounts/cache".query]
               class Route { post(req) { return query(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("request-dependent Module control flow must remain interpreter-only");
        };
        assert!(reason.contains("static const/if prefixes require request-independent Route arguments"));
    }

'''
text = text.replace(anchor, new_tests + anchor, 1)
path.write_text(text)


path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()
anchor = """    #[test]
    fn linked_storage_module_helpers_compile_native_with_exact_storage_grant() {"""
if anchor not in text:
    raise SystemExit("generation 15 RELC integration test anchor missing")
new_test = r'''    #[test]
    fn linked_storage_module_static_prefix_compiles_into_native_image() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "accounts/prefix-cache",
                "module/accounts/prefix-cache.module",
                r#":import[storage.read as readEntry]
                   function suffix() { return ".json"; }
                   export function load(name) {
                       const path = "users/" + name + suffix();
                       return readEntry(path);
                   }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "account-prefix-cache",
                "api/account-prefix-cache.route",
                r#":import["./module/accounts/prefix-cache".load]
                   class Route { get(req) { return load("kate"); } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();
        let artifact = image
            .route_wasm_artifact(route, "get")
            .expect("static Module prefix should produce Route-WASM");
        assert_eq!(artifact.verb, "get");
        assert!(image.route_wasm_fallback(route, "get").is_none());
        let grants = image.container_capability_grants(route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, core_lib::ContainerCapabilityKind::Storage);
        assert_eq!(grants[0].target, "storage:accounts.prefix-cache");
        assert_eq!(grants[0].operations, vec!["read"]);
    }

'''
text = text.replace(anchor, new_test + anchor, 1)
path.write_text(text)
