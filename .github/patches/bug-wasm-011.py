from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# wasm_compiler.rs: linked exported Module wrappers may carry multiple exact
# host imports and use pure local Module helpers to construct static arguments.
# Host calls themselves are never executed during static evaluation.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text()

text = replace_once(
    text,
    """/// Generation 13 compiles each HTTP method independently so one `.route`
/// can pin distinct native artifacts and explicit fallbacks per verb. Exact
/// import binding, capability authority, and ABI v3 remain unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 13;""",
    """/// Generation 14 lowers linked Module wrappers with multiple exact host
/// imports and pure local Module helpers. The returned host binding is selected
/// exactly; helper evaluation remains bounded and host-call free. ABI v3 stays
/// unchanged.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 14;""",
    "compiler generation 14",
)

text = replace_once(
    text,
    """/// generation v13 keeps ABI v3 and lets RELC compile every method in a Route
/// independently while retaining generation 12's exact import binding,
/// generation 11's static capability arguments, and generation 10's bounded
/// helper folding. Namespace imports, ambiguous bindings, dynamic transforms,
/// wider Module bodies, and nested host-call chains remain interpreter-only.""",
    """/// generation v14 keeps ABI v3 and extends immutable linked Module lowering
/// to multiple exact host imports plus pure local Module helpers. It retains
/// generation 13's per-method artifacts, generation 12's exact Route import
/// binding, and bounded helper folding. Namespace host calls, ambiguous bindings,
/// dynamic transforms, wider host-call control flow, and nested host-call chains
/// remain interpreter-only.""",
    "compiler generation 14 contract",
)

text = replace_once(
    text,
    """struct LinkedModuleFunction {
    owner: String,
    function: FunctionDef,
    imports: Vec<ImportTarget>,
}""",
    """struct LinkedModuleFunction {
    owner: String,
    function: FunctionDef,
    imports: Vec<ImportTarget>,
    functions: Vec<FunctionDef>,
}""",
    "linked Module function helper context",
)

text = replace_once(
    text,
    """    pub(crate) fn insert_module_function(
        &mut self,
        route_binding: String,
        owner: String,
        function: FunctionDef,
        imports: Vec<ImportTarget>,
    ) {
        self.module_functions.insert(
            route_binding,
            LinkedModuleFunction {
                owner,
                function,
                imports,
            },
        );
    }""",
    """    pub(crate) fn insert_module_function(
        &mut self,
        route_binding: String,
        owner: String,
        function: FunctionDef,
        imports: Vec<ImportTarget>,
        functions: Vec<FunctionDef>,
    ) {
        self.module_functions.insert(
            route_binding,
            LinkedModuleFunction {
                owner,
                function,
                imports,
                functions,
            },
        );
    }""",
    "linked Module context insertion",
)

old_host_selection = r'''    let [module_import] = linked.imports.as_slice() else {
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
'''
new_host_selection = r'''    let Expr::Call(target, host_args) = module_expr else {
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
    let host = host_imports.get(host_binding).ok_or_else(|| {
        format!(
            "native linked Module return target {host_binding:?} is not one exact HTTP, Video, Service, or Storage import"
        )
    })?;
'''
text = replace_once(text, old_host_selection, new_host_selection, "linked Module exact host selection")

text = replace_once(
    text,
    """        let no_module_helpers = BTreeMap::<String, FunctionDef>::new();
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
            })""",
    """        let module_function_map = static_function_map(&linked.functions).ok_or_else(|| {
            "native linked Module helper lowering requires unique Module function names"
                .to_string()
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
            })""",
    "linked Module helper static evaluation",
)

text = replace_once(
    text,
    """        links.insert_module_function(
            route_binding.to_string(),
            owner.to_string(),
            function,
            module.imports.clone(),
        );""",
    """        links.insert_module_function(
            route_binding.to_string(),
            owner.to_string(),
            function,
            module.imports.clone(),
            module.functions.clone(),
        );""",
    "test linked Module context helper functions",
)

anchor = """    #[test]
    fn linked_module_service_call_substitutes_static_route_arguments() {"""
if anchor not in text:
    raise SystemExit("linked Module generation 14 tests anchor missing")
new_tests = r'''    #[test]
    fn linked_module_multiple_exact_host_imports_select_returned_binding() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               :import[storage.list as listEntries]
               export function load(path) { return readEntry(path); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { get(req) { return load("users/kate.json"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("linked Module should select the exact returned host import");
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
    fn linked_module_pure_helpers_build_static_host_arguments() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               function prefix() { return "users/"; }
               function buildPath(name) { return prefix() + name + ".json"; }
               export function load(name) { return readEntry(buildPath(name)); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { get(req) { return load("kate"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("pure Module helpers should fold into the exact host payload");
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
    fn linked_module_dynamic_helper_transform_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[storage.read as readEntry]
               function buildPath(name) { return "users/" + name + ".json"; }
               export function load(name) { return readEntry(buildPath(name)); }"#,
        );
        let links = link_module_function("load", "accounts.cache", &module, "load");
        let route = parse(
            r#":import["./module/accounts/cache".load]
               class Route { post(req) { return load(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("request-derived Module helper transforms must remain dynamic");
        };
        assert!(reason.contains("exactly one req.body value passed unchanged"));
    }

'''
text = text.replace(anchor, new_tests + anchor, 1)
path.write_text(text)


# ---------------------------------------------------------------------------
# relc.rs: immutable link context now carries the Module's local function set so
# pure helper evaluation has no need to reread source at runtime.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()

text = replace_once(
    text,
    """        context.insert_module_function(
            binding_name(import),
            module_owner_from_logical_name(source.logical_name()),
            function_def.clone(),
            module.imports.clone(),
        );""",
    """        context.insert_module_function(
            binding_name(import),
            module_owner_from_logical_name(source.logical_name()),
            function_def.clone(),
            module.imports.clone(),
            module.functions.clone(),
        );""",
    "RELC immutable Module helper context",
)

anchor = """    #[test]
    fn linked_storage_module_wrapper_compiles_native_with_exact_storage_grant() {"""
if anchor not in text:
    raise SystemExit("RELC generation 14 integration test anchor missing")
new_test = r'''    #[test]
    fn linked_storage_module_helpers_compile_native_with_exact_storage_grant() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "accounts/cache",
                "module/accounts/cache.module",
                r#":import[storage.read as readEntry]
                   :import[storage.list as listEntries]
                   function prefix() { return "users/"; }
                   function buildPath(name) { return prefix() + name + ".json"; }
                   export function load(name) { return readEntry(buildPath(name)); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "account-cache-helper",
                "api/account-cache-helper.route",
                r#":import["./module/accounts/cache".load]
                   class Route { get(req) { return load("kate"); } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();
        assert!(image.route_wasm_artifact(route, "get").is_some());
        assert!(image.route_wasm_fallback(route, "get").is_none());
        let grants = image.container_capability_grants(route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, core_lib::ContainerCapabilityKind::Storage);
        assert_eq!(grants[0].target, "storage:accounts.cache");
        assert_eq!(grants[0].operations, vec!["list", "read"]);
    }

'''
text = text.replace(anchor, new_test + anchor, 1)
path.write_text(text)
