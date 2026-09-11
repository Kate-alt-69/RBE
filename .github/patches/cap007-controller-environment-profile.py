from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# Controller owns general-environment profile selection.
runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
replace_once(
    runtime,
    '''pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,''',
    '''pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    next_general_environment: AtomicU64,''',
    "runtime general selector state",
)
replace_once(
    runtime,
    '''            config,
            next_execution: AtomicU64::new(max_sequence.saturating_add(1).max(1)),
            global_queue:''',
    '''            config,
            next_execution: AtomicU64::new(max_sequence.saturating_add(1).max(1)),
            next_general_environment: AtomicU64::new(0),
            global_queue:''',
    "runtime general selector init",
)
replace_once(
    runtime,
    '''    pub fn environment_generation(&self, id: EnvironmentId) -> u64 {
        self.generations
            .lock()
            .expect("generation table poisoned")
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    pub fn rebalance_once(&self) {''',
    '''    pub fn environment_generation(&self, id: EnvironmentId) -> u64 {
        self.generations
            .lock()
            .expect("generation table poisoned")
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    /// Resolve the logical `general` profile inside Controller authority. The
    /// caller receives an exact Environment binding and never participates in
    /// generation selection. Payment is deliberately outside this pool.
    pub fn select_general_environment(&self) -> EnvironmentId {
        let sequence = self
            .next_general_environment
            .fetch_add(1, Ordering::Relaxed);
        general_environment_for_sequence(self.config.general_environments, sequence)
    }

    pub fn rebalance_once(&self) {''',
    "runtime general selector method",
)
replace_once(
    runtime,
    '''fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {
    let mut ids =
        EnvironmentId::GENERAL[..general_count.clamp(1, EnvironmentId::GENERAL.len())].to_vec();
    ids.push(EnvironmentId::Payment);
    ids
}

fn physical_core_count() -> usize {''',
    '''fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {
    let mut ids =
        EnvironmentId::GENERAL[..general_count.clamp(1, EnvironmentId::GENERAL.len())].to_vec();
    ids.push(EnvironmentId::Payment);
    ids
}

fn general_environment_for_sequence(general_count: usize, sequence: u64) -> EnvironmentId {
    let count = general_count.clamp(1, EnvironmentId::GENERAL.len());
    EnvironmentId::GENERAL[(sequence % count as u64) as usize]
}

fn physical_core_count() -> usize {''',
    "runtime pure general selector",
)
replace_once(
    runtime,
    '''    #[test]
    fn legacy_journal_recovery_remains_unattributed() {
        let task = event_to_task(journal_event(false)).expect("recover legacy task");
        assert!(task.provenance.is_none());
    }
}''',
    '''    #[test]
    fn legacy_journal_recovery_remains_unattributed() {
        let task = event_to_task(journal_event(false)).expect("recover legacy task");
        assert!(task.provenance.is_none());
    }

    #[test]
    fn general_profile_round_robins_only_configured_general_environments() {
        assert_eq!(general_environment_for_sequence(3, 0), EnvironmentId::General1);
        assert_eq!(general_environment_for_sequence(3, 1), EnvironmentId::General2);
        assert_eq!(general_environment_for_sequence(3, 2), EnvironmentId::General3);
        assert_eq!(general_environment_for_sequence(3, 3), EnvironmentId::General1);
        for sequence in 0..32 {
            assert_ne!(general_environment_for_sequence(5, sequence), EnvironmentId::Payment);
        }
    }

    #[test]
    fn general_profile_respects_single_environment_configuration() {
        for sequence in 0..8 {
            assert_eq!(general_environment_for_sequence(1, sequence), EnvironmentId::General1);
        }
    }
}''',
    "runtime general selector tests",
)

# Manifest registration may accept a logical profile, but broker keys and
# execution remain exact Environment identities.
controller = Path("container-runtime/crates/container-bin/src/main.rs")
old_manifest = '''        Request::RegisterCapabilityManifest(request) => {
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
        }'''
new_manifest = '''        Request::RegisterCapabilityManifest(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                let requested_environment = request.environment.clone();
                let environment = if requested_environment == "general" {
                    Some(runtime.select_general_environment())
                } else {
                    parse_environment(&requested_environment)
                        .filter(|id| runtime.has_environment(*id))
                };
                let Some(environment) = environment else {
                    return write_frame(
                        &mut stream,
                        &Response::Error {
                            request_id: Some(request.request_id),
                            code: "INVALID_ENVIRONMENT".into(),
                            message: format!(
                                "container environment/profile is unavailable: {requested_environment}"
                            ),
                        },
                    )
                    .map_err(Into::into);
                };

                let exact_environment = environment.to_string();
                let generation = runtime.environment_generation(environment);
                let mut bound_request = request;
                bound_request.environment = exact_environment.clone();
                match capability_broker.register_manifest(&bound_request, generation) {
                    Ok(grants) => {
                        emit_event(
                            "capability_manifest_registered",
                            &format!(
                                "runtime_image={} source_id={} requested_environment={} environment={} generation={} grants={grants}",
                                bound_request.runtime_image,
                                bound_request.source_id,
                                requested_environment,
                                exact_environment,
                                generation
                            ),
                        );
                        Response::CapabilityManifestRegistered {
                            request_id: bound_request.request_id,
                            runtime_image: bound_request.runtime_image,
                            source_id: bound_request.source_id,
                            environment: exact_environment,
                            generation,
                            grants,
                        }
                    }
                    Err(error) => Response::Error {
                        request_id: Some(bound_request.request_id),
                        code: error.code.into(),
                        message: error.message,
                    },
                }
            }
        }'''
replace_once(controller, old_manifest, new_manifest, "Controller profile manifest binding")

# Backend must use the exact Environment returned by Controller for execution.
client = Path("engine/crates/core/src/container_client.rs")
replace_once(
    client,
    '''#[derive(Debug)]
pub struct ContainerAuthorizedExecution<'a> {''',
    '''#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerCapabilityBinding {
    pub environment: String,
    pub generation: u64,
}

#[derive(Debug)]
pub struct ContainerAuthorizedExecution<'a> {''',
    "Container binding type",
)
replace_once(
    client,
    '''    /// Register the exact capability set for one Runtime Image source.
    /// Container Controller supplies and returns the live Environment generation.
    pub async fn register_capability_manifest(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<u64> {''',
    '''    /// Register one source's capability set against an exact Environment or
    /// a Controller-owned logical profile such as `general`. Controller returns
    /// the exact Environment + generation it bound; callers never mint either.
    pub async fn register_capability_manifest(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        grants: Vec<CapabilityGrant>,
    ) -> anyhow::Result<ContainerCapabilityBinding> {''',
    "manifest registration binding return",
)
replace_once(
    client,
    '''        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::CapabilityManifestRegistered { generation, .. } => Ok(generation),''',
    '''        match call(endpoint, request, Duration::from_secs(5)).await? {
            Response::CapabilityManifestRegistered {
                environment,
                generation,
                ..
            } => Ok(ContainerCapabilityBinding {
                environment,
                generation,
            }),''',
    "manifest response exact binding",
)
replace_once(
    client,
    '''        let generation = self
            .register_capability_manifest(request.identity, request.grants)
            .await?;
        tracing::debug!(
            runtime_image = request.identity.runtime_image,
            source_id = request.identity.source_id,
            environment = request.identity.environment,
            generation,
            artifact_hash = request.artifact_hash,
            "Container execution authority admitted"
        );
        self.execute_and_wait(
            request.identity,
            request.artifact_hash,
            request.input,
            request.declared_cost,
            request.timeout,
        )
        .await''',
    '''        let binding = self
            .register_capability_manifest(request.identity, request.grants)
            .await?;
        let exact_identity = ContainerExecutionIdentity {
            runtime_image: request.identity.runtime_image,
            source_id: request.identity.source_id,
            environment: &binding.environment,
        };
        tracing::debug!(
            runtime_image = exact_identity.runtime_image,
            source_id = exact_identity.source_id,
            requested_environment = request.identity.environment,
            environment = exact_identity.environment,
            generation = binding.generation,
            artifact_hash = request.artifact_hash,
            "Container execution authority admitted"
        );
        self.execute_and_wait(
            exact_identity,
            request.artifact_hash,
            request.input,
            request.declared_cost,
            request.timeout,
        )
        .await''',
    "authorized execution exact environment binding",
)

core_lib = Path("engine/crates/core/src/lib.rs")
replace_once(
    core_lib,
    '''    ContainerAuthorizedExecution, ContainerClient, ContainerEndpointSnapshot,
    ContainerExecutionIdentity,
};''',
    '''    ContainerAuthorizedExecution, ContainerCapabilityBinding, ContainerClient,
    ContainerEndpointSnapshot, ContainerExecutionIdentity,
};''',
    "core binding export",
)

# Native Runtime Image routes ask for the logical general profile; Controller
# chooses the exact general-N Environment and the client reuses that binding.
discovery = Path("engine/crates/route-engine/src/discovery.rs")
replace_once(
    discovery,
    '''const NATIVE_ROUTE_ENVIRONMENT: &str = "general-1";''',
    '''const NATIVE_ROUTE_ENVIRONMENT_PROFILE: &str = "general";''',
    "native environment profile constant",
)
replace_once(
    discovery,
    '''        environment: NATIVE_ROUTE_ENVIRONMENT,''',
    '''        environment: NATIVE_ROUTE_ENVIRONMENT_PROFILE,''',
    "native environment profile use",
)

# Current-runtime documentation: logical profile is admission-only; execution is exact.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator.''',
    '''Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator.

Native routes request the logical `general` Environment profile during manifest admission. Container Controller resolves that profile round-robin across the configured `general-N` Environments and returns one exact Environment + generation binding. The Execute request then uses that exact Environment; `general` is never accepted as wildcard execution authority, and the dedicated Payment Environment is never selected by the general profile.''',
    "general Environment profile documentation",
)
