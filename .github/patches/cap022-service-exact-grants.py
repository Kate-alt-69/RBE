from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


service_runtime = Path("engine/crates/service-runtime/src/lib.rs")
replace_once(
    service_runtime,
    '''pub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;''',
    '''pub(crate) const SERVICE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SERVICE_IPC_REQUEST_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SERVICE_IPC_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Container-visible Service targets are logical catalog names only. The
/// prefix makes them unambiguous from every other capability kind/namespace.
pub const SERVICE_CAPABILITY_TARGET_PREFIX: &str = "service:";

/// Keep the complete logical target within Container Controller's 256-byte
/// capability-target ceiling and reject path/socket-like names.
pub fn service_capability_name_allowed(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 248
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}''',
    "shared Service capability target contract",
)

runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''use sha2::{Digest, Sha256};''',
    '''use service_runtime::{
    service_capability_name_allowed, SERVICE_CAPABILITY_TARGET_PREFIX,
};
use sha2::{Digest, Sha256};''',
    "Runtime Image Service capability contract import",
)
replace_once(
    runtime_image,
    '''    let mut public_http_operations = BTreeSet::new();
    let mut video_operations = BTreeMap::<String, BTreeSet<String>>::new();''',
    '''    let mut public_http_operations = BTreeSet::new();
    let mut video_operations = BTreeMap::<String, BTreeSet<String>>::new();
    let mut service_operations = BTreeMap::<String, BTreeSet<String>>::new();''',
    "collect Service grants",
)
replace_once(
    runtime_image,
    '''            RuntimeCapabilityRequirement::Service { service, operation } => {
                return Err(RuntimeCapabilityLoweringError {
                    message: format!(
                        "Service capability {service:?}.{operation} has no native Container grant lowering yet"
                    ),
                });
            }''',
    '''            RuntimeCapabilityRequirement::Service { service, operation } => {
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
            }''',
    "lower exact Service requirement",
)
replace_once(
    runtime_image,
    '''    for (owner, operations) in video_operations {
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

fn valid_video_owner(owner: &str) -> bool {''',
    '''    for (owner, operations) in video_operations {
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
    Ok(grants)
}

fn valid_service_operation(operation: &str) -> bool {
    !operation.is_empty()
        && operation.len() <= 128
        && operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_video_owner(owner: &str) -> bool {''',
    "emit Service exact grants",
)
replace_once(
    runtime_image,
    '''    #[test]
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
    '''    #[test]
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
    fn unknown_public_http_operation_fails_closed() {''',
    "Service exact grant tests",
)

host = Path("engine/crates/backend/src/host_capability.rs")
replace_once(
    host,
    '''use service_runtime::ServiceManager;''',
    '''use service_runtime::{
    service_capability_name_allowed, ServiceManager, SERVICE_CAPABILITY_TARGET_PREFIX,
};''',
    "Backend Service capability target contract import",
)
replace_once(
    host,
    '''fn normalize_service_target(target: &str) -> Option<&str> {
    let target = target.strip_prefix("service:").unwrap_or(target);
    valid_logical_name(target).then_some(target)
}''',
    '''fn normalize_service_target(target: &str) -> Option<&str> {
    let service = target.strip_prefix(SERVICE_CAPABILITY_TARGET_PREFIX)?;
    service_capability_name_allowed(service).then_some(service)
}''',
    "require prefixed Service target",
)
replace_once(
    host,
    '''    fn service_target_normalization_is_logical_only() {
        assert_eq!(
            normalize_service_target("service:uac-cache"),
            Some("uac-cache")
        );
        assert_eq!(normalize_service_target("mail"), Some("mail"));
        assert_eq!(normalize_service_target("../mail"), None);
        assert_eq!(normalize_service_target("service:mail/socket"), None);
    }''',
    '''    fn service_target_normalization_is_prefixed_and_logical_only() {
        assert_eq!(
            normalize_service_target("service:uac-cache"),
            Some("uac-cache")
        );
        assert_eq!(normalize_service_target("mail"), None);
        assert_eq!(normalize_service_target("../mail"), None);
        assert_eq!(normalize_service_target("service:mail/socket"), None);
    }''',
    "Service target prefix test",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Service requirements remain fail-closed until their native lowering exists. Route-WASM v3 still has no linked-module/Video call producer, so this grant+adapter boundary is ready before end-to-end native Video lowering is enabled.''',
    '''Service requirements lower to exact `Service/service:<name>` Controller grants with compiler-derived exported operation sets, and the authenticated Backend adapter rejects raw/unprefixed targets before calling `ServiceManager`. Route-WASM v3 still has no linked-module Service/Video call producer, so these grant+adapter boundaries are ready before end-to-end native linked-module host calls are enabled.''',
    "Service exact grant docs",
)
