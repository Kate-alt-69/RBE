from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


protocol = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
replace_once(
    protocol,
    '''pub enum CapabilityKind {
    Service,
    Network,
    Storage,
    Vault,
    HostFile,
    Video,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]''',
    '''pub enum CapabilityKind {
    Service,
    Network,
    Storage,
    Vault,
    HostFile,
    Video,
    Debug,
}

impl CapabilityKind {
    /// Stable numeric representation used by the guest WASM capability ABI.
    /// This mapping is part of CAPABILITY_ABI_VERSION and must never be inferred
    /// from Rust enum discriminants or serde ordering.
    pub const fn abi_code(self) -> i32 {
        match self {
            Self::Service => 0,
            Self::Network => 1,
            Self::Storage => 2,
            Self::Vault => 3,
            Self::HostFile => 4,
            Self::Video => 5,
            Self::Debug => 6,
        }
    }

    pub const fn from_abi_code(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Service),
            1 => Some(Self::Network),
            2 => Some(Self::Storage),
            3 => Some(Self::Vault),
            4 => Some(Self::HostFile),
            5 => Some(Self::Video),
            6 => Some(Self::Debug),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]''',
    "versioned capability kind ABI mapping",
)
replace_once(
    protocol,
    '''    #[test]
    fn capability_manifest_round_trip_preserves_exact_grants() {''',
    '''    #[test]
    fn capability_kind_numeric_abi_is_explicit_and_round_trips() {
        for (kind, code) in [
            (CapabilityKind::Service, 0),
            (CapabilityKind::Network, 1),
            (CapabilityKind::Storage, 2),
            (CapabilityKind::Vault, 3),
            (CapabilityKind::HostFile, 4),
            (CapabilityKind::Video, 5),
            (CapabilityKind::Debug, 6),
        ] {
            assert_eq!(kind.abi_code(), code);
            assert_eq!(CapabilityKind::from_abi_code(code), Some(kind));
        }
        assert_eq!(CapabilityKind::from_abi_code(-1), None);
        assert_eq!(CapabilityKind::from_abi_code(7), None);
    }

    #[test]
    fn capability_manifest_round_trip_preserves_exact_grants() {''',
    "capability kind ABI test",
)

executor = Path("container-runtime/crates/execution-engine/src/lib.rs")
replace_once(
    executor,
    '''                let Some(kind) = capability_kind_from_abi(kind) else {''',
    '''                let Some(kind) = CapabilityKind::from_abi_code(kind) else {''',
    "Wasmtime canonical capability kind decode",
)
replace_once(
    executor,
    '''fn capability_kind_from_abi(value: i32) -> Option<CapabilityKind> {
    match value {
        0 => Some(CapabilityKind::Service),
        1 => Some(CapabilityKind::Network),
        2 => Some(CapabilityKind::Storage),
        3 => Some(CapabilityKind::Vault),
        4 => Some(CapabilityKind::HostFile),
        5 => Some(CapabilityKind::Video),
        6 => Some(CapabilityKind::Debug),
        _ => None,
    }
}

''',
    '''''',
    "remove duplicate Wasmtime capability kind mapping",
)

compiler = Path("engine/crates/route-engine/src/wasm_compiler.rs")
replace_once(
    compiler,
    '''use core_lib::{
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};''',
    '''use core_lib::{
    ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_EXECUTION_INPUT_BYTES, PUBLIC_HTTP_TARGET,
};''',
    "compiler canonical capability kind type",
)
replace_once(
    compiler,
    '''            encode_public_http_module(operation, &payload),''',
    '''            encode_capability_call_module(
                ContainerCapabilityKind::Network,
                PUBLIC_HTTP_TARGET,
                operation,
                &payload,
            ),''',
    "HTTP through generic capability emitter",
)
replace_once(
    compiler,
    '''fn encode_public_http_module(operation: &str, payload: &[u8]) -> Vec<u8> {
    const NETWORK_CAPABILITY_KIND: i32 = 1;
    let target = PUBLIC_HTTP_TARGET.as_bytes();
    let operation = operation.as_bytes();''',
    '''fn encode_capability_call_module(
    kind: ContainerCapabilityKind,
    target: &str,
    operation: &str,
    payload: &[u8],
) -> Vec<u8> {
    let target = target.as_bytes();
    let operation = operation.as_bytes();''',
    "generic capability emitter signature",
)
replace_once(
    compiler,
    '''        .i32_const(NETWORK_CAPABILITY_KIND)''',
    '''        .i32_const(kind.abi_code())''',
    "compiler canonical capability kind encode",
)
replace_once(
    compiler,
    '''    #[test]
    fn aliased_static_http_get_is_native() {''',
    '''    #[test]
    fn generic_capability_emitter_uses_versioned_kind_mapping_for_video_and_service() {
        for (kind, target, operation) in [
            (
                ContainerCapabilityKind::Video,
                "module:media.bridge",
                "status",
            ),
            (
                ContainerCapabilityKind::Service,
                "service:uac-cache",
                "get_user",
            ),
        ] {
            let wasm = encode_capability_call_module(kind, target, operation, b"[]");
            wasmparser::validate(&wasm).unwrap();
            let expected_kind = kind;
            let expected_target = target.to_string();
            let expected_operation = operation.to_string();
            let host: CapabilityHost = Box::new(move |request| {
                assert_eq!(request.kind, expected_kind);
                assert_eq!(request.target, expected_target);
                assert_eq!(request.operation, expected_operation);
                assert_eq!(request.payload, b"[]");
                Ok(br#"{"ok":true}"#.to_vec())
            });
            let result = WasmExecutor::new()
                .unwrap()
                .execute_with_input_and_capabilities(
                    &wasm,
                    &[],
                    ExecutionLimits::default(),
                    Some(host),
                )
                .unwrap();
            assert_eq!(result.output, br#"{"ok":true}"#);
        }
    }

    #[test]
    fn aliased_static_http_get_is_native() {''',
    "generic Video/Service emitter round-trip test",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 retains the JSON invocation input for native `return req.body;` routes and adds real Controller-authorized `Network/public-http` capability calls for one directly imported `http.get`, `http.post`, or `http.request` operation when all call arguments are static JSON.''',
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Route-WASM ABI v3 retains the JSON invocation input for native `return req.body;` routes and adds real Controller-authorized `Network/public-http` capability calls for one directly imported `http.get`, `http.post`, or `http.request` operation when all call arguments are static JSON. Capability kind integers are defined centrally by `ipc-protocol::CapabilityKind` as part of the versioned capability ABI, and the compiler uses one generic capability-call emitter rather than hardcoding Network-specific numeric discriminants.''',
    "canonical capability kind ABI docs",
)
