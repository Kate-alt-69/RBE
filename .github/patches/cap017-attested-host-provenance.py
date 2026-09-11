from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


ipc = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
replace_once(
    ipc,
    '''pub const HOST_CAPABILITY_PROTOCOL_VERSION: u16 = 1;''',
    '''pub const HOST_CAPABILITY_PROTOCOL_VERSION: u16 = 2;''',
    "host capability protocol v2",
)
replace_once(
    ipc,
    '''pub struct HostCapabilityRequest {
    pub version: u16,
    pub auth_token: String,
    pub execution_id: String,
    pub call_id: u64,
    pub kind: CapabilityKind,''',
    '''pub struct HostCapabilityRequest {
    pub version: u16,
    pub auth_token: String,
    pub execution_id: String,
    /// Controller-attested execution provenance. These fields are copied from
    /// the admitted ExecutionTask and are never supplied by the WASM guest.
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
    pub call_id: u64,
    pub kind: CapabilityKind,''',
    "host capability attested provenance fields",
)
replace_once(
    ipc,
    '''    #[test]
    fn worker_pipe_round_trips_binary_input_and_output() {''',
    '''    #[test]
    fn host_capability_protocol_round_trip_preserves_attested_provenance() {
        let request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: "secret".into(),
            execution_id: "exec-1".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:api/video".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-2".into(),
            generation: 7,
            call_id: 9,
            kind: CapabilityKind::Video,
            target: "video".into(),
            operation: "status".into(),
            payload: b"[]".to_vec(),
            max_response_bytes: 4096,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let frame = read_frame(&mut bytes.as_slice()).unwrap();
        let decoded: HostCapabilityRequest = serde_json::from_slice(&frame).unwrap();
        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.runtime_image, "ab".repeat(32));
        assert_eq!(decoded.source_id, "route:api/video");
        assert_eq!(decoded.capability_abi, CAPABILITY_ABI_VERSION);
        assert_eq!(decoded.environment, "general-2");
        assert_eq!(decoded.generation, 7);
    }

    #[test]
    fn worker_pipe_round_trips_binary_input_and_output() {''',
    "host capability provenance roundtrip test",
)

envproc = Path("container-runtime/crates/container-bin/src/environment_process.rs")
replace_once(
    envproc,
    '''pub struct CapabilityDispatchRequest {
    pub execution_id: String,
    pub call_id: u64,
    pub kind: CapabilityKind,''',
    '''pub struct CapabilityDispatchRequest {
    pub execution_id: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
    pub call_id: u64,
    pub kind: CapabilityKind,''',
    "Controller dispatch provenance fields",
)
replace_once(
    envproc,
    '''        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.call_id,
            request.kind,''',
    '''        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.runtime_image.as_str(),
            request.source_id.as_str(),
            request.capability_abi,
            request.environment.as_str(),
            request.generation,
            request.call_id,
            request.kind,''',
    "unavailable dispatcher consumes provenance",
)
replace_once(
    envproc,
    '''        let host_request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: token.clone(),
            execution_id: execution_id.clone(),
            call_id,
            kind: request.kind,''',
    '''        let host_request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: token.clone(),
            execution_id: execution_id.clone(),
            runtime_image: request.runtime_image,
            source_id: request.source_id,
            capability_abi: request.capability_abi,
            environment: request.environment,
            generation: request.generation,
            call_id,
            kind: request.kind,''',
    "host dispatch forwards Controller provenance",
)
replace_once(
    envproc,
    '''        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
            call_id: call.call_id,
            kind: call.kind,''',
    '''        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: provenance.environment.clone(),
            generation: provenance.generation,
            call_id: call.call_id,
            kind: call.kind,''',
    "Controller stamps dispatcher provenance",
)

backend = Path("engine/crates/backend/src/host_capability.rs")
replace_once(
    backend,
    '''    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    '''    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse, CAPABILITY_ABI_VERSION,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    "Backend CAP ABI import",
)
replace_once(
    backend,
    '''const MAX_EXECUTION_ID_BYTES: usize = 128;
const MAX_LOGICAL_NAME_BYTES: usize = 256;''',
    '''const MAX_EXECUTION_ID_BYTES: usize = 128;
const MAX_LOGICAL_NAME_BYTES: usize = 256;
const MAX_SOURCE_ID_BYTES: usize = 512;''',
    "Backend SourceId bound",
)
replace_once(
    backend,
    '''    if request.execution_id.is_empty() || request.execution_id.len() > MAX_EXECUTION_ID_BYTES {
        return error(
            "CAPABILITY_HOST_INVALID_REQUEST",
            "invalid execution identity",
        );
    }
    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {''',
    '''    if request.execution_id.is_empty() || request.execution_id.len() > MAX_EXECUTION_ID_BYTES {
        return error(
            "CAPABILITY_HOST_INVALID_REQUEST",
            "invalid execution identity",
        );
    }
    if request.capability_abi != CAPABILITY_ABI_VERSION
        || !valid_runtime_image(&request.runtime_image)
        || !valid_source_id(&request.source_id)
        || !valid_environment_identity(&request.environment)
    {
        return error(
            "CAPABILITY_HOST_PROVENANCE_INVALID",
            "trusted host capability provenance is invalid",
        );
    }
    if request.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {''',
    "Backend verifies Controller-attested provenance",
)
replace_once(
    backend,
    '''                tracing::warn!(
                    execution_id = %request.execution_id,
                    call_id = request.call_id,
                    operation = %request.operation,''',
    '''                tracing::warn!(
                    execution_id = %request.execution_id,
                    runtime_image = %request.runtime_image,
                    source_id = %request.source_id,
                    environment = %request.environment,
                    generation = request.generation,
                    call_id = request.call_id,
                    operation = %request.operation,''',
    "Network failure provenance log",
)
replace_once(
    backend,
    '''            tracing::warn!(
                execution_id = %request.execution_id,
                call_id = request.call_id,
                service = %service_name,''',
    '''            tracing::warn!(
                execution_id = %request.execution_id,
                runtime_image = %request.runtime_image,
                source_id = %request.source_id,
                environment = %request.environment,
                generation = request.generation,
                call_id = request.call_id,
                service = %service_name,''',
    "Service failure provenance log",
)
replace_once(
    backend,
    '''fn normalize_service_target(target: &str) -> Option<&str> {''',
    '''fn valid_runtime_image(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SOURCE_ID_BYTES
        && !value.contains('\\0')
        && !value.chars().any(char::is_control)
}

fn valid_environment_identity(value: &str) -> bool {
    matches!(
        value,
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5" | "payment"
    )
}

fn normalize_service_target(target: &str) -> Option<&str> {''',
    "Backend trusted provenance validators",
)
replace_once(
    backend,
    '''    #[test]
    fn service_target_normalization_is_logical_only() {''',
    '''    #[test]
    fn attested_host_provenance_format_is_strict() {
        assert!(valid_runtime_image(&"ab".repeat(32)));
        assert!(!valid_runtime_image(&"AB".repeat(32)));
        assert!(valid_source_id("module:physical:media/player"));
        assert!(!valid_source_id("route:\\napi"));
        assert!(valid_environment_identity("general-5"));
        assert!(valid_environment_identity("payment"));
        assert!(!valid_environment_identity("general"));
        assert!(!valid_environment_identity("visitor-ip-1"));
    }

    #[tokio::test]
    async fn host_bridge_rejects_invalid_attested_provenance_before_dispatch() {
        let services = Arc::new(RwLock::new(None));
        let request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: "secret".into(),
            execution_id: "exec-1".into(),
            runtime_image: "NOT-A-RUNTIME-IMAGE".into(),
            source_id: "route:api/test".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-1".into(),
            generation: 3,
            call_id: 1,
            kind: CapabilityKind::Network,
            target: PUBLIC_HTTP_TARGET.into(),
            operation: "get".into(),
            payload: br#"[\"https://example.com\"]"#.to_vec(),
            max_response_bytes: 4096,
        };
        let response = dispatch_request(request, "secret", &services).await;
        let HostCapabilityResponse::Error { code, .. } = response else {
            panic!("malformed provenance must fail before capability dispatch");
        };
        assert_eq!(code, "CAPABILITY_HOST_PROVENANCE_INVALID");
    }

    #[test]
    fn service_target_normalization_is_logical_only() {''',
    "Backend attested provenance tests",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image.''',
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image. Authorized host-capability dispatch uses protocol v2 to carry Controller-attested Runtime Image, SourceId, capability ABI, exact Environment, and generation alongside each call; those ownership fields come from admitted execution provenance and are never accepted from guest WASM.''',
    "host capability provenance documentation",
)
