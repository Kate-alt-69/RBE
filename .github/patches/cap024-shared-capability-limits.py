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
    '''pub const MAX_CAPABILITY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_HOST_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 64 * 1024;
''',
    '''pub const MAX_CAPABILITY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Shared manifest-shape ceilings. These values are part of the capability
/// control-plane contract: producers must fail before registration rather than
/// constructing manifests the Controller is guaranteed to reject.
pub const MAX_CAPABILITY_GRANTS_PER_MANIFEST: usize = 128;
pub const MAX_CAPABILITY_OPERATIONS_PER_GRANT: usize = 64;
pub const MAX_CAPABILITY_TARGET_BYTES: usize = 256;
pub const MAX_CAPABILITY_OPERATION_BYTES: usize = 128;
pub const MAX_HOST_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 64 * 1024;
''',
    "shared IPC capability ceilings",
)

control = Path("container-runtime/crates/container-runtime-core/src/control_plane.rs")
replace_once(
    control,
    '''use ipc_protocol::{
    CapabilityGrant, CapabilityKind, RegisterCapabilityManifestRequest, CAPABILITY_ABI_VERSION,
    MAX_CAPABILITY_PAYLOAD_BYTES,
};

const MAX_GRANTS_PER_SOURCE: usize = 128;
const MAX_OPERATIONS_PER_GRANT: usize = 64;
const MAX_SOURCE_ID_BYTES: usize = 512;
const MAX_TARGET_BYTES: usize = 256;
const MAX_OPERATION_BYTES: usize = 128;
''',
    '''use ipc_protocol::{
    CapabilityGrant, CapabilityKind, RegisterCapabilityManifestRequest, CAPABILITY_ABI_VERSION,
    MAX_CAPABILITY_GRANTS_PER_MANIFEST, MAX_CAPABILITY_OPERATIONS_PER_GRANT,
    MAX_CAPABILITY_OPERATION_BYTES, MAX_CAPABILITY_PAYLOAD_BYTES, MAX_CAPABILITY_TARGET_BYTES,
};

const MAX_SOURCE_ID_BYTES: usize = 512;
''',
    "Controller shared capability imports",
)
text = control.read_text(encoding="utf-8")
for old, new in [
    ("MAX_GRANTS_PER_SOURCE", "MAX_CAPABILITY_GRANTS_PER_MANIFEST"),
    ("MAX_OPERATIONS_PER_GRANT", "MAX_CAPABILITY_OPERATIONS_PER_GRANT"),
    ("MAX_TARGET_BYTES", "MAX_CAPABILITY_TARGET_BYTES"),
    ("MAX_OPERATION_BYTES", "MAX_CAPABILITY_OPERATION_BYTES"),
]:
    text = text.replace(old, new)
control.write_text(text, encoding="utf-8")
replace_once(
    control,
    '''    #[test]
    fn denies_when_no_exact_manifest_exists() {''',
    '''    #[test]
    fn shared_manifest_shape_limits_are_enforced() {
        let broker = CapabilityBroker::new(false);

        let mut too_many_operations = service_grant();
        too_many_operations.operations = (0..=MAX_CAPABILITY_OPERATIONS_PER_GRANT)
            .map(|index| format!("op{index}"))
            .collect();
        assert_eq!(
            broker
                .register_manifest(&request(vec![too_many_operations]), 4)
                .unwrap_err()
                .code,
            "CAPABILITY_GRANT_INVALID"
        );

        let mut long_target = service_grant();
        long_target.target = "x".repeat(MAX_CAPABILITY_TARGET_BYTES + 1);
        assert_eq!(
            broker
                .register_manifest(&request(vec![long_target]), 4)
                .unwrap_err()
                .code,
            "CAPABILITY_GRANT_INVALID"
        );

        let mut long_operation = service_grant();
        long_operation.operations = vec!["x".repeat(MAX_CAPABILITY_OPERATION_BYTES + 1)];
        assert_eq!(
            broker
                .register_manifest(&request(vec![long_operation]), 4)
                .unwrap_err()
                .code,
            "CAPABILITY_GRANT_INVALID"
        );

        let too_many_grants = (0..=MAX_CAPABILITY_GRANTS_PER_MANIFEST)
            .map(|index| CapabilityGrant {
                kind: CapabilityKind::Service,
                target: format!("service:svc{index}"),
                operations: vec!["call".into()],
                max_request_bytes: 1024,
                max_response_bytes: 4096,
            })
            .collect();
        assert_eq!(
            broker
                .register_manifest(&request(too_many_grants), 4)
                .unwrap_err()
                .code,
            "CAPABILITY_MANIFEST_TOO_LARGE"
        );
    }

    #[test]
    fn denies_when_no_exact_manifest_exists() {''',
    "Controller shared limit tests",
)

core = Path("engine/crates/core/src/lib.rs")
replace_once(
    core,
    '''    MAX_CAPABILITY_PAYLOAD_BYTES as CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
''',
    '''    MAX_CAPABILITY_GRANTS_PER_MANIFEST as CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST,
    MAX_CAPABILITY_OPERATIONS_PER_GRANT as CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT,
    MAX_CAPABILITY_OPERATION_BYTES as CONTAINER_MAX_CAPABILITY_OPERATION_BYTES,
    MAX_CAPABILITY_PAYLOAD_BYTES as CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_CAPABILITY_TARGET_BYTES as CONTAINER_MAX_CAPABILITY_TARGET_BYTES,
    MAX_EXECUTION_INPUT_BYTES as CONTAINER_MAX_EXECUTION_INPUT_BYTES,
''',
    "Engine re-export shared capability ceilings",
)

runtime_image = Path("engine/crates/route-engine/src/runtime_image.rs")
replace_once(
    runtime_image,
    '''use core_lib::{
    video_language_operation_allowed, ContainerCapabilityGrant, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES, PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};''',
    '''use core_lib::{
    video_language_operation_allowed, ContainerCapabilityGrant, ContainerCapabilityKind,
    CONTAINER_MAX_CAPABILITY_GRANTS_PER_MANIFEST, CONTAINER_MAX_CAPABILITY_OPERATIONS_PER_GRANT,
    CONTAINER_MAX_CAPABILITY_OPERATION_BYTES, CONTAINER_MAX_CAPABILITY_PAYLOAD_BYTES,
    CONTAINER_MAX_CAPABILITY_TARGET_BYTES, PUBLIC_HTTP_TARGET, VIDEO_CAPABILITY_TARGET_PREFIX,
};''',
    "RELC shared capability imports",
)
replace_once(
    runtime_image,
    '''    for (service, operations) in service_operations {
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

fn valid_video_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 249
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
''',
    '''    for (service, operations) in service_operations {
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
''',
    "RELC preflight shared manifest shape",
)
replace_once(
    runtime_image,
    '''    #[test]
    fn unknown_public_http_operation_fails_closed() {''',
    '''    #[test]
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
                CONTAINER_MAX_CAPABILITY_TARGET_BYTES
                    - VIDEO_CAPABILITY_TARGET_PREFIX.len()
                    + 1,
            ),
            operation: "status".into(),
        };
        assert!(lower_container_grants(&BTreeSet::from([long_owner])).is_err());
    }

    #[test]
    fn unknown_public_http_operation_fails_closed() {''',
    "RELC shared limit tests",
)

backend = Path("engine/crates/backend/src/host_capability.rs")
replace_once(
    backend,
    '''    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse, CAPABILITY_ABI_VERSION,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    '''    CapabilityKind, HostCapabilityRequest, HostCapabilityResponse, CAPABILITY_ABI_VERSION,
    HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_OPERATION_BYTES,
    MAX_CAPABILITY_PAYLOAD_BYTES, MAX_CAPABILITY_TARGET_BYTES, MAX_HOST_CAPABILITY_FRAME_BYTES,
};''',
    "Backend shared capability imports",
)
replace_once(
    backend,
    '''const MAX_EXECUTION_ID_BYTES: usize = 128;
const MAX_LOGICAL_NAME_BYTES: usize = 256;
const MAX_SOURCE_ID_BYTES: usize = 512;
''',
    '''const MAX_EXECUTION_ID_BYTES: usize = 128;
const MAX_SOURCE_ID_BYTES: usize = 512;
''',
    "remove Backend duplicate logical limit",
)
replace_once(
    backend,
    '''    if !valid_logical_name(&request.operation) {
        return error(
            "CAPABILITY_HOST_INVALID_OPERATION",
            "invalid logical service operation",
        );
    }''',
    '''    if !valid_logical_name(&request.operation, MAX_CAPABILITY_OPERATION_BYTES) {
        return error(
            "CAPABILITY_HOST_INVALID_OPERATION",
            "invalid logical service operation",
        );
    }''',
    "Backend shared Service operation ceiling",
)
replace_once(
    backend,
    '''fn normalize_video_target(target: &str) -> Option<&str> {
    let owner = target.strip_prefix(VIDEO_CAPABILITY_TARGET_PREFIX)?;
    valid_logical_name(owner).then_some(owner)
}

fn normalize_service_target(target: &str) -> Option<&str> {
    let service = target.strip_prefix(SERVICE_CAPABILITY_TARGET_PREFIX)?;
    service_capability_name_allowed(service).then_some(service)
}

fn valid_logical_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LOGICAL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
''',
    '''fn normalize_video_target(target: &str) -> Option<&str> {
    if target.len() > MAX_CAPABILITY_TARGET_BYTES {
        return None;
    }
    let owner = target.strip_prefix(VIDEO_CAPABILITY_TARGET_PREFIX)?;
    valid_logical_name(
        owner,
        MAX_CAPABILITY_TARGET_BYTES.saturating_sub(VIDEO_CAPABILITY_TARGET_PREFIX.len()),
    )
    .then_some(owner)
}

fn normalize_service_target(target: &str) -> Option<&str> {
    if target.len() > MAX_CAPABILITY_TARGET_BYTES {
        return None;
    }
    let service = target.strip_prefix(SERVICE_CAPABILITY_TARGET_PREFIX)?;
    service_capability_name_allowed(service).then_some(service)
}

fn valid_logical_name(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
''',
    "Backend shared target ceiling",
)
replace_once(
    backend,
    '''    #[test]
    fn network_target_is_fixed_logical_public_http() {''',
    '''    #[test]
    fn trusted_host_adapter_uses_shared_capability_size_limits() {
        let max_video_owner = "x".repeat(
            MAX_CAPABILITY_TARGET_BYTES.saturating_sub(VIDEO_CAPABILITY_TARGET_PREFIX.len()),
        );
        let max_video_target = format!("{VIDEO_CAPABILITY_TARGET_PREFIX}{max_video_owner}");
        assert_eq!(normalize_video_target(&max_video_target), Some(max_video_owner.as_str()));
        assert!(normalize_video_target(&format!("{max_video_target}x")).is_none());

        assert!(valid_logical_name(
            &"x".repeat(MAX_CAPABILITY_OPERATION_BYTES),
            MAX_CAPABILITY_OPERATION_BYTES,
        ));
        assert!(!valid_logical_name(
            &"x".repeat(MAX_CAPABILITY_OPERATION_BYTES + 1),
            MAX_CAPABILITY_OPERATION_BYTES,
        ));
    }

    #[test]
    fn network_target_is_fixed_logical_public_http() {''',
    "Backend shared size limit tests",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image. Authorized host-capability dispatch uses protocol v2 to carry Controller-attested Runtime Image, SourceId, capability ABI, exact Environment, and generation alongside each call; those ownership fields come from admitted execution provenance and are never accepted from guest WASM.
''',
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image. Capability manifest shape ceilings (grant count, operations per grant, target bytes, operation bytes, and payload bytes) live in the shared `ipc-protocol` contract; RELC preflights lowered manifests against the same values before registration, while Controller remains the final authority. Authorized host-capability dispatch uses protocol v2 to carry Controller-attested Runtime Image, SourceId, capability ABI, exact Environment, and generation alongside each call; those ownership fields come from admitted execution provenance and are never accepted from guest WASM.
''',
    "shared capability limit docs",
)
