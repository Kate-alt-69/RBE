from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


wasm = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    wasm,
    '''use core_lib::{
    ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};
use sha2::{Digest, Sha256};''',
    '''use std::collections::BTreeMap;

use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::{service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX};
use sha2::{Digest, Sha256};''',
    "linked compiler imports",
)
replace_once(
    wasm,
    '''use crate::ast::{Expr, ImportTarget, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 3;''',
    '''use crate::ast::{Expr, FunctionDef, ImportTarget, RouteFile, Statement};
use crate::modules::binding_name;

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 4;''',
    "compiler version and AST imports",
)
replace_once(
    wasm,
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

#[derive(Debug, Clone)]
struct DirectHostImport {
    binding: String,
    kind: ContainerCapabilityKind,
    target: String,
    operation: String,
}

/// Compile the currently supported native route subset.''',
    "linked compiler context types",
)
replace_once(
    wasm,
    '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no helper functions. In addition to literals and req.body passthrough, ABI v3
/// permits one directly imported `http.get/post/request` function with fully
/// static JSON arguments. Namespace imports stay interpreter-only so native
/// grants remain operation-exact rather than widening authority.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let http_import =
        match file.imports.as_slice() {
            [] => None,
            [import] => match direct_http_import(import) {
                Some(import) => Some(import),
                None => return fallback(
                    "native Route-WASM v3 only supports one direct http.get/post/request import",
                ),
            },
            _ => {
                return fallback(
                    "native Route-WASM v3 supports at most one direct host capability import",
                )
            }
        };
''',
    '''/// Native lowering remains intentionally strict: one HTTP method, one return,
/// no route helper functions. Compiler v4 keeps ABI v3 and additionally permits
/// one direct exported Module function when RELC supplies an immutable link
/// context. That Module function must itself be one return of one exact direct
/// HTTP/Video/Service capability import. Route arguments and the resulting host
/// arguments must be static JSON, so native lowering cannot widen authority or
/// invent dynamic host targets.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    let links = RouteWasmLinkContext::default();
    compile_route_with_links(file, &links)
}

pub(crate) fn compile_route_with_links(
    file: &RouteFile,
    links: &RouteWasmLinkContext,
) -> RouteWasmCompilation {
    let mut http_import = None;
    let mut linked_import = None;
    match file.imports.as_slice() {
        [] => {}
        [import] => {
            if let Some(found) = direct_http_import(import) {
                http_import = Some(found);
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
                    "native Route-WASM compiler v4 supports one direct http.get/post/request import or one linked Module function import",
                );
            }
        }
        _ => {
            return fallback(
                "native Route-WASM compiler v4 supports at most one direct or linked host-call import",
            )
        }
    }
''',
    "compile with immutable links",
)
replace_once(
    wasm,
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
            encode_capability_call_module(
                ContainerCapabilityKind::Network,
                PUBLIC_HTTP_TARGET,
                operation,
                &payload,
            ),
            RouteWasmInput::None,
        )
    } else if let Some(value) = static_json(expr) {''',
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
            encode_capability_call_module(
                ContainerCapabilityKind::Network,
                PUBLIC_HTTP_TARGET,
                operation,
                &payload,
            ),
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
    "linked module capability lowering branch",
)
replace_once(
    wasm,
    '''fn direct_http_import(import: &ImportTarget) -> Option<(String, String)> {
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

fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {''',
    '''fn base_import(import: &ImportTarget) -> &ImportTarget {
    match import {
        ImportTarget::Aliased { target, .. } => base_import(target),
        other => other,
    }
}

fn direct_http_import(import: &ImportTarget) -> Option<(String, String)> {
    let ImportTarget::BuiltinFunction { module, function } = base_import(import) else {
        return None;
    };
    (module == "http" && matches!(function.as_str(), "get" | "post" | "request"))
        .then(|| (binding_name(import), function.clone()))
}

fn direct_module_host_import(import: &ImportTarget, owner: &str) -> Option<DirectHostImport> {
    let binding = binding_name(import);
    let (kind, target, operation) = match base_import(import) {
        ImportTarget::BuiltinFunction { module, function }
            if module == "http" && matches!(function.as_str(), "get" | "post" | "request") =>
        {
            (
                ContainerCapabilityKind::Network,
                PUBLIC_HTTP_TARGET.to_string(),
                function.clone(),
            )
        }
        ImportTarget::BuiltinFunction { module, function }
            if matches!(module.as_str(), "vm" | "video-manager")
                && video_language_operation_allowed(function) =>
        {
            (
                ContainerCapabilityKind::Video,
                format!("{VIDEO_CAPABILITY_TARGET_PREFIX}{owner}"),
                function.clone(),
            )
        }
        ImportTarget::ServiceFunction { service, function }
            if service_capability_name_allowed(service) && valid_capability_operation(function) =>
        {
            (
                ContainerCapabilityKind::Service,
                format!("{SERVICE_CAPABILITY_TARGET_PREFIX}{service}"),
                function.clone(),
            )
        }
        _ => return None,
    };
    if target.len() > CONTAINER_MAX_CAPABILITY_TARGET_BYTES
        || operation.len() > CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
    {
        return None;
    }
    Some(DirectHostImport {
        binding,
        kind,
        target,
        operation,
    })
}

fn valid_capability_operation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
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
            "native linked Module call arity mismatch: route supplied {}, Module function expects {}",
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
    let host = direct_module_host_import(module_import, &linked.owner).ok_or_else(|| {
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
    "linked host import resolution",
)
replace_once(
    wasm,
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
    "static linked parameter substitution",
)
replace_once(
    wasm,
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
replace_once(
    wasm,
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
            Ok(br#"{"id":"kate"}"#.to_vec())
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
        assert_eq!(result.output, br#"{"id":"kate"}"#);
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
    "linked module compiler tests",
)
replace_once(
    wasm,
    '''        assert!(reason.contains("one direct http.get/post/request import"));''',
    '''        assert!(reason.contains("one direct http.get/post/request import or one linked Module function import"));''',
    "namespace fallback assertion",
)
replace_once(
    wasm,
    '''        assert!(reason.contains("outside the native Route-WASM v3 subset"));''',
    '''        assert!(reason.contains("outside the native Route-WASM v3 subset"));''',
    "preserve existing fallback assertion",
)

relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''use crate::wasm_compiler::{compile_route, RouteWasmCompilation};''',
    '''use crate::wasm_compiler::{
    compile_route_with_links, RouteWasmCompilation, RouteWasmLinkContext,
};''',
    "RELC linked compiler imports",
)
replace_once(
    relc,
    '''        match compile_route(file) {
            RouteWasmCompilation::Native(artifact) => {''',
    '''        let link_context = route_wasm_link_context(&registry, &compiled, file);
        match compile_route_with_links(file, &link_context) {
            RouteWasmCompilation::Native(artifact) => {''',
    "RELC linked route compilation",
)
replace_once(
    relc,
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
replace_once(
    relc,
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

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Route-WASM v3 still has no linked-module Service/Video call producer, so these grant+adapter boundaries are ready before end-to-end native linked-module host calls are enabled.''',
    '''Route-WASM compiler v4 (still using capability ABI v3) adds the first linked-Module host-call producer without widening Route authority: one directly imported exported Module function may lower natively when the Route call uses only static JSON arguments and the Module function is exactly one return of one directly imported HTTP, Video, or Service function. Video ownership comes from RELC's canonical Module principal, and Service targets remain exact `service:<name>` grants; dynamic, namespace-wide, multi-import, or multi-statement chains remain interpreter fallback.''',
    "linked Module capability docs",
)
replace_once(
    doc,
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 retains the JSON invocation input for native `return req.body;` routes and adds real Controller-authorized `Network/public-http` capability calls for one directly imported `http.get`, `http.post`, or `http.request` operation when all call arguments are static JSON. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind` as part of the versioned capability ABI, and the compiler uses one generic capability-call emitter rather than hardcoding Network-specific numeric discriminants. Namespace `http` imports and dynamic HTTP arguments remain explicit interpreter fallback, preventing native compilation from widening the operation grant.''',
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 retains the JSON invocation input for native `return req.body;` routes and real Controller-authorized capability calls. Compiler v4 supports one directly imported `http.get`, `http.post`, or `http.request` operation with static JSON arguments, plus the strict linked-Module wrapper described above for exact HTTP, Video, and Service calls. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind` as part of the versioned capability ABI, and the compiler uses one generic capability-call emitter rather than kind-specific numeric discriminants. Namespace imports, dynamic host arguments, and wider Module bodies remain explicit interpreter fallback, preventing native compilation from widening the operation grant.''',
    "native linked compiler docs",
)
