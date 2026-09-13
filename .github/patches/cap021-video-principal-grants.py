from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


video = Path("engine/crates/core/src/video_language.rs")
replace_once(
    video,
    '''use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, ReserveLiveSessionRequest, VideoAssetState,
    VideoLiveSession, VideoManager, VideoSourceType, VideoVariant,
};

#[derive(Clone)]''',
    '''use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, ReserveLiveSessionRequest, VideoAssetState,
    VideoLiveSession, VideoManager, VideoSourceType, VideoVariant,
};

/// Exact public operation names accepted by the Video language facade. RELC,
/// Container grant lowering, and the trusted host adapter all consume this one
/// list so authority cannot drift from the actual language surface.
pub const VIDEO_LANGUAGE_OPERATIONS: &[&str] = &[
    "status",
    "databaseHealth",
    "database_health",
    "get",
    "job",
    "variants",
    "create",
    "queueDownload",
    "queue_download",
    "reserveLive",
    "reserve_live",
    "liveSession",
    "live_session",
    "endLive",
    "end_live",
];

/// Controller-visible Video targets are principals, never sockets or database
/// handles. The suffix is the canonical Module owner supplied by RELC.
pub const VIDEO_CAPABILITY_TARGET_PREFIX: &str = "module:";

pub fn video_language_operation_allowed(operation: &str) -> bool {
    VIDEO_LANGUAGE_OPERATIONS.contains(&operation)
}

#[derive(Clone)]''',
    "central Video language capability surface",
)
replace_once(
    video,
    '''    #[test]
    fn creates_and_reads_only_the_calling_modules_assets() {''',
    '''    #[test]
    fn exported_operation_allowlist_matches_language_aliases() {
        for operation in VIDEO_LANGUAGE_OPERATIONS {
            assert!(video_language_operation_allowed(operation));
        }
        assert!(!video_language_operation_allowed("deleteEverything"));
        assert_eq!(VIDEO_CAPABILITY_TARGET_PREFIX, "module:");
    }

    #[test]
    fn creates_and_reads_only_the_calling_modules_assets() {''',
    "Video operation surface test",
)

core = Path("engine/crates/core/src/lib.rs")
replace_once(
    core,
    '''pub use video_language::{VideoLanguage, VideoLanguageError};''',
    '''pub use video_language::{
    video_language_operation_allowed, VideoLanguage, VideoLanguageError,
    VIDEO_CAPABILITY_TARGET_PREFIX, VIDEO_LANGUAGE_OPERATIONS,
};''',
    "export Video capability contract",
)

relc = Path("engine/crates/route-engine/src/relc.rs")
replace_once(
    relc,
    '''use serde_json::Value as JsonValue;
use service_runtime::ServiceCatalog;''',
    '''use core_lib::VIDEO_LANGUAGE_OPERATIONS;
use serde_json::Value as JsonValue;
use service_runtime::ServiceCatalog;''',
    "RELC shared Video operation surface import",
)
replace_once(
    relc,
    '''const HTTP_HOST_OPERATIONS: &[&str] = &["get", "post", "request"];
const VIDEO_HOST_OPERATIONS: &[&str] = &[
    "status",
    "databaseHealth",
    "database_health",
    "get",
    "job",
    "variants",
    "create",
    "queueDownload",
    "queue_download",
    "reserveLive",
    "reserve_live",
    "liveSession",
    "live_session",
    "endLive",
    "end_live",
];''',
    '''const HTTP_HOST_OPERATIONS: &[&str] = &["get", "post", "request"];''',
    "remove duplicate RELC Video operation surface",
)
replace_once(
    relc,
    '''        "vm" | "video-manager" => VIDEO_HOST_OPERATIONS,''',
    '''        "vm" | "video-manager" => VIDEO_LANGUAGE_OPERATIONS,''',
    "RELC shared Video operation lookup",
)

runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''use core_lib::{
    ContainerCapabilityGrant, ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    PUBLIC_HTTP_TARGET,
};''',
    '''use core_lib::{
    video_language_operation_allowed, ContainerCapabilityGrant, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};''',
    "Runtime Image Video capability contract import",
)
replace_once(
    runtime_image,
    '''fn lower_container_grants(
    requirements: &BTreeSet<RuntimeCapabilityRequirement>,
) -> Result<Vec<ContainerCapabilityGrant>, RuntimeCapabilityLoweringError> {
    let mut public_http_operations = BTreeSet::new();
    for requirement in requirements {''',
    '''fn lower_container_grants(
    requirements: &BTreeSet<RuntimeCapabilityRequirement>,
) -> Result<Vec<ContainerCapabilityGrant>, RuntimeCapabilityLoweringError> {
    let mut public_http_operations = BTreeSet::new();
    let mut video_operations = BTreeMap::<String, BTreeSet<String>>::new();
    for requirement in requirements {''',
    "collect Video grants by principal",
)
replace_once(
    runtime_image,
    '''            RuntimeCapabilityRequirement::Video { owner, operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Video capability principal {owner:?} operation {operation:?} has no native Container grant lowering yet"
                    ),
                });
            }''',
    '''            RuntimeCapabilityRequirement::Video { owner, operation } => {
                if !valid_video_owner(owner) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Video capability principal {owner:?} is not a canonical Module owner"
                        ),
                    });
                }
                if !video_language_operation_allowed(operation) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Video capability operation {operation:?} is not part of the Video language surface"
                        ),
                    });
                }
                video_operations
                    .entry(owner.clone())
                    .or_default()
                    .insert(operation.clone());
            }''',
    "lower exact Video requirement",
)
replace_once(
    runtime_image,
    '''    if public_http_operations.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![ContainerCapabilityGrant {
        kind: ContainerCapabilityKind::Network,
        target: PUBLIC_HTTP_TARGET.to_string(),
        operations: public_http_operations.into_iter().collect(),
        // These are capability-envelope limits, not HTTP body limits. The
        // shared Network Broker applies the stricter HTTP request/response
        // policy after Controller authorization.
        max_request_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        max_response_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
    }])
}''',
    '''    let mut grants = Vec::new();
    if !public_http_operations.is_empty() {
        grants.push(ContainerCapabilityGrant {
            kind: ContainerCapabilityKind::Network,
            target: PUBLIC_HTTP_TARGET.to_string(),
            operations: public_http_operations.into_iter().collect(),
            // These are capability-envelope limits, not HTTP body limits. The
            // shared Network Broker applies the stricter HTTP request/response
            // policy after Controller authorization.
            max_request_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
            max_response_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        });
    }
    for (owner, operations) in video_operations {
        grants.push(ContainerCapabilityGrant {
            kind: ContainerCapabilityKind::Video,
            target: format!("{VIDEO_CAPABILITY_TARGET_PREFIX}{owner}"),
            operations: operations.into_iter().collect(),
            max_request_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
            max_response_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        });
    }
    Ok(grants)
}

fn valid_video_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 249
        && owner.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
        })
}''',
    "emit Video principal grants",
)
replace_once(
    runtime_image,
    '''    #[test]
    fn unsupported_native_capability_requirements_fail_closed() {
        for requirement in [
            RuntimeCapabilityRequirement::Video {
                owner: "media.bridge".into(),
                operation: "status".into(),
            },
            RuntimeCapabilityRequirement::Service {
                service: "uac".into(),
                operation: "get_user".into(),
            },
        ] {
            let error = lower_container_grants(&BTreeSet::from([requirement]))
                .expect_err("unsupported capability must not be silently dropped");
            assert!(error
                .message
                .contains("no native Container grant lowering yet"));
        }
    }

    #[test]
    fn unknown_public_http_operation_fails_closed() {''',
    '''    #[test]
    fn video_requirements_lower_to_exact_module_principal_grants() {
        let requirements = BTreeSet::from([
            RuntimeCapabilityRequirement::Video {
                owner: "media.bridge".into(),
                operation: "status".into(),
            },
            RuntimeCapabilityRequirement::Video {
                owner: "media.bridge".into(),
                operation: "get".into(),
            },
            RuntimeCapabilityRequirement::Video {
                owner: "media.other".into(),
                operation: "variants".into(),
            },
        ]);
        let grants = lower_container_grants(&requirements).unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].kind, ContainerCapabilityKind::Video);
        assert_eq!(grants[0].target, "module:media.bridge");
        assert_eq!(grants[0].operations, vec!["get", "status"]);
        assert_eq!(grants[1].kind, ContainerCapabilityKind::Video);
        assert_eq!(grants[1].target, "module:media.other");
        assert_eq!(grants[1].operations, vec!["variants"]);
    }

    #[test]
    fn invalid_video_operation_or_principal_fails_closed() {
        for requirement in [
            RuntimeCapabilityRequirement::Video {
                owner: "media.bridge".into(),
                operation: "deleteEverything".into(),
            },
            RuntimeCapabilityRequirement::Video {
                owner: "../media".into(),
                operation: "status".into(),
            },
        ] {
            assert!(lower_container_grants(&BTreeSet::from([requirement])).is_err());
        }
    }

    #[test]
    fn service_requirements_remain_fail_closed() {
        let requirement = RuntimeCapabilityRequirement::Service {
            service: "uac".into(),
            operation: "get_user".into(),
        };
        let error = lower_container_grants(&BTreeSet::from([requirement]))
            .expect_err("Service lowering is not implemented yet");
        assert!(error
            .message
            .contains("no native Container grant lowering yet"));
    }

    #[test]
    fn unknown_public_http_operation_fails_closed() {''',
    "Video grant tests",
)

host = Path("engine/crates/backend/src/host_capability.rs")
replace_once(
    host,
    '''use core_lib::{call_public_http, PUBLIC_HTTP_TARGET};''',
    '''use core_lib::{
    call_public_http, video_language_operation_allowed, VideoLanguage,
    VIDEO_CAPABILITY_TARGET_PREFIX, PUBLIC_HTTP_TARGET,
};''',
    "Backend Video host imports",
)
replace_once(
    host,
    '''pub struct HostCapabilityBridge {
    endpoint: HostCapabilityEndpoint,
    services: Arc<RwLock<Option<ServiceManager>>>,
    server_task: JoinHandle<()>,
}''',
    '''pub struct HostCapabilityBridge {
    endpoint: HostCapabilityEndpoint,
    services: Arc<RwLock<Option<ServiceManager>>>,
    video: Arc<RwLock<VideoLanguage>>,
    server_task: JoinHandle<()>,
}''',
    "Host bridge Video state",
)
replace_once(
    host,
    '''        let services = Arc::new(RwLock::new(None));
        let server_services = Arc::clone(&services);
        let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));''',
    '''        let services = Arc::new(RwLock::new(None));
        let video = Arc::new(RwLock::new(VideoLanguage::new(None)));
        let server_services = Arc::clone(&services);
        let server_video = Arc::clone(&video);
        let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));''',
    "Host bridge Video initialization",
)
replace_once(
    host,
    '''                let services = Arc::clone(&server_services);
                let expected_token = token.clone();
                tokio::spawn(async move {''',
    '''                let services = Arc::clone(&server_services);
                let video = Arc::clone(&server_video);
                let expected_token = token.clone();
                tokio::spawn(async move {''',
    "clone Video host state per call",
)
replace_once(
    host,
    '''                        handle_connection(stream, expected_token, services),''',
    '''                        handle_connection(stream, expected_token, services, video),''',
    "pass Video host state",
)
replace_once(
    host,
    '''        Ok(Self {
            endpoint,
            services,
            server_task,
        })''',
    '''        Ok(Self {
            endpoint,
            services,
            video,
            server_task,
        })''',
    "store Video host state",
)
replace_once(
    host,
    '''    pub async fn install_service_manager(&self, manager: ServiceManager) {
        *self.services.write().await = Some(manager);
    }
}''',
    '''    pub async fn install_service_manager(&self, manager: ServiceManager) {
        *self.services.write().await = Some(manager);
    }

    pub async fn install_video_manager(&self, manager: Option<Arc<video_manager::VideoManager>>) {
        *self.video.write().await = VideoLanguage::new(manager);
    }
}''',
    "install trusted Video Manager",
)
replace_once(
    host,
    '''async fn handle_connection(
    mut stream: TcpStream,
    expected_token: String,
    services: Arc<RwLock<Option<ServiceManager>>>,
) -> anyhow::Result<()> {
    let request: HostCapabilityRequest = read_typed_frame(&mut stream).await?;
    let response = dispatch_request(request, &expected_token, &services).await;''',
    '''async fn handle_connection(
    mut stream: TcpStream,
    expected_token: String,
    services: Arc<RwLock<Option<ServiceManager>>>,
    video: Arc<RwLock<VideoLanguage>>,
) -> anyhow::Result<()> {
    let request: HostCapabilityRequest = read_typed_frame(&mut stream).await?;
    let response = dispatch_request(request, &expected_token, &services, &video).await;''',
    "handle Video capability connection",
)
replace_once(
    host,
    '''async fn dispatch_request(
    request: HostCapabilityRequest,
    expected_token: &str,
    services: &Arc<RwLock<Option<ServiceManager>>>,
) -> HostCapabilityResponse {''',
    '''async fn dispatch_request(
    request: HostCapabilityRequest,
    expected_token: &str,
    services: &Arc<RwLock<Option<ServiceManager>>>,
    video: &Arc<RwLock<VideoLanguage>>,
) -> HostCapabilityResponse {''',
    "dispatch Video host state",
)
replace_once(
    host,
    '''    if request.kind != CapabilityKind::Service {
        return error(
            "CAPABILITY_KIND_UNSUPPORTED",
            "this trusted host adapter does not support that capability kind",
        );
    }
    let Some(service_name) = normalize_service_target(&request.target) else {''',
    '''    if request.kind == CapabilityKind::Video {
        let Some(module_owner) = normalize_video_target(&request.target) else {
            return error(
                "CAPABILITY_HOST_INVALID_TARGET",
                "Video capability target must be an exact module principal",
            );
        };
        if !video_language_operation_allowed(&request.operation) {
            return error(
                "CAPABILITY_HOST_INVALID_OPERATION",
                "invalid Video capability operation",
            );
        }
        let args: Vec<Value> = match serde_json::from_slice(&request.payload) {
            Ok(args) => args,
            Err(_) => {
                return error(
                    "CAPABILITY_VIDEO_ARGS_INVALID",
                    "Video capability payload must be a JSON argument array",
                )
            }
        };
        let language = video.read().await.clone();
        let value = match language.call(module_owner, &request.operation, &args) {
            Ok(value) => value,
            Err(call_error) => {
                tracing::warn!(
                    execution_id = %request.execution_id,
                    runtime_image = %request.runtime_image,
                    source_id = %request.source_id,
                    environment = %request.environment,
                    generation = request.generation,
                    call_id = request.call_id,
                    module_owner,
                    operation = %request.operation,
                    error = %call_error,
                    "authorized sandbox Video capability call failed"
                );
                return error(
                    "CAPABILITY_VIDEO_CALL_FAILED",
                    "trusted Video Manager call failed",
                );
            }
        };
        let payload = match serde_json::to_vec(&value) {
            Ok(payload) => payload,
            Err(_) => {
                return error(
                    "CAPABILITY_VIDEO_RESPONSE_INVALID",
                    "trusted Video Manager returned an unserializable response",
                )
            }
        };
        let response_limit = request
            .max_response_bytes
            .min(MAX_CAPABILITY_PAYLOAD_BYTES as u64) as usize;
        if payload.len() > response_limit {
            return error(
                "CAPABILITY_RESPONSE_TOO_LARGE",
                "trusted Video Manager response exceeded the capability grant",
            );
        }
        return HostCapabilityResponse::Success {
            execution_id: request.execution_id,
            call_id: request.call_id,
            payload,
        };
    }
    if request.kind != CapabilityKind::Service {
        return error(
            "CAPABILITY_KIND_UNSUPPORTED",
            "this trusted host adapter does not support that capability kind",
        );
    }
    let Some(service_name) = normalize_service_target(&request.target) else {''',
    "trusted Video host dispatch",
)
replace_once(
    host,
    '''fn normalize_service_target(target: &str) -> Option<&str> {''',
    '''fn normalize_video_target(target: &str) -> Option<&str> {
    let owner = target.strip_prefix(VIDEO_CAPABILITY_TARGET_PREFIX)?;
    valid_logical_name(owner).then_some(owner)
}

fn normalize_service_target(target: &str) -> Option<&str> {''',
    "Video module principal parser",
)
replace_once(
    host,
    '''        let services = Arc::new(RwLock::new(None));
        let request = HostCapabilityRequest {''',
    '''        let services = Arc::new(RwLock::new(None));
        let video = Arc::new(RwLock::new(VideoLanguage::new(None)));
        let request = HostCapabilityRequest {''',
    "invalid provenance test Video state",
)
replace_once(
    host,
    '''        let response = dispatch_request(request, "secret", &services).await;''',
    '''        let response = dispatch_request(request, "secret", &services, &video).await;''',
    "invalid provenance test dispatch signature",
)
replace_once(
    host,
    '''    #[test]
    fn service_target_normalization_is_logical_only() {''',
    '''    #[test]
    fn video_target_requires_exact_module_principal() {
        assert_eq!(
            normalize_video_target("module:learning.catalog"),
            Some("learning.catalog")
        );
        assert_eq!(normalize_video_target("learning.catalog"), None);
        assert_eq!(normalize_video_target("module:../catalog"), None);
        assert_eq!(normalize_video_target("module:learning/catalog"), None);
    }

    #[tokio::test]
    async fn video_dispatch_uses_authorized_target_as_owner_and_not_payload_identity() {
        let services = Arc::new(RwLock::new(None));
        let video = Arc::new(RwLock::new(VideoLanguage::new(None)));
        let request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: "secret".into(),
            execution_id: "exec-video-1".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:physical:api/catalog".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-1".into(),
            generation: 3,
            call_id: 9,
            kind: CapabilityKind::Video,
            target: "module:learning.catalog".into(),
            operation: "status".into(),
            payload: b"[]".to_vec(),
            max_response_bytes: 4096,
        };
        let response = dispatch_request(request, "secret", &services, &video).await;
        let HostCapabilityResponse::Error { code, .. } = response else {
            panic!("disabled Video Manager should fail inside the Video adapter");
        };
        assert_eq!(code, "CAPABILITY_VIDEO_CALL_FAILED");
    }

    #[test]
    fn service_target_normalization_is_logical_only() {''',
    "Video host adapter tests",
)

main = Path("engine/crates/backend/src/main.rs")
replace_once(
    main,
    '''    let app_state = AppState::new(
        config.clone(),''',
    '''    host_capability_bridge
        .install_video_manager(video_manager.clone())
        .await;
    tracing::info!(
        enabled = video_manager.is_some(),
        "trusted host capability bridge attached to Video Manager"
    );

    let app_state = AppState::new(
        config.clone(),''',
    "attach Video Manager to trusted host bridge",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''RELC `PublicHttp` requirements lower to one exact `Network/public-http` Controller grant with an explicit operation set. Video requirements now retain the canonical declaring Module principal (for example `learning.catalog`) through dependency propagation, but remain fail-closed until Video grant/host lowering is implemented; Service requirements likewise remain fail-closed until their native lowering exists.''',
    '''RELC `PublicHttp` requirements lower to one exact `Network/public-http` Controller grant with an explicit operation set. Video requirements retain the canonical declaring Module principal (for example `learning.catalog`) through dependency propagation and lower to exact `Video/module:<owner>` Controller grants with explicit operation sets. The authenticated Backend host bridge accepts only those module-principal targets, derives `VideoLanguage` ownership from the authorized target rather than guest payload, and keeps Video Manager internals behind the trusted facade. Service requirements remain fail-closed until their native lowering exists. Route-WASM v3 still has no linked-module/Video call producer, so this grant+adapter boundary is ready before end-to-end native Video lowering is enabled.''',
    "Video grant and host adapter docs",
)
