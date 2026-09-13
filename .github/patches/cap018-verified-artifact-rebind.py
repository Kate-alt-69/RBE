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
    "pub const PROTOCOL_VERSION: u16 = 7;",
    "pub const PROTOCOL_VERSION: u16 = 8;",
    "Controller protocol v8",
)
replace_once(
    protocol,
    '''#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]''',
    '''#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}

/// Bind an artifact that Controller has already SHA-256 verified and retained.
/// No executable bytes cross IPC on this fast path; a cache miss must fall back
/// to RegisterArtifact so Backend can never assert cache presence by itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub artifact_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]''',
    "BindArtifact request",
)
replace_once(
    protocol,
    '''pub enum Request {
    Hello(Hello),
    RegisterArtifact(RegisterArtifactRequest),
    RegisterCapabilityManifest(RegisterCapabilityManifestRequest),''',
    '''pub enum Request {
    Hello(Hello),
    BindArtifact(BindArtifactRequest),
    RegisterArtifact(RegisterArtifactRequest),
    RegisterCapabilityManifest(RegisterCapabilityManifestRequest),''',
    "BindArtifact request variant",
)
replace_once(
    protocol,
    '''    ArtifactRegistered {
        request_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        artifact_hash: String,
        already_present: bool,
        already_bound: bool,
    },
    CapabilityManifestRegistered {''',
    '''    ArtifactRegistered {
        request_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        artifact_hash: String,
        already_present: bool,
        already_bound: bool,
    },
    ArtifactBound {
        request_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        artifact_hash: String,
        already_bound: bool,
    },
    CapabilityManifestRegistered {''',
    "ArtifactBound response variant",
)
replace_once(
    protocol,
    '''    #[test]
    fn capability_manifest_round_trip_preserves_exact_grants() {''',
    '''    #[test]
    fn cached_artifact_binding_round_trip_carries_identity_without_wasm() {
        let request = Request::BindArtifact(BindArtifactRequest {
            request_id: "bind-1".into(),
            auth_token: "secret".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            artifact_hash: "cd".repeat(32),
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::BindArtifact(decoded) = decoded else {
            panic!("expected cached artifact binding request");
        };
        assert_eq!(decoded.runtime_image, "ab".repeat(32));
        assert_eq!(decoded.source_id, "route:api/me");
        assert_eq!(decoded.capability_abi, CAPABILITY_ABI_VERSION);
        assert_eq!(decoded.artifact_hash, "cd".repeat(32));
    }

    #[test]
    fn capability_manifest_round_trip_preserves_exact_grants() {''',
    "BindArtifact protocol test",
)

cache = Path("container-runtime/crates/container-runtime-core/src/cache.rs")
replace_once(
    cache,
    '''    pub fn contains_artifact(&self, artifact_hash: &str) -> bool {
        if artifact_hash.len() != 64''',
    '''    /// Fast path for an artifact already verified during this Controller
    /// process. On a cold process this falls through to the durable cache and
    /// verifies SHA-256 before admitting the hash into memory. Workers still
    /// independently verify the bytes they execute.
    pub fn verified_artifact_available(&self, artifact_hash: &str) -> bool {
        if self
            .artifacts
            .lock()
            .expect("artifact cache poisoned")
            .contains_key(artifact_hash)
        {
            return true;
        }
        self.contains_artifact(artifact_hash)
    }

    pub fn contains_artifact(&self, artifact_hash: &str) -> bool {
        if artifact_hash.len() != 64''',
    "verified artifact memory fast path",
)

runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
replace_once(
    runtime,
    '''                let output = if cache.contains_artifact(&task.artifact_hash) {''',
    '''                let output = if cache.verified_artifact_available(&task.artifact_hash) {''',
    "runtime verified artifact fast path",
)

main = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    main,
    '''        Request::RegisterArtifact(request) => {
            if request.auth_token != token {''',
    '''        Request::BindArtifact(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else if !runtime
                .cache()
                .verified_artifact_available(&request.artifact_hash)
            {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_NOT_FOUND".into(),
                    message: "verified artifact is not present in the Controller cache".into(),
                }
            } else {
                match capability_broker.register_artifact_binding(
                    &request.runtime_image,
                    &request.source_id,
                    request.capability_abi,
                    &request.artifact_hash,
                ) {
                    Ok(already_bound) => Response::ArtifactBound {
                        request_id: request.request_id,
                        runtime_image: request.runtime_image,
                        source_id: request.source_id,
                        capability_abi: request.capability_abi,
                        artifact_hash: request.artifact_hash,
                        already_bound,
                    },
                    Err(error) => Response::Error {
                        request_id: Some(request.request_id),
                        code: error.code.into(),
                        message: error.message,
                    },
                }
            }
        }
        Request::RegisterArtifact(request) => {
            if request.auth_token != token {''',
    "Controller BindArtifact handler",
)
replace_once(
    main,
    '''            } else if !runtime.cache().contains_artifact(&request.artifact_hash) {''',
    '''            } else if !runtime
                .cache()
                .verified_artifact_available(&request.artifact_hash)
            {''',
    "Execute verified artifact presence fast path",
)

client = Path("engine/crates/core/src/container_client.rs")
replace_once(
    client,
    '''use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, CancelRequest, CapabilityGrant,
    ExecuteRequest, HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
    RegisterCapabilityManifestRequest, Request, Response, ResumeRequest, WorkCost as IpcWorkCost,''',
    '''use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, BindArtifactRequest, CancelRequest,
    CapabilityGrant, ExecuteRequest, HealthRequest, InspectRequest, PrepareRefreshRequest,
    RegisterArtifactRequest, RegisterCapabilityManifestRequest, Request, Response, ResumeRequest,
    WorkCost as IpcWorkCost,''',
    "Container client BindArtifact import",
)
replace_once(
    client,
    '''    pub async fn register_artifact(
        &self,
        identity: ContainerExecutionIdentity<'_>,''',
    '''    /// Ask Controller to bind an already verified cached artifact to this
    /// exact Runtime Image source. `false` means Controller does not have the
    /// artifact and the caller must send bytes through RegisterArtifact.
    pub async fn bind_cached_artifact(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
    ) -> anyhow::Result<bool> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::BindArtifact(BindArtifactRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            artifact_hash: artifact_hash.to_string(),
        });
        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::ArtifactBound {
                runtime_image,
                source_id,
                capability_abi,
                artifact_hash: returned_hash,
                ..
            } if runtime_image == identity.runtime_image
                && source_id == identity.source_id
                && capability_abi == CAPABILITY_ABI_VERSION
                && returned_hash == artifact_hash =>
            {
                Ok(true)
            }
            Response::ArtifactBound { .. } => {
                anyhow::bail!("Container returned a mismatched cached artifact binding")
            }
            Response::Error { code, .. } if code == "ARTIFACT_NOT_FOUND" => Ok(false),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container cached artifact binding failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected cached artifact binding response: {other:?}"),
        }
    }

    pub async fn register_artifact(
        &self,
        identity: ContainerExecutionIdentity<'_>,''',
    "Container client cached artifact bind method",
)
replace_once(
    client,
    '''    /// Admit and execute one immutable artifact under one exact capability
    /// identity. Registration is deliberately repeated/idempotent so an
    /// Environment generation restart cannot leave a stale backend-side cache
    /// authorizing work that Controller has already invalidated.
    pub async fn execute_authorized(
        &self,
        request: ContainerAuthorizedExecution<'_>,
    ) -> anyhow::Result<Vec<u8>> {
        self.register_artifact(request.identity, request.artifact_hash, request.wasm)
            .await?;''',
    '''    /// Admit and execute one immutable artifact under one exact capability
    /// identity. Controller is queried on every call so Backend never keeps a
    /// stale authority cache, but verified WASM bytes are only transferred when
    /// Controller reports a cache miss.
    pub async fn execute_authorized(
        &self,
        request: ContainerAuthorizedExecution<'_>,
    ) -> anyhow::Result<Vec<u8>> {
        if !self
            .bind_cached_artifact(request.identity, request.artifact_hash)
            .await?
        {
            self.register_artifact(request.identity, request.artifact_hash, request.wasm)
                .await?;
        }''',
    "execute_authorized cached artifact fast path",
)
replace_once(
    client,
    '''    #[tokio::test]
    async fn execute_and_wait_cancels_execution_when_synchronous_deadline_expires() {''',
    '''    #[tokio::test]
    async fn cached_artifact_binding_uses_identity_only_fast_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server_token = token.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let frame = read_frame(&mut stream).unwrap();
            let request: Request = serde_json::from_slice(&frame).unwrap();
            let Request::BindArtifact(request) = request else {
                panic!("cached bind must not resend RegisterArtifact WASM bytes");
            };
            assert_eq!(request.auth_token, server_token);
            assert_eq!(request.runtime_image, "ab".repeat(32));
            assert_eq!(request.source_id, "route:api/test");
            assert_eq!(request.artifact_hash, "cd".repeat(32));
            respond(
                &mut stream,
                &Response::ArtifactBound {
                    request_id: request.request_id,
                    runtime_image: request.runtime_image,
                    source_id: request.source_id,
                    capability_abi: request.capability_abi,
                    artifact_hash: request.artifact_hash,
                    already_bound: true,
                },
            );
        });

        let client = ContainerClient::new(address, token, None);
        assert!(client
            .bind_cached_artifact(
                ContainerExecutionIdentity {
                    runtime_image: &"ab".repeat(32),
                    source_id: "route:api/test",
                    environment: "general",
                },
                &"cd".repeat(32),
            )
            .await
            .unwrap());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cached_artifact_binding_reports_controller_miss_without_faking_authority() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "test-container-token".to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let frame = read_frame(&mut stream).unwrap();
            let request: Request = serde_json::from_slice(&frame).unwrap();
            let Request::BindArtifact(request) = request else {
                panic!("expected cached artifact binding request");
            };
            respond(
                &mut stream,
                &Response::Error {
                    request_id: Some(request.request_id),
                    code: "ARTIFACT_NOT_FOUND".into(),
                    message: "cold Controller cache".into(),
                },
            );
        });

        let client = ContainerClient::new(address, token, None);
        assert!(!client
            .bind_cached_artifact(
                ContainerExecutionIdentity {
                    runtime_image: &"ab".repeat(32),
                    source_id: "route:api/test",
                    environment: "general",
                },
                &"cd".repeat(32),
            )
            .await
            .unwrap());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn execute_and_wait_cancels_execution_when_synchronous_deadline_expires() {''',
    "cached artifact client tests",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission registers the immutable artifact and an exact capability manifest before execution. Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set.''',
    '''The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission binds the immutable artifact and an exact capability manifest before execution. Controller protocol v8 supports a hash-only rebind for artifacts already SHA-256 verified in its cache, so repeated native calls do not retransmit WASM bytes; a cold/missing cache fails that fast path and requires full artifact registration. Backend never treats its own cache state as authority. Capability-free native routes register an empty manifest; an ABI-v3 directly imported public HTTP operation registers only its compiler-lowered `Network/public-http` grant and exact operation set.''',
    "Runtime Image artifact rebind docs",
)
