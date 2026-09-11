from pathlib import Path
import re


def rep(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    found = text.count(old)
    if found != count:
        raise SystemExit(f"{path}: expected {count} anchors, found {found}: {old[:140]!r}")
    p.write_text(text.replace(old, new, count), encoding="utf-8")


def regex_rep(path: str, pattern: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    text, replaced = re.subn(pattern, new, text, count=count, flags=re.S)
    if replaced != count:
        raise SystemExit(f"{path}: expected {count} regex replacements, got {replaced}: {pattern[:140]!r}")
    p.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# IPC v6: caller supplies immutable Runtime Image / SourceId / CAP-ABI identity,
# but never an Environment generation. Generation is Controller-owned state.
# ---------------------------------------------------------------------------
ipc = "container-runtime/crates/ipc-protocol/src/lib.rs"
rep(ipc, "pub const PROTOCOL_VERSION: u16 = 5;", "pub const PROTOCOL_VERSION: u16 = 6;")
rep(
    ipc,
    '''pub struct RegisterCapabilityManifestRequest {
    pub request_id: String,
    pub auth_token: String,
    pub capability_abi: u16,
    pub runtime_image: String,
    pub source_id: String,
    pub environment: String,
    pub generation: u64,
    pub grants: Vec<CapabilityGrant>,
}''',
    '''pub struct RegisterCapabilityManifestRequest {
    pub request_id: String,
    pub auth_token: String,
    pub capability_abi: u16,
    pub runtime_image: String,
    pub source_id: String,
    pub environment: String,
    /// The caller deliberately does not provide an Environment generation.
    /// Container Controller binds the manifest to its current live generation.
    pub grants: Vec<CapabilityGrant>,
}''',
)
rep(
    ipc,
    '''pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    /// Invocation data for an already-registered artifact. This field must
    /// never be interpreted as executable bytes.
    pub input: Vec<u8>,
}''',
    '''pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    /// Identity of the immutable Runtime Image that selected this execution.
    pub runtime_image: String,
    /// Exact REL source identity inside the Runtime Image.
    pub source_id: String,
    /// Capability ABI expected by this caller. Environment generation is
    /// intentionally absent: only Container Controller may stamp generation.
    pub capability_abi: u16,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    /// Invocation data for an already-registered artifact. This field must
    /// never be interpreted as executable bytes.
    pub input: Vec<u8>,
}''',
)
rep(
    ipc,
    '''        let execute = Request::Execute(ExecuteRequest {
            request_id: "exec-1".into(),
            auth_token: "secret".into(),
            environment: "general-1".into(),''',
    '''        let execute = Request::Execute(ExecuteRequest {
            request_id: "exec-1".into(),
            auth_token: "secret".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-1".into(),''',
)
rep(
    ipc,
    '''            source_id: "route:api/me".into(),
            environment: "general-1".into(),
            generation: 7,
            grants: vec![CapabilityGrant {''',
    '''            source_id: "route:api/me".into(),
            environment: "general-1".into(),
            grants: vec![CapabilityGrant {''',
)


# ---------------------------------------------------------------------------
# Capability Broker: registration generation comes from Controller, and an
# execution must have an exact pre-registered manifest (including empty grants).
# ---------------------------------------------------------------------------
broker = "container-runtime/crates/container-runtime-core/src/control_plane.rs"
rep(
    broker,
    '''    pub fn register_manifest(
        &self,
        request: &RegisterCapabilityManifestRequest,
    ) -> Result<usize, CapabilityError> {''',
    '''    pub fn register_manifest(
        &self,
        request: &RegisterCapabilityManifestRequest,
        generation: u64,
    ) -> Result<usize, CapabilityError> {''',
)
rep(broker, "            generation: request.generation,", "            generation,", count=1)
rep(
    broker,
    '''    pub fn authorize(
        &self,
        call: CapabilityCall<'_>,
    ) -> Result<AuthorizedCapability, CapabilityError> {''',
    '''    /// Verify that an execution identity is explicitly bound to a manifest
    /// for the Controller's current Environment generation. Empty manifests are
    /// valid and intentionally distinguish "no host capabilities" from
    /// "identity was never registered".
    pub fn authorize_execution(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        generation: u64,
        capability_abi: u16,
    ) -> Result<(), CapabilityError> {
        validate_runtime_image(runtime_image)?;
        validate_source_id(source_id)?;
        validate_environment(environment)?;
        if capability_abi != CAPABILITY_ABI_VERSION {
            return Err(CapabilityError {
                code: "CAPABILITY_ABI_UNSUPPORTED",
                message: format!(
                    "capability ABI {capability_abi} is unsupported; controller supports {CAPABILITY_ABI_VERSION}"
                ),
            });
        }
        let key = ManifestKey {
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            environment: environment.to_string(),
            generation,
        };
        if self
            .manifests
            .read()
            .expect("capability manifest table poisoned")
            .contains_key(&key)
        {
            Ok(())
        } else {
            Err(CapabilityError {
                code: "CAPABILITY_MANIFEST_UNKNOWN",
                message: "execution has no exact capability manifest for this Runtime Image/SourceId/Environment generation".into(),
            })
        }
    }

    pub fn authorize(
        &self,
        call: CapabilityCall<'_>,
    ) -> Result<AuthorizedCapability, CapabilityError> {''',
)
rep(
    broker,
    '''            environment: "general-1".into(),
            generation: 4,
            grants,''',
    '''            environment: "general-1".into(),
            grants,''',
)
# Three tests use the same service grant registration; one uses a variable grant.
p = Path(broker)
text = p.read_text(encoding="utf-8")
service_anchor = ".register_manifest(&request(vec![service_grant()]))"
service_count = text.count(service_anchor)
if service_count != 3:
    raise SystemExit(f"{broker}: expected 3 service manifest registrations, found {service_count}")
text = text.replace(service_anchor, ".register_manifest(&request(vec![service_grant()]), 4)")
grant_anchor = ".register_manifest(&request(vec![grant]))"
if text.count(grant_anchor) != 1:
    raise SystemExit(f"{broker}: expected one variable manifest registration")
text = text.replace(grant_anchor, ".register_manifest(&request(vec![grant]), 4)")
p.write_text(text, encoding="utf-8")
rep(
    broker,
    '''    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {''',
    '''    #[test]
    fn execution_binding_requires_exact_controller_generation_and_abi() {
        let broker = CapabilityBroker::new(false);
        broker
            .register_manifest(&request(Vec::new()), 4)
            .unwrap();
        broker
            .authorize_execution(
                &"ab".repeat(32),
                "route:api/me",
                "general-1",
                4,
                CAPABILITY_ABI_VERSION,
            )
            .unwrap();
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    5,
                    CAPABILITY_ABI_VERSION,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_MANIFEST_UNKNOWN"
        );
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    4,
                    CAPABILITY_ABI_VERSION + 1,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_ABI_UNSUPPORTED"
        );
    }

    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {''',
)


# ---------------------------------------------------------------------------
# Backend Container client: expose explicit manifest registration and require
# immutable caller identity on execution. No method accepts generation.
# ---------------------------------------------------------------------------
client = "engine/crates/core/src/container_client.rs"
rep(
    client,
    '''use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, ExecuteRequest, HealthRequest,
    InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest, Request, Response,
    ResumeRequest, WorkCost as IpcWorkCost, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS,
    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES,
};''',
    '''use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, CapabilityGrant, ExecuteRequest,
    HealthRequest, InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest,
    RegisterCapabilityManifestRequest, Request, Response, ResumeRequest, WorkCost as IpcWorkCost,
    CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES,
    MAX_EXECUTION_OUTPUT_BYTES,
};''',
)
rep(
    client,
    '''    pub async fn execute(
        &self,
        environment: &str,
        artifact_hash: &str,''',
    '''    /// Register the exact capability set for one Runtime Image source.
    /// Container Controller supplies and returns the live Environment generation.
    pub async fn register_capability_manifest(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<u64> {
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::RegisterCapabilityManifest(RegisterCapabilityManifestRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            capability_abi: CAPABILITY_ABI_VERSION,
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            environment: environment.to_string(),
            grants,
        });
        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::CapabilityManifestRegistered { generation, .. } => Ok(generation),
            Response::Error { code, message, .. } => {
                anyhow::bail!("container capability registration failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected capability registration response: {other:?}"),
        }
    }

    pub async fn execute(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        artifact_hash: &str,''',
)
rep(
    client,
    '''        let request = Request::Execute(ExecuteRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            environment: environment.to_string(),''',
    '''        let request = Request::Execute(ExecuteRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: environment.to_string(),''',
)
rep(
    client,
    '''    pub async fn execute_and_wait(
        &self,
        environment: &str,
        artifact_hash: &str,''',
    '''    pub async fn execute_and_wait(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        artifact_hash: &str,''',
)
rep(
    client,
    '''        let execution_id = self
            .execute(environment, artifact_hash, input, declared_cost)
            .await?;''',
    '''        let execution_id = self
            .execute(
                runtime_image,
                source_id,
                environment,
                artifact_hash,
                input,
                declared_cost,
            )
            .await?;''',
)


# ---------------------------------------------------------------------------
# Container Controller: stamp generation on manifest registration and require
# an exact manifest before an Execute request can enter the scheduler.
# ---------------------------------------------------------------------------
main = "container-runtime/crates/container-bin/src/main.rs"
manifest_arm = r'''        Request::RegisterCapabilityManifest\(request\) => \{.*?\n        \}\n        Request::Execute\(request\) => \{'''
manifest_replacement = '''        Request::RegisterCapabilityManifest(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else if let Some(environment) =
                parse_environment(&request.environment).filter(|id| runtime.has_environment(*id))
            {
                let generation = runtime.environment_generation(environment);
                match capability_broker.register_manifest(&request, generation) {
                    Ok(grants) => {
                        emit_event(
                            "capability_manifest_registered",
                            &format!(
                                "runtime_image={} source_id={} environment={} generation={} grants={grants}",
                                request.runtime_image,
                                request.source_id,
                                request.environment,
                                generation
                            ),
                        );
                        Response::CapabilityManifestRegistered {
                            request_id: request.request_id,
                            runtime_image: request.runtime_image,
                            source_id: request.source_id,
                            environment: request.environment,
                            generation,
                            grants,
                        }
                    }
                    Err(error) => Response::Error {
                        request_id: Some(request.request_id),
                        code: error.code.into(),
                        message: error.message,
                    },
                }
            } else {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "INVALID_ENVIRONMENT".into(),
                    message: format!(
                        "container environment is unavailable: {}",
                        request.environment
                    ),
                }
            }
        }
        Request::Execute(request) => {'''
regex_rep(main, manifest_arm, manifest_replacement)
rep(
    main,
    '''            } else if let Some(environment) =
                parse_environment(&request.environment).filter(|id| runtime.has_environment(*id))
            {
                let cost = WorkCost {''',
    '''            } else if let Some(environment) =
                parse_environment(&request.environment).filter(|id| runtime.has_environment(*id))
            {
                // Generation is Controller state. The backend supplies immutable
                // Runtime Image/Source identity only; it cannot mint generation.
                let generation = runtime.environment_generation(environment);
                if let Err(error) = capability_broker.authorize_execution(
                    &request.runtime_image,
                    &request.source_id,
                    &request.environment,
                    generation,
                    request.capability_abi,
                ) {
                    return write_frame(
                        &mut stream,
                        &Response::Error {
                            request_id: Some(request.request_id),
                            code: error.code.into(),
                            message: error.message,
                        },
                    )
                    .map_err(Into::into);
                }
                let cost = WorkCost {''',
    count=1,
)

# There must be no caller-owned generation left in the Controller or broker.
for path in [main, broker]:
    text = Path(path).read_text(encoding="utf-8")
    if "request.generation" in text:
        raise SystemExit(f"{path}: caller-owned request.generation survived CAP-002")
