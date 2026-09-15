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
    """/// Generation 10 folds request-independent local Route helpers in addition to
/// generation 9's static statements and expressions. Dynamic request data and
/// host-dependent helper calls remain outside the native subset. Capability ABI
/// v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 10;""",
    """/// Generation 11 lets request-independent REL expressions and local helpers
/// feed exact host-capability arguments while preserving the same Controller
/// authority boundary. Dynamic request-derived transformations remain outside
/// the native subset. Capability ABI v3 remains stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 11;""",
    "compiler generation",
)

text = replace_once(
    text,
    """/// generation v10 keeps ABI v3, adds bounded deterministic folding of pure
/// request-independent local Route helpers, and retains generation 9's static
/// statements plus generation 8's immutable linked Module capability path.
/// Dynamic transformations, host-dependent helpers, wider Module bodies,
/// namespace imports, and nested host-call chains remain interpreter-only.""",
    """/// generation v11 keeps ABI v3, allows request-independent expressions and
/// local helpers to construct exact capability arguments, and retains generation
/// 10's bounded helper folding plus generation 8's immutable linked Module
/// authority path. Dynamic transformations, host-dependent helpers, wider
/// Module bodies, namespace imports, and nested host-call chains remain
/// interpreter-only.""",
    "compiler contract",
)

text = replace_once(
    text,
    """        let Some(args) = static_direct_call(&import.binding, expr) else {""",
    """        let Some(args) = static_direct_call(&import.binding, expr, &file.functions) else {""",
    "direct capability static arguments",
)

text = replace_once(
    text,
    """            method.param_name.as_deref(),
        ) {""",
    """            method.param_name.as_deref(),
            &file.functions,
        ) {""",
    "linked capability route functions",
)

text = replace_once(
    text,
    """    route_expr: &Expr,
    request_parameter: Option<&str>,
) -> Result<LoweredCapabilityCall, String> {""",
    """    route_expr: &Expr,
    request_parameter: Option<&str>,
    route_functions: &[FunctionDef],
) -> Result<LoweredCapabilityCall, String> {""",
    "linked capability signature",
)

old_static_link = r'''    if let Some(route_args) = route_args
        .iter()
        .map(static_json)
        .collect::<Option<Vec<_>>>()
    {
        let bindings = linked
            .function
            .params
            .iter()
            .cloned()
            .zip(route_args)
            .collect::<BTreeMap<_, _>>();
        let args = host_args
            .iter()
            .map(|argument| static_json_with_bindings(argument, &bindings))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                "native linked Module host arguments must resolve to static JSON values".to_string()
            })?;
        let payload = serde_json::to_vec(&args)
            .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
'''
new_static_link = r'''    let route_function_map = static_function_map(route_functions).ok_or_else(|| {
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
        let no_module_helpers = BTreeMap::<String, FunctionDef>::new();
        let args = host_args
            .iter()
            .map(|argument| {
                static_eval_expr(
                    argument,
                    &bindings,
                    &no_module_helpers,
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
'''
text = replace_once(text, old_static_link, new_static_link, "linked static expression lowering")

start = text.index("fn static_direct_call(")
end = text.index("\n#[derive(Debug, Clone)]\nenum StaticFlow", start)
new_helpers = r'''fn static_direct_call(
    binding: &str,
    expr: &Expr,
    functions: &[FunctionDef],
) -> Option<Vec<serde_json::Value>> {
    let Expr::Call(target, args) = expr else {
        return None;
    };
    if !matches!(target.as_ref(), Expr::Ident(name) if name == binding) {
        return None;
    }

    let functions = static_function_map(functions)?;
    let scope = BTreeMap::<String, Value>::new();
    args.iter()
        .map(|argument| {
            static_eval_expr(
                argument,
                &scope,
                &functions,
                MAX_STATIC_HELPER_CALL_DEPTH,
            )
            .map(|value| static_value_to_json(&value))
        })
        .collect()
}

fn static_function_map(functions: &[FunctionDef]) -> Option<BTreeMap<String, FunctionDef>> {
    let mut function_map = BTreeMap::new();
    for function in functions {
        if function_map
            .insert(function.name.clone(), function.clone())
            .is_some()
        {
            return None;
        }
    }
    Some(function_map)
}
'''
text = text[:start] + new_helpers + text[end:]

old_map = r'''    let mut function_map = BTreeMap::<String, FunctionDef>::new();
    for function in functions {
        if function_map
            .insert(function.name.clone(), function.clone())
            .is_some()
        {
            return None;
        }
    }

'''
text = replace_once(
    text,
    old_map,
    "    let function_map = static_function_map(functions)?;\n\n",
    "static route function map",
)

static_json_start = text.index("\nfn static_json(expr: &Expr) -> Option<serde_json::Value> {")
encode_start = text.index("\nfn encode_capability_call_module(", static_json_start)
text = text[:static_json_start] + "\n" + text[encode_start:]

anchor = """    #[test]
    fn generated_http_wasm_round_trips_through_real_capability_host_abi() {"""
if anchor not in text:
    raise SystemExit("capability expression test anchor missing")
new_tests = r'''    #[test]
    fn direct_capability_arguments_can_use_pure_route_helpers() {
        let route = parse(
            r#":import[http.get]
               function origin() { return "https://example.com"; }
               function url(path) { return origin() + path; }
               class Route { get(req) { return get(url("/data")); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route(&route) else {
            panic!("pure helper-built HTTP argument should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Network);
            assert_eq!(request.operation, "get");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["https://example.com/data"])
            );
            Ok(br#"{"ok":true}"#.to_vec())
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
        assert_eq!(result.output, br#"{"ok":true}"#);
    }

    #[test]
    fn linked_module_capability_arguments_fold_pure_rel_expressions() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               export function load(prefix, id) {
                   return readEntry(prefix + "/" + id + ".json");
               }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               function user() { return "kate"; }
               class Route { get(req) { return load("users", user()); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("pure linked Module capability expressions should compile natively");
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

'''
text = text.replace(anchor, new_tests + anchor, 1)

path.write_text(text)
