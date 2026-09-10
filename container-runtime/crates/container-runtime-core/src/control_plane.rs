use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use ipc_protocol::{
    CapabilityGrant, CapabilityKind, RegisterCapabilityManifestRequest, CAPABILITY_ABI_VERSION,
    MAX_CAPABILITY_PAYLOAD_BYTES,
};

const MAX_GRANTS_PER_SOURCE: usize = 128;
const MAX_OPERATIONS_PER_GRANT: usize = 64;
const MAX_SOURCE_ID_BYTES: usize = 512;
const MAX_TARGET_BYTES: usize = 256;
const MAX_OPERATION_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ManifestKey {
    runtime_image: String,
    source_id: String,
    environment: String,
    generation: u64,
}

#[derive(Debug, Clone)]
struct CapabilityManifest {
    grants: Vec<CapabilityGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCall<'a> {
    pub runtime_image: &'a str,
    pub source_id: &'a str,
    pub environment: &'a str,
    pub generation: u64,
    pub kind: CapabilityKind,
    pub target: &'a str,
    pub operation: &'a str,
    pub request_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedCapability {
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CapabilityError {}

/// Exact, deny-by-default authority table for sandbox-originated host calls.
///
/// The broker deliberately knows logical targets (`uac`, `service.uac`, a
/// network destination policy name, etc.), never Service PIDs, IPC addresses,
/// Vault credentials, or host handles. A grant is bound to one immutable
/// Runtime Image + SourceId + Environment generation. Replacing any of those
/// identities requires a new manifest.
pub struct CapabilityBroker {
    debug_enabled: bool,
    manifests: RwLock<HashMap<ManifestKey, CapabilityManifest>>,
}

impl CapabilityBroker {
    pub fn new(debug_enabled: bool) -> Self {
        Self {
            debug_enabled,
            manifests: RwLock::new(HashMap::new()),
        }
    }

    pub fn debug_enabled(&self) -> bool {
        self.debug_enabled
    }

    pub fn register_manifest(
        &self,
        request: &RegisterCapabilityManifestRequest,
    ) -> Result<usize, CapabilityError> {
        validate_runtime_image(&request.runtime_image)?;
        validate_source_id(&request.source_id)?;
        validate_environment(&request.environment)?;
        if request.capability_abi != CAPABILITY_ABI_VERSION {
            return Err(CapabilityError {
                code: "CAPABILITY_ABI_UNSUPPORTED",
                message: format!(
                    "capability ABI {} is unsupported; controller supports {}",
                    request.capability_abi, CAPABILITY_ABI_VERSION
                ),
            });
        }
        if request.grants.len() > MAX_GRANTS_PER_SOURCE {
            return Err(CapabilityError {
                code: "CAPABILITY_MANIFEST_TOO_LARGE",
                message: format!(
                    "capability manifest contains {} grants; maximum is {MAX_GRANTS_PER_SOURCE}",
                    request.grants.len()
                ),
            });
        }

        let mut seen = HashSet::new();
        for grant in &request.grants {
            validate_grant(grant, self.debug_enabled)?;
            for operation in &grant.operations {
                let identity = (grant.kind, grant.target.as_str(), operation.as_str());
                if !seen.insert(identity) {
                    return Err(CapabilityError {
                        code: "CAPABILITY_DUPLICATE_GRANT",
                        message: format!(
                            "duplicate capability {:?}:{}:{}",
                            grant.kind, grant.target, operation
                        ),
                    });
                }
            }
        }

        let key = ManifestKey {
            runtime_image: request.runtime_image.clone(),
            source_id: request.source_id.clone(),
            environment: request.environment.clone(),
            generation: request.generation,
        };
        self.manifests
            .write()
            .expect("capability manifest table poisoned")
            .insert(
                key,
                CapabilityManifest {
                    grants: request.grants.clone(),
                },
            );
        Ok(request.grants.len())
    }

    pub fn authorize(
        &self,
        call: CapabilityCall<'_>,
    ) -> Result<AuthorizedCapability, CapabilityError> {
        validate_runtime_image(call.runtime_image)?;
        validate_source_id(call.source_id)?;
        validate_environment(call.environment)?;
        validate_target(call.target)?;
        validate_operation(call.operation)?;
        if matches!(call.kind, CapabilityKind::Debug | CapabilityKind::HostFile)
            && !self.debug_enabled
        {
            return Err(CapabilityError {
                code: "DEBUG_DISABLED",
                message: "debug-only capability is disabled by the Container Controller".into(),
            });
        }
        if call.request_bytes > MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err(CapabilityError {
                code: "CAPABILITY_REQUEST_TOO_LARGE",
                message: format!(
                    "capability request is {} bytes; global maximum is {MAX_CAPABILITY_PAYLOAD_BYTES}",
                    call.request_bytes
                ),
            });
        }

        let key = ManifestKey {
            runtime_image: call.runtime_image.to_string(),
            source_id: call.source_id.to_string(),
            environment: call.environment.to_string(),
            generation: call.generation,
        };
        let manifests = self
            .manifests
            .read()
            .expect("capability manifest table poisoned");
        let manifest = manifests.get(&key).ok_or_else(|| CapabilityError {
            code: "CAPABILITY_MANIFEST_UNKNOWN",
            message: "no exact capability manifest exists for this Runtime Image/SourceId/Environment generation".into(),
        })?;

        let grant = manifest
            .grants
            .iter()
            .find(|grant| {
                grant.kind == call.kind
                    && grant.target == call.target
                    && grant.operations.iter().any(|op| op == call.operation)
            })
            .ok_or_else(|| CapabilityError {
                code: "CAPABILITY_DENIED",
                message: format!(
                    "{:?}:{}:{} is not granted to {}",
                    call.kind, call.target, call.operation, call.source_id
                ),
            })?;

        if call.request_bytes as u64 > grant.max_request_bytes {
            return Err(CapabilityError {
                code: "CAPABILITY_REQUEST_TOO_LARGE",
                message: format!(
                    "capability request is {} bytes; grant permits {} bytes",
                    call.request_bytes, grant.max_request_bytes
                ),
            });
        }

        Ok(AuthorizedCapability {
            max_request_bytes: grant.max_request_bytes,
            max_response_bytes: grant.max_response_bytes,
        })
    }

    pub fn revoke_environment_generation(&self, environment: &str, generation: u64) -> usize {
        let mut manifests = self
            .manifests
            .write()
            .expect("capability manifest table poisoned");
        let before = manifests.len();
        manifests.retain(|key, _| {
            !(key.environment == environment && key.generation == generation)
        });
        before.saturating_sub(manifests.len())
    }

    pub fn manifest_count(&self) -> usize {
        self.manifests
            .read()
            .expect("capability manifest table poisoned")
            .len()
    }
}

fn validate_grant(grant: &CapabilityGrant, debug_enabled: bool) -> Result<(), CapabilityError> {
    validate_target(&grant.target)?;
    if grant.operations.is_empty() || grant.operations.len() > MAX_OPERATIONS_PER_GRANT {
        return Err(CapabilityError {
            code: "CAPABILITY_GRANT_INVALID",
            message: format!(
                "capability grant must contain 1..={MAX_OPERATIONS_PER_GRANT} operations"
            ),
        });
    }
    for operation in &grant.operations {
        validate_operation(operation)?;
    }
    if grant.max_request_bytes == 0
        || grant.max_response_bytes == 0
        || grant.max_request_bytes > MAX_CAPABILITY_PAYLOAD_BYTES as u64
        || grant.max_response_bytes > MAX_CAPABILITY_PAYLOAD_BYTES as u64
    {
        return Err(CapabilityError {
            code: "CAPABILITY_GRANT_INVALID",
            message: format!(
                "capability grant byte limits must be 1..={MAX_CAPABILITY_PAYLOAD_BYTES}"
            ),
        });
    }
    if matches!(grant.kind, CapabilityKind::Debug | CapabilityKind::HostFile) && !debug_enabled {
        return Err(CapabilityError {
            code: "DEBUG_DISABLED",
            message: "debug/host-file grants cannot be registered in a production Controller"
                .into(),
        });
    }
    Ok(())
}

fn validate_runtime_image(value: &str) -> Result<(), CapabilityError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_identity(
            "runtime_image",
            "must be a lowercase 64-character SHA-256",
        ));
    }
    Ok(())
}

fn validate_source_id(value: &str) -> Result<(), CapabilityError> {
    if value.is_empty()
        || value.len() > MAX_SOURCE_ID_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(invalid_identity("source_id", "is empty, too long, or invalid"));
    }
    Ok(())
}

fn validate_environment(value: &str) -> Result<(), CapabilityError> {
    if !matches!(
        value,
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5" | "payment"
    ) {
        return Err(invalid_identity("environment", "is not a configured RBE Environment"));
    }
    Ok(())
}

fn validate_target(value: &str) -> Result<(), CapabilityError> {
    if value.is_empty()
        || value.len() > MAX_TARGET_BYTES
        || value.contains('*')
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(CapabilityError {
            code: "CAPABILITY_GRANT_INVALID",
            message: "capability target is empty, too long, contains a wildcard, or is invalid"
                .into(),
        });
    }
    Ok(())
}

fn validate_operation(value: &str) -> Result<(), CapabilityError> {
    if value.is_empty()
        || value.len() > MAX_OPERATION_BYTES
        || value.contains('*')
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
    {
        return Err(CapabilityError {
            code: "CAPABILITY_GRANT_INVALID",
            message: "capability operation is empty, too long, wildcarded, or contains unsupported characters".into(),
        });
    }
    Ok(())
}

fn invalid_identity(field: &str, reason: &str) -> CapabilityError {
    CapabilityError {
        code: "CAPABILITY_IDENTITY_INVALID",
        message: format!("{field} {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(grants: Vec<CapabilityGrant>) -> RegisterCapabilityManifestRequest {
        RegisterCapabilityManifestRequest {
            request_id: "req-1".into(),
            auth_token: "ignored-by-broker".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            environment: "general-1".into(),
            generation: 4,
            grants,
        }
    }

    fn service_grant() -> CapabilityGrant {
        CapabilityGrant {
            kind: CapabilityKind::Service,
            target: "uac".into(),
            operations: vec!["get_user".into()],
            max_request_bytes: 1024,
            max_response_bytes: 4096,
        }
    }

    fn call<'a>(generation: u64, operation: &'a str, bytes: usize) -> CapabilityCall<'a> {
        CapabilityCall {
            runtime_image: "abababababababababababababababababababababababababababababababab",
            source_id: "route:api/me",
            environment: "general-1",
            generation,
            kind: CapabilityKind::Service,
            target: "uac",
            operation,
            request_bytes: bytes,
        }
    }

    #[test]
    fn denies_when_no_exact_manifest_exists() {
        let broker = CapabilityBroker::new(false);
        let error = broker.authorize(call(4, "get_user", 20)).unwrap_err();
        assert_eq!(error.code, "CAPABILITY_MANIFEST_UNKNOWN");
    }

    #[test]
    fn exact_grant_authorizes_and_enforces_request_bound() {
        let broker = CapabilityBroker::new(false);
        broker.register_manifest(&request(vec![service_grant()])).unwrap();
        let authorized = broker.authorize(call(4, "get_user", 1000)).unwrap();
        assert_eq!(authorized.max_response_bytes, 4096);
        let error = broker.authorize(call(4, "get_user", 1025)).unwrap_err();
        assert_eq!(error.code, "CAPABILITY_REQUEST_TOO_LARGE");
    }

    #[test]
    fn wrong_operation_and_generation_fail_closed() {
        let broker = CapabilityBroker::new(false);
        broker.register_manifest(&request(vec![service_grant()])).unwrap();
        assert_eq!(
            broker.authorize(call(4, "delete_user", 1)).unwrap_err().code,
            "CAPABILITY_DENIED"
        );
        assert_eq!(
            broker.authorize(call(5, "get_user", 1)).unwrap_err().code,
            "CAPABILITY_MANIFEST_UNKNOWN"
        );
    }

    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {
        let broker = CapabilityBroker::new(false);
        for kind in [CapabilityKind::Debug, CapabilityKind::HostFile] {
            let mut grant = service_grant();
            grant.kind = kind;
            grant.target = "shell".into();
            assert_eq!(
                broker.register_manifest(&request(vec![grant])).unwrap_err().code,
                "DEBUG_DISABLED"
            );
        }
    }

    #[test]
    fn revoke_is_bound_to_environment_generation() {
        let broker = CapabilityBroker::new(false);
        broker.register_manifest(&request(vec![service_grant()])).unwrap();
        assert_eq!(broker.revoke_environment_generation("general-1", 4), 1);
        assert_eq!(broker.manifest_count(), 0);
        assert_eq!(
            broker.authorize(call(4, "get_user", 1)).unwrap_err().code,
            "CAPABILITY_MANIFEST_UNKNOWN"
        );
    }
}
