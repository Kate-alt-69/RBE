from pathlib import Path


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


def edit(path: str, transform) -> None:
    file = Path(path)
    before = file.read_text(encoding="utf-8")
    after = transform(before)
    if after == before:
        raise SystemExit(f"{path}: transform made no change")
    file.write_text(after, encoding="utf-8")


def patch_wasm(text: str) -> str:
    text = one(
        text,
        '''use core_lib::{
    ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};
use service_runtime::SERVICE_CAPABILITY_TARGET_PREFIX;''',
        '''use std::collections::BTreeMap;

use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::SERVICE_CAPABILITY_TARGET_PREFIX;''',
        "linked compiler imports",
    )
    text = one(
        text,
        '''use crate::ast::{Expr, ImportTarget, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 4;''',
        '''use crate::ast::{Expr, FunctionDef, ImportTarget, RouteFile, Statement};
use crate::modules::binding_name;

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
/// Generation history: `pub const ROUTE_WASM_COMPILER_VERSION: u32 = 4`
/// introduced direct host-capability lowering. Generation 5 adds strict
/// immutable linked-Module host-call lowering while capability ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 5;''',
        "compiler generation and AST imports",
    )
    text = one(
        text,
        '''impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

/// Compile the currently supported native route subset.''',
        '''impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteWasmLinkContext {
    module_functions: BTreeMap<String, LinkedModuleFunction>,
}

#[derive(Debug, Clone)]
struct LinkedModuleFunction {
    owner: String,
    function: FunctionDef,
    imports: Vec<ImportTarget>,
}

impl RouteWasmLinkContext {
    pub(crate) fn insert_module_function(
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
    }
}

/// Compile the currently supported native route subset.''',
        "linked compiler context",
    )
    text = one(
        text,
        '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no helper functions. In addition to literals and req.body passthrough, compiler
/// generation v4 permits one directly imported host-capability function with fully
/// static JSON arguments: `http.get/post/request` or one exact Service export.
/// Namespace imports stay interpreter-only so native grants remain operation-exact
/// rather than widening authority.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let host_import = match file.imports.as_slice() {
        [] => None,
        [import] => match direct_capability_import(import) {
            Some(import) => Some(import),
            None => return fallback(
                "native Route-WASM v4 only supports one direct http.get/post/request or service:<name>.<operation> import",
            ),
        },
        _ => {
            return fallback(
                "native Route-WASM v4 supports at most one direct host capability import",
            )
        }
    };''',
        '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler generation v5 keeps ABI v3 and adds one
/// immutable linked Module function supplied by RELC. The linked function must
/// itself be exactly one return of one direct HTTP, Video, or Service host call,
/// and every Route/host argument must resolve to static JSON. Namespace imports,
/// dynamic arguments, wider Module bodies, and nested chains remain interpreter-only.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let links = RouteWasmLinkContext::default();
    compile_route_with_links(file, &links)
}

pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    let mut host_import = None;
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
                    "native Route-WASM v5 only supports one direct http.get/post/request or service:<name>.<operation> import or one linked Module function import",
                );
            }
        }
        _ => {
            return fallback(
                "native Route-WASM v5 supports at most one direct or linked host-call import",
            )
        }
    }''',
        "compile with immutable links",
    )
    text = one(
        text,
        '''    let (bytes, input) = if let Some(import) = host_import.as_ref() {
        let Some(args) = static_direct_call(&import.binding, expr) else {
            return fallback(
                "native host capability calls require the directly imported function as the return value with static JSON arguments",
            );
        };
        let payload = match serde_json::to_vec(&args) {
            Ok(payload) => payload,
            Err(error) => return fallback(format!("encode native capability arguments: {error}")),
        };
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return fallback("native capability argument payload exceeds the capability envelope");
        }
        (
            encode_capability_call_module(import.kind, &import.target, &import.operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some(value) = static_json(expr) {''',
        '''    let (bytes, input) = if let Some(import) = host_import.as_ref() {
        let Some(args) = static_direct_call(&import.binding, expr) else {
            return fallback(
                "native host capability calls require the directly imported function as the return value with static JSON arguments",
            );
        };
        let payload = match serde_json::to_vec(&args) {
            Ok(payload) => payload,
            Err(error) => return fallback(format!("encode native capability arguments: {error}")),
        };
        if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
            return fallback("native capability argument payload exceeds the capability envelope");
        }
        (
            encode_capability_call_module(import.kind, &import.target, &import.operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some((binding, linked)) = linked_import.as_ref() {
        let (kind, target, operation, payload) =
            match lower_linked_module_capability_call(binding, linked, expr) {
                Ok(call) => call,
                Err(reason) => return fallback(reason),
            };
        (
            encode_capability_call_module(kind, &target, &operation, &payload),
            RouteWasmInput::None,
        )
    } else if let Some(value) = static_json(expr) {''',
        "linked capability lowering branch",
    )
    text = one(
        text,
        '''fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {''',
        '''fn base_import(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => base_import(target),
        other => other,
    }
}

fn direct_linked_capability_import(
    import: &ImportTarget,
    owner: &str,
) -> Option<DirectCapabilityImport> {
    if let Some(import) = direct_capability_import(import) {
        return Some(import);
    }
    let ImportTarget::BuiltinFunction { module, function } = base_import(import) else {
        return None;
    };
    if !matches!(module.as_str(), "vm" | "video-manager")
        || !video_language_operation_allowed(function)
    {
        return None;
    }
    Some(DirectCapabilityImport {
        binding: binding_name(import),
        kind: ContainerCapabilityKind::Video,
        target: format!("{VIDEO_CAPABILITY_TARGET_PREFIX}{owner}"),
        operation: function.clone(),
    })
}

fn lower_linked_module_capability_call(
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
        return Err("native linked Module function must contain exactly one return statement".into());
    };
    let [module_import] = linked.imports.as_slice() else {
        return Err(
            "native linked Module function requires exactly one direct host capability import"
                .into(),
        );
    };
    let host = direct_linked_capability_import(module_import, &linked.owner).ok_or_else(|| {
        "native linked Module import must be one exact HTTP, Video, or Service function"
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
    let args = host_args
        .iter()
        .map(|argument| static_json_with_bindings(argument, &bindings))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            "native linked Module host arguments must resolve to static JSON values".to_string()
        })?;
    let payload = serde_json::to_vec(&args)
        .map_err(|error| format!("encode native linked Module arguments: {error}"))?;
    if payload.len() > CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err("native linked Module argument payload exceeds the capability envelope".into());
    }
    Ok((host.kind, host.target, host.operation, payload))
}

fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {''',
        "linked host resolution",
    )
    text = one(
        text,
        '''fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {''',
        '''fn static_json_with_bindings(
    expr: &Expr,
    bindings: &BTreeMap<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    match expr {
        Expr::Ident(name) => bindings.get(name).cloned(),
        Expr::String(value) => Some(serde_json::Value::String(value.clone())),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(serde_json::Value::Number),
        Expr::Bool(value) => Some(serde_json::Value::Bool(*value)),
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Array(values) => values
            .iter()
            .map(|value| static_json_with_bindings(value, bindings))
            .collect::<Option<Vec<_>>>()
            .map(serde_json::Value::Array),
        Expr::Object(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                object.insert(name.clone(), static_json_with_bindings(value, bindings)?);
            }
            Some(serde_json::Value::Object(object))
        }
        Expr::Member(_, _)
        | Expr::Call(_, _)
        | Expr::UnaryNot(_)
        | Expr::Binary { .. } => None,
    }
}

fn returns_request_body(parameter: Option<&str>, expr: &Expr) -> bool {''',
        "linked static substitution",
    )
    text = one(
        text,
        '''    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }
''',
        '''    fn parse(source: &str) -> RouteFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_file().unwrap()
    }

    fn parse_module(source: &str) -> crate::ast::ModuleFile {
        let tokens = Lexer::new(source).tokenize().unwrap();
        Parser::new(tokens).parse_module_file().unwrap()
    }

    fn link_module_function(
        route_binding: &str,
        owner: &str,
        module: &crate::ast::ModuleFile,
        function: &str,
    ) -> RouteWasmLinkContext {
        let mut links = RouteWasmLinkContext::default();
        let function = module
            .functions
            .iter()
            .find(|candidate| candidate.name == function)
            .unwrap()
            .clone();
        links.insert_module_function(
            route_binding.to_string(),
            owner.to_string(),
            function,
            module.imports.clone(),
        );
        links
    }
''',
        "linked compiler test helpers",
    )
    text = one(
        text,
        '''    #[test]
    fn generic_capability_emitter_uses_versioned_kind_mapping_for_video_and_service() {''',
        '''    #[test]
    fn linked_module_service_call_substitutes_static_route_arguments() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { get(req) { return lookup("kate"); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("strict linked Service wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Service);
            assert_eq!(request.target, "service:uac");
            assert_eq!(request.operation, "get_user");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.payload).unwrap(),
                serde_json::json!(["kate"])
            );
            Ok(br#"{\"id\":\"kate\"}"#.to_vec())
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
        assert_eq!(result.output, br#"{\"id\":\"kate\"}"#);
    }

    #[test]
    fn linked_module_video_call_uses_canonical_module_owner() {
        let module = parse_module(
            r#":import[video-manager.status as vmStatus]
               export function status() { return vmStatus(); }"#,
        );
        let links = link_module_function("status", "media.status", &module, "status");
        let route = parse(
            r#":import["./module/media/status".status]
               class Route { get(req) { return status(); } }"#,
        );
        let RouteWasmCompilation::Native(artifact) = compile_route_with_links(&route, &links)
        else {
            panic!("strict linked Video wrapper should compile natively");
        };
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, ContainerCapabilityKind::Video);
            assert_eq!(request.target, "module:media.status");
            assert_eq!(request.operation, "status");
            assert_eq!(request.payload, b"[]");
            Ok(br#"{\"ok\":true}"#.to_vec())
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
        assert_eq!(result.output, br#"{\"ok\":true}"#);
    }

    #[test]
    fn linked_module_dynamic_route_argument_stays_interpreter_fallback() {
        let module = parse_module(
            r#":import[service:uac.get_user as getUser]
               export function lookup(id) { return getUser(id); }"#,
        );
        let links = link_module_function("lookup", "accounts", &module, "lookup");
        let route = parse(
            r#":import["./module/accounts".lookup]
               class Route { post(req) { return lookup(req.body); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } =
            compile_route_with_links(&route, &links)
        else {
            panic!("dynamic linked arguments must remain interpreter-only");
        };
        assert!(reason.contains("static JSON arguments"));
    }

    #[test]
    fn generic_capability_emitter_uses_versioned_kind_mapping_for_video_and_service() {''',
        "linked compiler tests",
    )
    return text


edit("engine/crates/route-engine/src/wasm_compiler.rs", patch_wasm)


def patch_relc(text: str) -> str:
    text = one(
        text,
        '''use crate::wasm_compiler::{compile_route, RouteWasmCompilation};''',
        '''use crate::wasm_compiler::{
    compile_route_with_links, RouteWasmCompilation, RouteWasmLinkContext,
};''',
        "RELC linked compiler imports",
    )
    text = one(
        text,
        '''        match compile_route(file) {
            RouteWasmCompilation::Native(artifact) => {''',
        '''        let link_context = route_wasm_link_context(&registry, &compiled, file);
        match compile_route_with_links(file, &link_context) {
            RouteWasmCompilation::Native(artifact) => {''',
        "RELC linked route compilation",
    )
    text = one(
        text,
        '''fn runtime_env_from_settings(
    settings: &JsonValue,
) -> Result<BTreeMap<String, JsonValue>, RelcError> {''',
        '''fn route_wasm_link_context(
    registry: &RelSourceRegistry,
    compiled: &BTreeMap<SourceId, CompiledUnit>,
    route: &RouteFile,
) -> RouteWasmLinkContext {
    let mut context = RouteWasmLinkContext::default();
    for import in &route.imports {
        let ImportTarget::CustomFunction { path, function } = import_base(import) else {
            continue;
        };
        let logical = logical_module_name(path);
        let Some(source) = registry.get_logical(RelSourceKind::Module, &logical) else {
            continue;
        };
        let Some(CompiledUnit::Module(module)) = compiled.get(source.id()) else {
            continue;
        };
        if !module.exports.iter().any(|export| export == function) {
            continue;
        }
        let Some(function_def) = module
            .functions
            .iter()
            .find(|candidate| candidate.name == *function)
        else {
            continue;
        };
        context.insert_module_function(
            binding_name(import),
            module_owner_from_logical_name(source.logical_name()),
            function_def.clone(),
            module.imports.clone(),
        );
    }
    context
}

fn runtime_env_from_settings(
    settings: &JsonValue,
) -> Result<BTreeMap<String, JsonValue>, RelcError> {''',
        "immutable RELC linked Module context",
    )
    text = one(
        text,
        '''    #[test]
    fn video_principal_survives_nested_module_to_route_propagation() {''',
        '''    #[test]
    fn linked_video_module_wrapper_compiles_native_with_exact_owner_grant() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "media/status",
                "module/media/status.module",
                r#":import[video-manager.status as vmStatus]
                   export function videoStatus() { return vmStatus(); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "video-status",
                "api/video-status.route",
                r#":import["./module/media/status".videoStatus]
                   class Route { get(req) { return videoStatus(); } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();
        assert!(image.route_wasm_artifact(route).is_some());
        assert!(image.route_wasm_fallback(route).is_none());
        let grants = image.container_capability_grants(route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, core_lib::ContainerCapabilityKind::Video);
        assert_eq!(grants[0].target, "module:media.status");
        assert_eq!(grants[0].operations, vec!["status"]);
    }

    #[test]
    fn linked_service_module_wrapper_compiles_native_with_exact_service_grant() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Service,
                "uac",
                "service/uac.service",
                r#":service[name = uac]
                   export function get_user(id) { return id; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Module,
                "accounts",
                "module/accounts.module",
                r#":import[service:uac.get_user as getUser]
                   export function lookup(id) { return getUser(id); }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "account-lookup",
                "api/account-lookup.route",
                r#":import["./module/accounts".lookup]
                   class Route { get(req) { return lookup("kate"); } }"#,
            ),
        ];
        let image =
            compile_runtime_image("server Main {}", sources, &serde_json::json!({})).unwrap();
        let route = image.routes.first().unwrap();
        assert!(image.route_wasm_artifact(route).is_some());
        assert!(image.route_wasm_fallback(route).is_none());
        let grants = image.container_capability_grants(route).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, core_lib::ContainerCapabilityKind::Service);
        assert_eq!(grants[0].target, "service:uac");
        assert_eq!(grants[0].operations, vec!["get_user"]);
    }

    #[test]
    fn video_principal_survives_nested_module_to_route_propagation() {''',
        "linked Module RELC integration tests",
    )
    return text


edit("engine/crates/route-engine/src/relc.rs", patch_relc)


def patch_docs(text: str) -> str:
    text = one(
        text,
        '''Route-WASM compiler generation v4 now emits direct `service:<name>.<operation>` imports with static JSON arguments through the existing versioned capability ABI, so those calls execute end-to-end through Controller authorization and the trusted Service Gateway. Service namespace imports remain interpreter-only to avoid widening authority; linked-module Video production remains separate work.''',
        '''Route-WASM compiler v4 introduced the generic direct Service capability-call emitter, while RELC continued to forbid Route REL from importing Service REL directly. Compiler generation v5 makes that authority usable without weakening the language boundary: a Route may natively call one exported Module function when RELC pins that exact function, and that Module may contain exactly one returned HTTP, Video, or Service host call with static JSON-resolvable arguments. Video ownership remains the canonical declaring Module principal and Service targets remain exact `service:<name>` grants. Namespace-wide, dynamic, multi-import, multi-statement, and nested host-call chains remain interpreter fallback.''',
        "linked Module authority docs",
    )
    text = one(
        text,
        '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 remains stable while compiler generation v4 emits real Controller-authorized capability calls for one directly imported host operation: `http.get`, `http.post`, `http.request`, or an exact `service:<name>.<operation>` export when all call arguments are static JSON. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind` as part of the versioned capability ABI, and the compiler uses one generic capability-call emitter rather than hardcoding Network- or Service-specific numeric discriminants. Namespace HTTP/Service imports and dynamic capability arguments remain explicit interpreter fallback, preventing native compilation from widening the operation grant.''',
        '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 remains stable while compiler generation v5 supports the existing direct HTTP host operations plus strict RELC-linked Module wrappers for exact HTTP, Video, and Service calls. A linked wrapper is native only when the Route has one imported exported Module function, the Route arguments are static JSON, the Module function contains exactly one return statement, and that return directly calls the Module's one exact host-capability import with JSON values or those bound parameters. Capability kind integers remain defined centrally by `ipc-protocol::CapabilityKind`, and one generic capability-call emitter handles all supported host kinds. Namespace imports, dynamic host arguments, wider Module bodies, and nested host-call chains remain explicit interpreter fallback.''',
        "native compiler v5 docs",
    )
    text = one(
        text,
        '''Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set, while a direct Service-function route registers only the exact `Service/service:<name>` operation it imports.''',
        '''Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set. RELC still rejects direct Route-to-Service imports. A compiler-v5 linked Module route instead registers only the exact requirements propagated from that Module, such as `Video/module:<owner>` or `Service/service:<name>` with the precise operation set.''',
        "native admission docs",
    )
    return text


edit("doc/runtime-image.md", patch_docs)
