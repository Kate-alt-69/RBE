from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


core = Path("engine/crates/core/src/lib.rs")
replace_once(
    core,
    '''pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost,
    MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    '''pub use ipc_protocol::{
    CapabilityGrant as ContainerCapabilityGrant, CapabilityKind as ContainerCapabilityKind,
    WorkCost as ContainerWorkCost,
    MAX_CAPABILITY_PAYLOAD_BYTES as CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
};''',
    "Container capability payload re-export",
)

runtime = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime,
    '''use sha2::{Digest, Sha256};

use crate::ast::{ModuleFile, RouteFile, ServiceProgram};''',
    '''use core_lib::{
    ContainerCapabilityGrant, ContainerCapabilityKind, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    PUBLIC_HTTP_TARGET,
};
use sha2::{Digest, Sha256};

use crate::ast::{ModuleFile, RouteFile, ServiceProgram};''',
    "Runtime Image grant imports",
)
replace_once(
    runtime,
    '''#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeCapabilityRequirement {
    PublicHttp { operation: String },
    Video { operation: String },
    Service { service: String, operation: String },
}

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {''',
    '''#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeCapabilityRequirement {
    PublicHttp { operation: String },
    Video { operation: String },
    Service { service: String, operation: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCapabilityLoweringError {
    pub message: String,
}

impl std::fmt::Display for RuntimeCapabilityLoweringError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeCapabilityLoweringError {}

fn lower_container_grants(
    requirements: &BTreeSet<RuntimeCapabilityRequirement>,
) -> Result<Vec<ContainerCapabilityGrant>, RuntimeCapabilityLoweringError> {
    let mut public_http_operations = BTreeSet::new();
    for requirement in requirements {
        match requirement {
            RuntimeCapabilityRequirement::PublicHttp { operation } => {
                if !matches!(operation.as_str(), "get" | "post" | "request") {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "public HTTP operation {operation:?} is not lowerable to the Container Network Broker"
                        ),
                    });
                }
                public_http_operations.insert(operation.clone());
            }
            RuntimeCapabilityRequirement::Video { operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Video capability operation {operation:?} has no native Container grant lowering yet"
                    ),
                });
            }
            RuntimeCapabilityRequirement::Service { service, operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Service capability {service:?}.{operation} has no native Container grant lowering yet"
                    ),
                });
            }
        }
    }

    if public_http_operations.is_empty() {
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
}

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {''',
    "typed Container capability lowering",
)
replace_once(
    runtime,
    '''    pub fn capability_requirements(
        &self,
        id: &SourceId,
    ) -> Option<&BTreeSet<RuntimeCapabilityRequirement>> {
        self.capabilities.get(id)
    }

    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {''',
    '''    pub fn capability_requirements(
        &self,
        id: &SourceId,
    ) -> Option<&BTreeSet<RuntimeCapabilityRequirement>> {
        self.capabilities.get(id)
    }

    /// Lower compiler-discovered host requirements into the exact logical
    /// grants Container Controller understands. Unsupported capability kinds
    /// fail closed instead of being dropped or widened.
    pub fn container_capability_grants(
        &self,
        id: &SourceId,
    ) -> Result<Vec<ContainerCapabilityGrant>, RuntimeCapabilityLoweringError> {
        match self.capability_requirements(id) {
            Some(requirements) => lower_container_grants(requirements),
            None => Ok(Vec::new()),
        }
    }

    pub fn route_wasm_artifact(&self, id: &SourceId) -> Option<&RouteWasmArtifact> {''',
    "Runtime Image grant lowering method",
)
replace_once(
    runtime,
    '''    #[test]
    fn source_hash_is_deterministic_and_sensitive_to_content() {''',
    '''    #[test]
    fn public_http_requirements_lower_to_one_exact_network_grant() {
        let requirements = BTreeSet::from([
            RuntimeCapabilityRequirement::PublicHttp {
                operation: "post".into(),
            },
            RuntimeCapabilityRequirement::PublicHttp {
                operation: "get".into(),
            },
        ]);
        let grants = lower_container_grants(&requirements).unwrap();
        assert_eq!(grants.len(), 1);
        let grant = &grants[0];
        assert_eq!(grant.kind, ContainerCapabilityKind::Network);
        assert_eq!(grant.target, PUBLIC_HTTP_TARGET);
        assert_eq!(grant.operations, vec!["get", "post"]);
        assert_eq!(
            grant.max_request_bytes,
            CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64
        );
        assert_eq!(
            grant.max_response_bytes,
            CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64
        );
    }

    #[test]
    fn unsupported_native_capability_requirements_fail_closed() {
        for requirement in [
            RuntimeCapabilityRequirement::Video {
                operation: "status".into(),
            },
            RuntimeCapabilityRequirement::Service {
                service: "uac".into(),
                operation: "get_user".into(),
            },
        ] {
            let error = lower_container_grants(&BTreeSet::from([requirement]))
                .expect_err("unsupported capability must not be silently dropped");
            assert!(error.message.contains("no native Container grant lowering yet"));
        }
    }

    #[test]
    fn unknown_public_http_operation_fails_closed() {
        let error = lower_container_grants(&BTreeSet::from([
            RuntimeCapabilityRequirement::PublicHttp {
                operation: "connect".into(),
            },
        ]))
        .expect_err("unknown Network operation must not lower");
        assert!(error.message.contains("not lowerable"));
    }

    #[test]
    fn source_hash_is_deterministic_and_sensitive_to_content() {''',
    "Runtime Image grant lowering tests",
)

discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''    if image
        .capability_requirements(&plan.source_id)
        .is_some_and(|capabilities| !capabilities.is_empty())
    {
        let error = "native Route-WASM declares host capability requirements not lowered by the native compiler";
        tracing::error!(path = %path, source = %plan.source_id, "native route capability invariant failed");
        append_runtime_error(path, error);
        return request_error(StatusCode::INTERNAL_SERVER_ERROR, error);
    }

    let identity = ContainerExecutionIdentity {''',
    '''    let grants = match image.container_capability_grants(&plan.source_id) {
        Ok(grants) => grants,
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path,
                source = %plan.source_id,
                "native route capability lowering failed closed"
            );
            append_runtime_error(path, &error.to_string());
            return request_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "native route capability requirements are not executable",
            );
        }
    };

    let identity = ContainerExecutionIdentity {''',
    "native route grant lowering",
)
replace_once(
    discovery,
    '''            wasm: plan.artifact.bytes.clone(),
            grants: Vec::new(),
            input,''',
    '''            wasm: plan.artifact.bytes.clone(),
            grants,
            input,''',
    "native route exact grants",
)

# Document the compiler-to-Controller lowering boundary.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Public HTTP execution is centralized in `core_lib` as the `public-http` Network Broker: interpreter calls and authenticated Container host calls share the same public-address DNS pinning, redirect/proxy denial, controlled-header rules, request/response ceilings, and timeouts.''',
    '''Public HTTP execution is centralized in `core_lib` as the `public-http` Network Broker: interpreter calls and authenticated Container host calls share the same public-address DNS pinning, redirect/proxy denial, controlled-header rules, request/response ceilings, and timeouts. RELC `PublicHttp` requirements lower to one exact `Network/public-http` Controller grant with an explicit operation set; Service and Video requirements remain fail-closed until their own native lowering exists.''',
    "grant lowering documentation",
)
