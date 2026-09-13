from pathlib import Path


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


def edit(path: str, transform) -> None:
    file = Path(path)
    before = file.read_text(encoding="utf-8")
    after = transform(before)
    if after == before:
        raise SystemExit(f"{path}: transform made no change")
    file.write_text(after, encoding="utf-8")


def edit_environment_process(text: str) -> str:
    text = one(
        text,
        "Canceller, CapabilityBroker, CapabilityCall, EnvironmentId, EnvironmentStorageManager,\n    ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,",
        "Canceller, CapabilityBroker, CapabilityCall, EnvironmentId, EnvironmentProfile,\n    EnvironmentStorageManager, ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,",
        "EnvironmentProfile import",
    )

    start_marker = "    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {"
    end_marker = "    fn new_session(&self, id: EnvironmentId, generation: u64) -> String {"
    start = text.find(start_marker)
    end = text.find(end_marker)
    if start < 0 or end < 0 or end <= start:
        raise SystemExit("spawn_one function boundaries not found")
    chunk = text[start:end]
    chunk = one(
        chunk,
        "        let session = self.new_session(id, generation);\n",
        "        let session = self.new_session(id, generation);\n        let child_debug = environment_debug_enabled(self.debug, id);\n",
        "child debug derivation",
    )
    count = chunk.count("self.debug")
    # One self.debug remains in the child_debug derivation itself; every other
    # occurrence in spawn_one must use the derived per-profile value.
    if count < 2:
        raise SystemExit(f"spawn_one self.debug count unexpectedly low: {count}")
    derivation = "environment_debug_enabled(self.debug, id)"
    protected = "environment_debug_enabled(__CONTROLLER_DEBUG__, id)"
    chunk = chunk.replace(derivation, protected, 1)
    chunk = chunk.replace("self.debug", "child_debug")
    chunk = chunk.replace(protected, derivation, 1)
    text = text[:start] + chunk + text[end:]

    marker = "fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {"
    if text.count(marker) != 1:
        raise SystemExit("active_environment_ids anchor missing")
    helper = '''fn environment_debug_enabled(controller_debug: bool, id: EnvironmentId) -> bool {
    controller_debug && id.profile() != EnvironmentProfile::Secure
}

'''
    text = text.replace(marker, helper + marker, 1)

    test_marker = '''    #[test]
    fn session_comparison_rejects_wrong_value() {'''
    if text.count(test_marker) != 1:
        raise SystemExit("environment debug test anchor missing")
    test = '''    #[test]
    fn secure_environment_never_inherits_controller_debug() {
        assert!(environment_debug_enabled(true, EnvironmentId::General1));
        assert!(!environment_debug_enabled(true, EnvironmentId::Payment));
        assert!(!environment_debug_enabled(false, EnvironmentId::General1));
        assert!(!environment_debug_enabled(false, EnvironmentId::Payment));
    }

'''
    return text.replace(test_marker, test + test_marker, 1)


edit("container-runtime/crates/container-bin/src/environment_process.rs", edit_environment_process)


def edit_control_plane(text: str) -> str:
    import_anchor = "use std::sync::RwLock;\n\nuse ipc_protocol::{"
    text = one(
        text,
        import_anchor,
        "use std::sync::RwLock;\n\nuse environments::EnvironmentProfile;\nuse ipc_protocol::{",
        "EnvironmentProfile policy import",
    )

    text = one(
        text,
        "        validate_environment(&request.environment)?;\n        if request.capability_abi != CAPABILITY_ABI_VERSION {",
        "        let environment_profile = validate_environment(&request.environment)?;\n        if request.capability_abi != CAPABILITY_ABI_VERSION {",
        "manifest environment profile capture",
    )
    text = one(
        text,
        "            validate_grant(grant, self.debug_enabled)?;",
        "            validate_grant(grant, self.debug_enabled, environment_profile)?;",
        "profile-aware grant validation",
    )

    authorize_start = text.find("    pub fn authorize(\n")
    revoke_start = text.find("    pub fn revoke_environment_generation", authorize_start)
    if authorize_start < 0 or revoke_start < 0:
        raise SystemExit("authorize function boundaries missing")
    authorize = text[authorize_start:revoke_start]
    authorize = one(
        authorize,
        "        validate_environment(call.environment)?;",
        "        let environment_profile = validate_environment(call.environment)?;",
        "call environment profile capture",
    )
    old_guard = '''        if matches!(call.kind, CapabilityKind::Debug | CapabilityKind::HostFile)
            && !self.debug_enabled
        {
            return Err(CapabilityError {
                code: "DEBUG_DISABLED",
                message: "debug-only capability is disabled by the Container Controller".into(),
            });
        }'''
    new_guard = '''        if matches!(call.kind, CapabilityKind::Debug | CapabilityKind::HostFile) {
            if environment_profile == EnvironmentProfile::Secure {
                return Err(CapabilityError {
                    code: "CAPABILITY_DENIED",
                    message: "debug/host-file capabilities are unavailable to secure Environments"
                        .into(),
                });
            }
            if !self.debug_enabled {
                return Err(CapabilityError {
                    code: "DEBUG_DISABLED",
                    message: "debug-only capability is disabled by the Container Controller".into(),
                });
            }
        }'''
    authorize = one(authorize, old_guard, new_guard, "secure call-time authority guard")
    text = text[:authorize_start] + authorize + text[revoke_start:]

    text = one(
        text,
        "fn validate_grant(grant: &CapabilityGrant, debug_enabled: bool) -> Result<(), CapabilityError> {",
        "fn validate_grant(\n    grant: &CapabilityGrant,\n    debug_enabled: bool,\n    environment_profile: EnvironmentProfile,\n) -> Result<(), CapabilityError> {",
        "validate_grant signature",
    )
    old_grant_guard = '''    if matches!(grant.kind, CapabilityKind::Debug | CapabilityKind::HostFile) && !debug_enabled {
        return Err(CapabilityError {
            code: "DEBUG_DISABLED",
            message: "debug/host-file grants cannot be registered in a production Controller"
                .into(),
        });
    }'''
    new_grant_guard = '''    if matches!(grant.kind, CapabilityKind::Debug | CapabilityKind::HostFile) {
        if environment_profile == EnvironmentProfile::Secure {
            return Err(CapabilityError {
                code: "CAPABILITY_DENIED",
                message: "debug/host-file grants cannot be registered for secure Environments"
                    .into(),
            });
        }
        if !debug_enabled {
            return Err(CapabilityError {
                code: "DEBUG_DISABLED",
                message: "debug/host-file grants cannot be registered in a production Controller"
                    .into(),
            });
        }
    }'''
    text = one(text, old_grant_guard, new_grant_guard, "secure manifest authority guard")

    old_environment = '''fn validate_environment(value: &str) -> Result<(), CapabilityError> {
    if !matches!(
        value,
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5" | "payment"
    ) {
        return Err(invalid_identity(
            "environment",
            "is not a configured RBE Environment",
        ));
    }
    Ok(())
}'''
    new_environment = '''fn validate_environment(value: &str) -> Result<EnvironmentProfile, CapabilityError> {
    match value {
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5" => {
            Ok(EnvironmentProfile::General)
        }
        "payment" => Ok(EnvironmentProfile::Secure),
        _ => Err(invalid_identity(
            "environment",
            "is not a configured RBE Environment",
        )),
    }
}'''
    text = one(text, old_environment, new_environment, "environment profile validation")

    test_marker = '''    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {'''
    if text.count(test_marker) != 1:
        raise SystemExit("secure manifest tests anchor missing")
    tests = '''    #[test]
    fn debug_controller_still_denies_debug_and_host_file_to_secure_profile() {
        let broker = CapabilityBroker::new(true);
        for kind in [CapabilityKind::Debug, CapabilityKind::HostFile] {
            let mut general_grant = service_grant();
            general_grant.kind = kind;
            general_grant.target = "shell".into();
            broker
                .register_manifest(&request(vec![general_grant]), 4)
                .expect("debug general Environment should retain debug authority");

            let mut secure_request = request(vec![{
                let mut grant = service_grant();
                grant.kind = kind;
                grant.target = "shell".into();
                grant
            }]);
            secure_request.environment = "payment".into();
            assert_eq!(
                broker
                    .register_manifest(&secure_request, 4)
                    .unwrap_err()
                    .code,
                "CAPABILITY_DENIED"
            );
        }
    }

    #[test]
    fn call_time_guard_denies_secure_debug_even_if_manifest_table_is_injected() {
        let broker = CapabilityBroker::new(true);
        let grant = CapabilityGrant {
            kind: CapabilityKind::Debug,
            target: "shell".into(),
            operations: vec!["inspect".into()],
            max_request_bytes: 1024,
            max_response_bytes: 4096,
        };
        broker
            .manifests
            .write()
            .unwrap()
            .insert(
                ManifestKey {
                    runtime_image: "ab".repeat(32),
                    source_id: "route:api/me".into(),
                    environment: "payment".into(),
                    generation: 4,
                },
                CapabilityManifest {
                    grants: vec![grant],
                },
            );
        let error = broker
            .authorize(CapabilityCall {
                runtime_image: &"ab".repeat(32),
                source_id: "route:api/me",
                environment: "payment",
                generation: 4,
                kind: CapabilityKind::Debug,
                target: "shell",
                operation: "inspect",
                request_bytes: 1,
            })
            .unwrap_err();
        assert_eq!(error.code, "CAPABILITY_DENIED");
    }

'''
    return text.replace(test_marker, tests + test_marker, 1)


edit("container-runtime/crates/container-runtime-core/src/control_plane.rs", edit_control_plane)
