from pathlib import Path


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


def section(text: str, start: str, end: str, new: str, label: str) -> str:
    if text.count(start) != 1:
        raise SystemExit(f"{label}: start marker count={text.count(start)}")
    begin = text.index(start)
    finish = text.find(end, begin)
    if finish < 0:
        raise SystemExit(f"{label}: end marker missing")
    return text[:begin] + new + text[finish:]


wasm = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = wasm.read_text(encoding="utf-8")
text = one(
    text,
    '''use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::SERVICE_CAPABILITY_TARGET_PREFIX;''',
    '''use core_lib::{
    video_language_operation_allowed, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES,
    PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::{service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX};''',
    "shared capability limit imports",
)
text = one(
    text,
    '''/// Generation history: `pub const ROUTE_WASM_COMPILER_VERSION: u32 = 4`
/// introduced direct host-capability lowering. Generation 5 adds strict
/// immutable linked-Module host-call lowering while capability ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 5;''',
    '''/// Generation 6 removes the unreachable direct Route-to-Service lowering
/// left by generation 4. Service and Video host calls now require RELC's linked
/// Module authority boundary; capability ABI v3 stays stable.
pub const ROUTE_WASM_COMPILER_VERSION: u32 = 6;''',
    "compiler generation bump",
)
text = one(
    text,
    '''                    "native Route-WASM v5 only supports one direct http.get/post/request or service:<name>.<operation> import or one linked Module function import",''',
    '''                    "native Route-WASM v6 only supports one direct http.get/post/request import or one linked Module function import",''',
    "direct import fallback",
)
text = one(
    text,
    '''                "native Route-WASM v5 supports at most one direct or linked host-call import",''',
    '''                "native Route-WASM v6 supports at most one direct or linked host-call import",''',
    "multiple import fallback",
)
text = section(
    text,
    "fn direct_capability_import(import: &ImportTarget) -> Option<DirectCapabilityImport> {",
    "fn base_import(import: &ImportTarget) -> &ImportTarget {",
    '''fn direct_capability_import(import: &ImportTarget) -> Option<DirectCapabilityImport> {
    let (binding, base) = match import {
        ImportTarget::Aliased { target, alias } => (alias.clone(), target.as_ref()),
        ImportTarget::BuiltinFunction { function, .. } => (function.clone(), import),
        _ => return None,
    };

    match base {
        ImportTarget::BuiltinFunction { module, function }
            if module == "http" && matches!(function.as_str(), "get" | "post" | "request") =>
        {
            Some(DirectCapabilityImport {
                binding,
                kind: ContainerCapabilityKind::Network,
                target: PUBLIC_HTTP_TARGET.to_string(),
                operation: function.clone(),
            })
        }
        _ => None,
    }
}

''',
    "direct Route host resolver",
)
text = section(
    text,
    "fn direct_linked_capability_import(\n",
    "fn lower_linked_module_capability_call(\n",
    '''fn direct_linked_capability_import(
    import: &ImportTarget,
    owner: &str,
) -> Option<DirectCapabilityImport> {
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
    Some(DirectCapabilityImport {
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

''',
    "linked host resolver",
)
text = section(
    text,
    "    #[test]\n    fn direct_static_service_call_is_native_v4_capability_call() {",
    "    #[test]\n    fn aliased_static_http_get_is_native() {",
    '''    #[test]
    fn direct_service_route_import_stays_outside_native_subset() {
        let route = parse(
            r#":import[service:uac.get_user]
               class Route { get(req) { return get_user("alice"); } }"#,
        );
        let RouteWasmCompilation::InterpreterFallback { reason } = compile_route(&route) else {
            panic!("Route-to-Service must remain behind an exported Module boundary");
        };
        assert!(reason.contains("linked Module function"));
    }

''',
    "remove direct Service native tests",
)
text = one(
    text,
    '''        assert!(reason
            .contains("one direct http.get/post/request or service:<name>.<operation> import"));''',
    '''        assert!(reason.contains(
            "one direct http.get/post/request import or one linked Module function import"
        ));''',
    "HTTP namespace fallback assertion",
)
wasm.write_text(text, encoding="utf-8")


doc = Path("doc/runtime-image.md")
d = doc.read_text(encoding="utf-8")
d = one(
    d,
    '''Route-WASM compiler v4 introduced the generic direct Service capability-call emitter, while RELC continued to forbid Route REL from importing Service REL directly. Compiler generation v5 makes that authority usable without weakening the language boundary: a Route may natively call one exported Module function when RELC pins that exact function, and that Module may contain exactly one returned HTTP, Video, or Service host call with static JSON-resolvable arguments.''',
    '''Route-WASM compiler generation v6 removes the leftover direct Route-to-Service compiler path entirely. A Route may reach Service or Video natively only through one exported Module function pinned by RELC; that Module may contain exactly one returned HTTP, Video, or Service host call with static JSON-resolvable arguments.''',
    "runtime authority docs",
)
d = one(
    d,
    '''Route-WASM ABI v3 remains stable while compiler generation v5 supports the existing direct HTTP host operations plus strict RELC-linked Module wrappers for exact HTTP, Video, and Service calls.''',
    '''Route-WASM ABI v3 remains stable while compiler generation v6 supports direct HTTP host operations plus strict RELC-linked Module wrappers for exact HTTP, Video, and Service calls; Service and Video are never direct Route imports.''',
    "native compiler docs",
)
doc.write_text(d, encoding="utf-8")
