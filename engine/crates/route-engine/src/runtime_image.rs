//! Immutable linked REL Runtime Image and transactional activation slot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use core_lib::{
    video_language_operation_allowed, ContainerCapabilityGrant, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST, CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};
use service_runtime::{service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX};
use sha2::{Digest, Sha256};

use crate::ast::{ModuleFile, RouteFile, ServiceProgram};
use crate::dependency_graph::{SymbolDependencyGraph, SymbolId};
use crate::middleware_plan::MiddlewarePlan;
use crate::runtime_env::RuntimeEnv;
use crate::server_policy::ServerPolicy;
use crate::server_rel::ServerProgram;
use crate::source_registry::{RelSourceKind, SourceId};
use crate::wasm_compiler::{
    RouteWasmArtifact, ROUTE_WASM_ABI_VERSION, ROUTE_WASM_COMPILER_VERSION,
};

pub(crate) const STORAGE_CAPABILITY_TARGET_PREFIX: &str = "storage:";
pub(crate) const STORAGE_CAPABILITY_OPERATIONS: [&str; 4] = ["read", "list", "snapshot", "commit"];
pub(crate) const MAX_STORAGE_NAMESPACE_BYTES: usize = 64;

pub(crate) fn storage_capability_operation_allowed(operation: &str) -> bool {
    STORAGE_CAPABILITY_OPERATIONS.contains(&operation)
}

pub(crate) fn storage_capability_owner_allowed(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= MAX_STORAGE_NAMESPACE_BYTES
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn storage_capability_target(owner: &str) -> Option<String> {
    if !storage_capability_owner_allowed(owner) {
        return None;
    }
    let target = format!("{STORAGE_CAPABILITY_TARGET_PREFIX}{owner}");
    (target.len() <= CONTAINER_MAX_CAPABILITY_TARGET_BYTES).then_some(target)
}

/// Host-crossing operations RELC discovered for a source. These are
/// compiler requirements, not Controller grants: target policy/byte limits and
/// reachability are still lowered explicitly before native execution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeCapabilityRequirement {
    PublicHttp { operation: String },
    Storage { owner: String, operation: String },
    Video { owner: String, operation: String },
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
    let mut storage_operations = BTreeMap::<String, BTreeSet<String>>::new();
    let mut video_operations = BTreeMap::<String, BTreeSet<String>>::new();
    let mut service_operations = BTreeMap::<String, BTreeSet<String>>::new();
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
            RuntimeCapabilityRequirement::Storage { owner, operation } => {
                if !storage_capability_owner_allowed(owner) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Storage capability principal {owner:?} is not a valid Environment Storage namespace"
                        ),
                    });
                }
                if !storage_capability_operation_allowed(operation) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Storage capability operation {operation:?} is not part of the Environment Storage surface"
                        ),
                    });
                }
                storage_operations
                    .entry(owner.clone())
                    .or_default()
                    .insert(operation.clone());
            }
            RuntimeCapabilityRequirement::Video { owner, operation } => {
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
            }
            RuntimeCapabilityRequirement::Service { service, operation } => {
                if !service_capability_name_allowed(service) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Service capability target {service:?} is not a valid logical Service name"
                        ),
                    });
                }
                if !valid_service_operation(operation) {
                    return Err(RuntimeCapabilityLoweringError {
                        message: format!(
                            "Service capability operation {operation:?} is not a valid exported operation"
                        ),
                    });
                }
                service_operations
                    .entry(service.clone())
                    .or_default()
                    .insert(operation.clone());
            }
        }
    }

    let mut grants = Vec::new();
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
    for (owner, operations) in storage_operations {
        let target =
            storage_capability_target(&owner).ok_or_else(|| RuntimeCapabilityLoweringError {
                message: format!(
                    "Storage capability principal {owner:?} cannot be lowered to an exact target"
                ),
            })?;
        grants.push(ContainerCapabilityGrant {
            kind: ContainerCapabilityKind::Storage,
            target,
            operations: operations.into_iter().collect(),
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
    for (service, operations) in service_operations {
        grants.push(ContainerCapabilityGrant {
            kind: ContainerCapabilityKind::Service,
            target: format!("{SERVICE_CAPABILITY_TARGET_PREFIX}{service}"),
            operations: operations.into_iter().collect(),
            max_request_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
            max_response_bytes: CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES as u64,
        });
    }
    validate_lowered_grant_shape(&grants)?;
    Ok(grants)
}

fn validate_lowered_grant_shape(
    grants: &[ContainerCapabilityGrant],
) -> Result<(), RuntimeCapabilityLoweringError> {
    if grants.len() > CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST {
        return Err(RuntimeCapabilityLoweringError {
            message: format!(
                "lowered capability manifest contains {} grants; Controller permits at most {}",
                grants.len(),
                CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST
            ),
        });
    }
    for grant in grants {
        if grant.target.is_empty() || grant.target.len() > CONTAINER_MAX_CAPABILITY_TARGET_BYTES {
            return Err(RuntimeCapabilityLoweringError {
                message: format!(
                    "lowered capability target {:?} exceeds the Controller target ceiling of {} bytes",
                    grant.target, CONTAINER_MAX_CAPABILITY_TARGET_BYTES
                ),
            });
        }
        if grant.operations.is_empty()
            || grant.operations.len() > CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT
        {
            return Err(RuntimeCapabilityLoweringError {
                message: format!(
                    "lowered capability grant {:?} contains {} operations; Controller permits 1..={} operations",
                    grant.target,
                    grant.operations.len(),
                    CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT
                ),
            });
        }
        if let Some(operation) = grant.operations.iter().find(|operation| {
            operation.is_empty() || operation.len() > CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
        }) {
            return Err(RuntimeCapabilityLoweringError {
                message: format!(
                    "lowered capability operation {operation:?} exceeds the Controller operation ceiling of {} bytes",
                    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
                ),
            });
        }
    }
    Ok(())
}

fn valid_service_operation(operation: &str) -> bool {
    !operation.is_empty()
        && operation.len() <= CONTAINER_MAX_CAPABILITY_OPERATION_BYTES
        && operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_video_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len()
            <= CONTAINER_MAX_CAPABILITY_TARGET_BYTES
                .saturating_sub(VIDEO_CAPABILITY_TARGET_PREFIX.len())
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[derive(Debug, Clone)]
pub struct RuntimeSourceManifest {
    pub id: SourceId,
    pub kind: RelSourceKind,
    pub logical_name: String,
    pub exports: Vec<String>,
    pub imports: Vec<String>,
    pub route_path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RuntimeExecutable {
    Route(Arc<RouteFile>),
    Module(Arc<ModuleFile>),
    Service(Arc<ServiceProgram>),
    Server(Arc<ServerProgram>),
}

#[derive(Debug, Clone)]
pub struct RuntimeImage {
    pub image_id: String,
    pub source_hash: String,
    pub server_policy: ServerPolicy,
    pub environment: RuntimeEnv,
    pub routes: Vec<SourceId>,
    pub modules: Vec<SourceId>,
    pub services: Vec<SourceId>,
    pub sources: Vec<RuntimeSourceManifest>,
    pub symbol_table: BTreeSet<SymbolId>,
    pub dependency_graph: SymbolDependencyGraph,
    pub recursive_groups: Vec<Vec<SymbolId>>,
    pub middleware_plan: MiddlewarePlan,
    pub service_assignments: BTreeMap<String, String>,
    pub capabilities: BTreeMap<SourceId, BTreeSet<RuntimeCapabilityRequirement>>,
    /// Exact native route artifacts pinned at image-link time, grouped by
    /// source and HTTP verb. These bytes are the only Route-WASM payloads
    /// eligible for Container registration.
    pub route_wasm_artifacts: BTreeMap<SourceId, BTreeMap<String, RouteWasmArtifact>>,
    /// Method-level compiler fallbacks remain explicit instead of collapsing an
    /// entire multi-method Route back to the evaluator.
    pub route_wasm_fallbacks: BTreeMap<SourceId, BTreeMap<String, String>>,
    pub executables: BTreeMap<SourceId, RuntimeExecutable>,
}

impl RuntimeImage {
    pub fn source(&self, id: &SourceId) -> Option<&RuntimeSourceManifest> {
        self.sources.iter().find(|source| &source.id == id)
    }

    pub fn contains_source(&self, id: &SourceId) -> bool {
        self.source(id).is_some()
    }

    pub fn executable(&self, id: &SourceId) -> Option<&RuntimeExecutable> {
        self.executables.get(id)
    }

    pub fn capability_requirements(
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

    pub fn route_wasm_artifact(&self, id: &SourceId, verb: &str) -> Option<&RouteWasmArtifact> {
        self.route_wasm_artifacts.get(id)?.get(verb)
    }

    pub fn route_wasm_fallback(&self, id: &SourceId, verb: &str) -> Option<&str> {
        self.route_wasm_fallbacks
            .get(id)?
            .get(verb)
            .map(String::as_str)
    }

    pub fn route_file(&self, id: &SourceId) -> Option<Arc<RouteFile>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Route(file)) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn module_file(&self, id: &SourceId) -> Option<Arc<ModuleFile>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Module(file)) => Some(file.clone()),
            _ => None,
        }
    }

    pub fn service_program(&self, id: &SourceId) -> Option<Arc<ServiceProgram>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Service(program)) => Some(program.clone()),
            _ => None,
        }
    }

    pub fn server_program(&self, id: &SourceId) -> Option<Arc<ServerProgram>> {
        match self.executable(id) {
            Some(RuntimeExecutable::Server(program)) => Some(program.clone()),
            _ => None,
        }
    }
}

/// Holds the currently active immutable image. Readers clone the Arc and are
/// never affected by a later activation. A failed compilation never reaches
/// this slot, which gives reloads the A-running/B-compiling transactional model.
pub struct RuntimeImageSlot {
    current: RwLock<Arc<RuntimeImage>>,
}

impl RuntimeImageSlot {
    pub fn new(initial: RuntimeImage) -> Self {
        Self {
            current: RwLock::new(Arc::new(initial)),
        }
    }

    pub fn snapshot(&self) -> Arc<RuntimeImage> {
        self.current
            .read()
            .expect("Runtime Image slot poisoned")
            .clone()
    }

    pub fn activate(&self, next: RuntimeImage) -> Arc<RuntimeImage> {
        let next = Arc::new(next);
        let mut current = self.current.write().expect("Runtime Image slot poisoned");
        std::mem::replace(&mut *current, next)
    }
}

pub(crate) fn stable_source_hash<'a>(
    sources: impl Iterator<Item = (&'a SourceId, &'a str)>,
) -> String {
    // Runtime Image authority is security-sensitive. Canonicalize the source
    // set and bind the complete SourceId + source bytes with SHA-256 instead of
    // the former 64-bit non-cryptographic FNV identity.
    let mut sources = sources.collect::<Vec<_>>();
    sources.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_SOURCE_SET_V1");
    feed_hash(&mut hash, &(sources.len() as u64).to_be_bytes());
    for (id, source) in sources {
        feed_hash(&mut hash, id.as_str().as_bytes());
        feed_hash(&mut hash, source.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(crate) fn stable_image_hash(source_hash: &str, settings: &serde_json::Value) -> String {
    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V3");
    feed_hash(&mut hash, source_hash.as_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_ABI_VERSION.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_COMPILER_VERSION.to_be_bytes());
    hash_json(&mut hash, settings);
    hex::encode(hash.finalize())
}

fn feed_hash(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn hash_json(hash: &mut Sha256, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => feed_hash(hash, b"N"),
        serde_json::Value::Bool(value) => feed_hash(hash, if *value { b"T" } else { b"F" }),
        serde_json::Value::Number(value) => {
            feed_hash(hash, b"D");
            feed_hash(hash, value.to_string().as_bytes());
            feed_hash(hash, &[0]);
        }
        serde_json::Value::String(value) => {
            feed_hash(hash, b"S");
            feed_hash(hash, &(value.len() as u64).to_be_bytes());
            feed_hash(hash, value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            feed_hash(hash, b"A");
            feed_hash(hash, &(values.len() as u64).to_be_bytes());
            for value in values {
                hash_json(hash, value);
            }
        }
        serde_json::Value::Object(values) => {
            feed_hash(hash, b"O");
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            feed_hash(hash, &(keys.len() as u64).to_be_bytes());
            for key in keys {
                feed_hash(hash, &(key.len() as u64).to_be_bytes());
                feed_hash(hash, key.as_bytes());
                hash_json(hash, &values[key]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::RelSourceKind;

    #[test]
    fn image_hash_is_sensitive_to_settings_and_key_order_is_stable() {
        let source_hash = "2a".repeat(32);
        let first = serde_json::json!({"runtimeEnv": {"A": 1, "B": true}});
        let reordered = serde_json::json!({"runtimeEnv": {"B": true, "A": 1}});
        let changed = serde_json::json!({"runtimeEnv": {"A": 2, "B": true}});
        let first_hash = stable_image_hash(&source_hash, &first);
        assert_eq!(first_hash, stable_image_hash(&source_hash, &reordered));
        assert_ne!(first_hash, stable_image_hash(&source_hash, &changed));
        assert_eq!(first_hash.len(), 64);
        assert!(first_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }

    #[test]
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
    fn storage_requirements_lower_to_exact_module_namespace_grant() {
        let requirements = BTreeSet::from([
            RuntimeCapabilityRequirement::Storage {
                owner: "accounts.cache".into(),
                operation: "snapshot".into(),
            },
            RuntimeCapabilityRequirement::Storage {
                owner: "accounts.cache".into(),
                operation: "read".into(),
            },
        ]);
        let grants = lower_container_grants(&requirements).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].kind, ContainerCapabilityKind::Storage);
        assert_eq!(grants[0].target, "storage:accounts.cache");
        assert_eq!(grants[0].operations, vec!["read", "snapshot"]);
    }

    #[test]
    fn invalid_storage_operation_or_principal_fails_closed() {
        for requirement in [
            RuntimeCapabilityRequirement::Storage {
                owner: "accounts.cache".into(),
                operation: "erase_everything".into(),
            },
            RuntimeCapabilityRequirement::Storage {
                owner: "../accounts".into(),
                operation: "read".into(),
            },
            RuntimeCapabilityRequirement::Storage {
                owner: "x".repeat(MAX_STORAGE_NAMESPACE_BYTES + 1),
                operation: "read".into(),
            },
        ] {
            assert!(lower_container_grants(&BTreeSet::from([requirement])).is_err());
        }
    }

    #[test]
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
    fn service_requirements_lower_to_exact_prefixed_grants() {
        let requirements = BTreeSet::from([
            RuntimeCapabilityRequirement::Service {
                service: "uac-cache".into(),
                operation: "get_user".into(),
            },
            RuntimeCapabilityRequirement::Service {
                service: "uac-cache".into(),
                operation: "has_user".into(),
            },
            RuntimeCapabilityRequirement::Service {
                service: "mailer".into(),
                operation: "send".into(),
            },
        ]);
        let grants = lower_container_grants(&requirements).unwrap();
        let service_grants = grants
            .iter()
            .filter(|grant| grant.kind == ContainerCapabilityKind::Service)
            .collect::<Vec<_>>();
        assert_eq!(service_grants.len(), 2);
        assert_eq!(service_grants[0].target, "service:mailer");
        assert_eq!(service_grants[0].operations, vec!["send"]);
        assert_eq!(service_grants[1].target, "service:uac-cache");
        assert_eq!(service_grants[1].operations, vec!["get_user", "has_user"]);
    }

    #[test]
    fn invalid_service_target_or_operation_fails_closed() {
        for requirement in [
            RuntimeCapabilityRequirement::Service {
                service: "../uac".into(),
                operation: "get_user".into(),
            },
            RuntimeCapabilityRequirement::Service {
                service: "uac".into(),
                operation: "../../secret".into(),
            },
        ] {
            assert!(lower_container_grants(&BTreeSet::from([requirement])).is_err());
        }
    }

    #[test]
    fn compiler_rejects_manifest_shapes_above_shared_controller_limits() {
        let too_many_operations = (0..=CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT)
            .map(|index| RuntimeCapabilityRequirement::Service {
                service: "bulk".into(),
                operation: format!("op{index}"),
            })
            .collect::<BTreeSet<_>>();
        let error = lower_container_grants(&too_many_operations)
            .expect_err("compiler must reject a grant Controller cannot register");
        assert!(error.message.contains("operations"));

        let too_many_grants = (0..=CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST)
            .map(|index| RuntimeCapabilityRequirement::Service {
                service: format!("svc{index}"),
                operation: "call".into(),
            })
            .collect::<BTreeSet<_>>();
        let error = lower_container_grants(&too_many_grants)
            .expect_err("compiler must reject a manifest Controller cannot register");
        assert!(error.message.contains("grants"));

        let long_operation = RuntimeCapabilityRequirement::Service {
            service: "mail".into(),
            operation: "x".repeat(CONTAINER_MAX_CAPABILITY_OPERATION_BYTES + 1),
        };
        assert!(lower_container_grants(&BTreeSet::from([long_operation])).is_err());

        let long_owner = RuntimeCapabilityRequirement::Video {
            owner: "x".repeat(
                CONTAINER_MAX_CAPABILITY_TARGET_BYTES - VIDEO_CAPABILITY_TARGET_PREFIX.len() + 1,
            ),
            operation: "status".into(),
        };
        assert!(lower_container_grants(&BTreeSet::from([long_owner])).is_err());
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
    fn source_hash_is_deterministic_and_sensitive_to_content() {
        let id = SourceId::physical(RelSourceKind::Module, "A").unwrap();
        let first = stable_source_hash(std::iter::once((&id, "one")));
        let same = stable_source_hash(std::iter::once((&id, "one")));
        let other = stable_source_hash(std::iter::once((&id, "two")));
        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_eq!(first.len(), 64);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}
