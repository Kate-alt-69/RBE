from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_section(path: Path, start: str, end: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    if text.count(start) != 1 or text.count(end) < 1:
        raise SystemExit(f"{label}: section markers are missing or ambiguous")
    begin = text.index(start)
    finish = text.index(end, begin)
    path.write_text(text[:begin] + new + text[finish:], encoding="utf-8")


wasm = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    wasm,
    '''use core_lib::{
    ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};
use service_runtime::SERVICE_CAPABILITY_TARGET_PREFIX;''',
    '''use std::collections::BTreeMap;

use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::{service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX};''',
    "linked compiler imports",
)
replace_once(
    wasm,
    '''use crate::ast::{Expr, ImportTarget, RouteFile, Statement};

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 4;''',
    '''use crate::ast::{Expr, FunctionDef, ImportTarget, RouteFile, Statement};
use crate::modules::binding_name;

pub const ROUTE_WASM_ABI_VERSION: u32 = 3;
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 5;''',
    "compiler generation and AST imports",
)
replace_once(
    wasm,
    '''impl RouteWasmCompilation {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

''',
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

''',
    "linked compiler context",
)
replace_section(
    wasm,
    "/// Compile the currently supported native route subset.",
    "fn fallback(reason: impl Into<String>) -> RouteWasmCompilation {",
    '''/// Compile the currently supported native route subset.
///
/// Native lowering remains intentionally strict: one HTTP method and one return,
/// with no Route helper functions. Compiler generation v5 keeps capability ABI
/// v3 and adds one immutable RELC-linked Module function. That Module function
/// must itself be exactly one return of one direct HTTP, Video, or Service host
/// import. Route arguments and resulting host arguments must resolve to static
/// JSON, so native compilation cannot invent dynamic targets or widen authority.
pub fn compile_route(file: &RouteFile) -> RouteWasmCompilation {
    compile_route_with_links(file, &RouteWasmLinkContext::default())
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
                    "native Route-WASM compiler v5 supports one direct http.get/post/request import or one linked Module function import",
                );
            }
        }
        _ => {
            return fallback(
                "native Route-WASM compiler v5 supports at most one direct or linked host-call import",
            )
        }
    }
    if !file.functions.is_empty() {
        return fallback("route helper functions are not WASM-native yet");
    }
    if file.methods.len() != 1 {
        return fallback("native route compilation currently requires exactly one HTTP method");
    }

    let method = &file.methods[0];
    let [Statement::Return(expr)] = method.body.as_slice() else {
        return fallback("native route body currently requires one literal return statement");
    };
    let (bytes, input) = if let Some((binding, operation)) = http_import.as_ref() {
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
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    RouteWasmCompilation::Native(RouteWasmArtifact {
        verb: method.verb.clone(),
        bytes,
        sha256,
        input,
    })
}

''',
    "linked compile entrypoint",
)
replace_section(
    wasm,
    "#[derive(Debug, Clone)]\nstruct DirectCapabilityImport {",
    "fn static_direct_call(binding: &str, expr: &Expr) -> Option<Vec<serde_json::Value>> {",
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

''',
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
replace_section(
    wasm,
    "    #[test]\n    fn direct_static_service_call_is_native_v4_capability_call() {",
    "    #[test]\n    fn aliased_static_http_get_is_native() {",
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
    fn linked_module_video_call_uses_declaring_module_owner() {
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
            Ok(br#"{"enabled":true}"#.to_vec())
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
        assert_eq!(result.output, br#"{"enabled":true}"#);
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
    fn direct_service_route_import_stays_outside_native_subset() {
        let route = parse(
            r#":import[service:uac.get_user]
               class Route { get(req) { return get_user("alice"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("Route -> Service must remain behind a Module boundary");
        };
        assert!(reason.contains("linked Module function"));
    }

''',
    "replace unreachable direct Service tests",
)
replace_once(
    wasm,
    '''        assert!(reason
            .contains("one direct http.get/post/request or service:<name>.<operation> import"));''',
    '''        assert!(reason.contains(
            "one direct http.get/post/request import or one linked Module function import"
        ));''',
    "HTTP namespace fallback assertion",
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
    fn direct_service_route_still_fails_relc_capability_validation() {
        let sources = vec![
            PhysicalRelSource::new(
                RelSourceKind::Service,
                "uac",
                "service/uac.service",
                r#":service[name = uac]
                   export function get_user(id) { return id; }"#,
            ),
            PhysicalRelSource::new(
                RelSourceKind::Route,
                "bad-direct-service",
                "api/bad-direct-service.route",
                r#":import[service:uac.get_user]
                   class Route { get(req) { return get_user("kate"); } }"#,
            ),
        ];
        let error = compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
            .expect_err("Route -> Service must remain forbidden by RELC");
        assert!(error
            .to_string()
            .contains("Route REL cannot directly import Service REL; use a module boundary"));
    }

    #[test]
    fn video_principal_survives_nested_module_to_route_propagation() {''',
    "linked Module RELC integration tests",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Service requirements lower to exact `Service/service:<name>` Controller grants with compiler-derived exported operation sets, and the authenticated Backend adapter rejects raw/unprefixed targets before calling `ServiceManager`. Route-WASM compiler generation v4 now emits direct `service:<name>.<operation>` imports with static JSON arguments through the existing versioned capability ABI, so those calls execute end-to-end through Controller authorization and the trusted Service Gateway. Service namespace imports remain interpreter-only to avoid widening authority; linked-module Video production remains separate work.''',
    '''Service requirements lower to exact `Service/service:<name>` Controller grants with compiler-derived exported operation sets, and the authenticated Backend adapter rejects raw/unprefixed targets before calling `ServiceManager`. Route REL still cannot import Service REL directly. Route-WASM compiler generation v5 instead preserves that boundary: one directly imported exported Module function may lower natively when the Route call uses only static JSON arguments and the Module function is exactly one return of one directly imported HTTP, Video, or Service function. Video ownership comes from RELC's canonical Module principal, Service targets remain exact `service:<name>` grants, and dynamic, namespace-wide, multi-import, or multi-statement chains remain interpreter fallback.''',
    "linked Module capability docs",
)
replace_once(
    doc,
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 remains stable while compiler generation v4 emits real Controller-authorized capability calls for one directly imported host operation: `http.get`, `http.post`, `http.request`, or an exact `service:<name>.<operation>` export when all call arguments are static JSON. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind` as part of the versioned capability ABI, and the compiler uses one generic capability-call emitter rather than hardcoding Network- or Service-specific numeric discriminants. Namespace HTTP/Service imports and dynamic capability arguments remain explicit interpreter fallback, preventing native compilation from widening the operation grant.''',
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM capability ABI v3 remains stable while compiler generation v5 emits real Controller-authorized calls either for one directly imported `http.get/post/request` operation or for the strict RELC-linked Module wrapper described above. The linked wrapper can target exact HTTP, Video, or Service operations without granting Route REL direct Service authority. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind`, and the compiler uses one generic capability-call emitter rather than kind-specific numeric discriminants. Namespace imports, dynamic host arguments, and wider Module bodies remain explicit interpreter fallback.''',
    "native linked compiler docs",
)
replace_once(
    doc,
    '''Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set, while a direct Service-function route registers only the exact `Service/service:<name>` operation it imports.''',
    '''Capability-free native routes register an empty manifest; a directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set. A linked-Module native route registers only the exact requirements RELC propagated from that Module, such as `Video/module:<owner>` or `Service/service:<name>`; direct Route-to-Service imports remain invalid.''',
    "native manifest docs",
)
