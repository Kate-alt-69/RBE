from pathlib import Path
import re


def rep(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    found = text.count(old)
    if found != count:
        raise SystemExit(f"{path}: expected {count} anchors, found {found}: {old[:160]!r}")
    p.write_text(text.replace(old, new, count), encoding="utf-8")


def regex_rep(path: str, pattern: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    text, replaced = re.subn(pattern, new, text, count=count, flags=re.S)
    if replaced != count:
        raise SystemExit(
            f"{path}: expected {count} regex replacements, got {replaced}: {pattern[:160]!r}"
        )
    p.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Core execution model: provenance becomes part of the queued task itself.
# ---------------------------------------------------------------------------
execution = "container-runtime/crates/container-runtime-core/src/execution.rs"
rep(
    execution,
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {''',
    '''/// Controller-stamped identity that authorized one execution. The caller
/// supplies Runtime Image / SourceId / ABI, but only Container Controller may
/// bind those values to an Environment generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProvenance {
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {''',
)
rep(
    execution,
    '''pub struct ExecutionTask {
    pub id: ExecutionId,
    pub environment: String,
    pub artifact_hash: String,''',
    '''pub struct ExecutionTask {
    pub id: ExecutionId,
    pub environment: String,
    /// None exists only for trusted internal/simulated work and legacy journal
    /// decoding. Production Environment dispatch rejects unattributed tasks.
    pub provenance: Option<ExecutionProvenance>,
    pub artifact_hash: String,''',
)

lib = "container-runtime/crates/container-runtime-core/src/lib.rs"
rep(
    lib,
    '''pub use execution::{
    ExecutionId, ExecutionOutcome, ExecutionRecord, ExecutionState, ExecutionTask, WorkCost,
};''',
    '''pub use execution::{
    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionRecord, ExecutionState,
    ExecutionTask, WorkCost,
};''',
)


# ---------------------------------------------------------------------------
# Journal + scheduler: preserve provenance through queueing and recovery while
# keeping old journal records readable as explicitly unattributed legacy work.
# ---------------------------------------------------------------------------
runtime = "container-runtime/crates/container-runtime-core/src/runtime.rs"
rep(
    runtime,
    '''use crate::execution::{ExecutionId, ExecutionOutcome, ExecutionTask, WorkCost};''',
    '''use crate::execution::{
    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionTask, WorkCost,
};''',
)
rep(
    runtime,
    '''    sequence: u64,
    environment: String,
    artifact_hash: String,''',
    '''    sequence: u64,
    environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capability_abi: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment_generation: Option<u64>,
    artifact_hash: String,''',
)

# Cancellation/checkpoint events intentionally carry no caller authority.
rep(
    runtime,
    '''            environment: "unknown".into(),
            artifact_hash: "unknown".into(),''',
    '''            environment: "unknown".into(),
            runtime_image: None,
            source_id: None,
            capability_abi: None,
            environment_generation: None,
            artifact_hash: "unknown".into(),''',
)
rep(
    runtime,
    '''        environment: "checkpoint".into(),
        artifact_hash: "checkpoint".into(),''',
    '''        environment: "checkpoint".into(),
        runtime_image: None,
        source_id: None,
        capability_abi: None,
        environment_generation: None,
        artifact_hash: "checkpoint".into(),''',
)

# Recovery reconstructs provenance only when the complete tuple exists. A
# partially written/legacy identity therefore becomes unattributed and the
# production Environment boundary rejects it rather than inventing authority.
rep(
    runtime,
    '''fn event_to_task(event: JournalEvent) -> Option<ExecutionTask> {
    let environment = parse_environment(&event.environment)?;
    Some(ExecutionTask {
        id: ExecutionId::from_parts(event.epoch_ns, event.sequence),
        environment: environment.to_string(),
        artifact_hash: event.artifact_hash,''',
    '''fn event_to_task(event: JournalEvent) -> Option<ExecutionTask> {
    let environment = parse_environment(&event.environment)?;
    let provenance_environment = event.environment.clone();
    let provenance = match (
        event.runtime_image,
        event.source_id,
        event.capability_abi,
        event.environment_generation,
    ) {
        (Some(runtime_image), Some(source_id), Some(capability_abi), Some(generation)) => {
            Some(ExecutionProvenance {
                runtime_image,
                source_id,
                capability_abi,
                environment: provenance_environment,
                generation,
            })
        }
        _ => None,
    };
    Some(ExecutionTask {
        id: ExecutionId::from_parts(event.epoch_ns, event.sequence),
        environment: environment.to_string(),
        provenance,
        artifact_hash: event.artifact_hash,''',
)

# Completion records retain the same provenance for audit/compaction.
rep(
    runtime,
    '''                    sequence: task.id.sequence(),
                    environment: task.environment.clone(),
                    artifact_hash: task.artifact_hash.clone(),''',
    '''                    sequence: task.id.sequence(),
                    environment: task.environment.clone(),
                    runtime_image: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.runtime_image.clone()),
                    source_id: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.source_id.clone()),
                    capability_abi: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.capability_abi),
                    environment_generation: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.generation),
                    artifact_hash: task.artifact_hash.clone(),''',
)

# Existing internal submission remains unattributed. The Controller admission
# path below supplies Some(provenance).
rep(
    runtime,
    '''            ResourceLimits::default(),
            SandboxPolicy::default(),
            work_ms,
            Vec::new(),''',
    '''            ResourceLimits::default(),
            SandboxPolicy::default(),
            None,
            work_ms,
            Vec::new(),''',
)
rep(
    runtime,
    '''        limits: ResourceLimits,
        sandbox: SandboxPolicy,
        work_ms: u64,
        payload: Vec<u8>,''',
    '''        limits: ResourceLimits,
        sandbox: SandboxPolicy,
        provenance: Option<ExecutionProvenance>,
        work_ms: u64,
        payload: Vec<u8>,''',
)
rep(
    runtime,
    '''            sequence: id.sequence(),
            environment: environment.to_string(),
            artifact_hash: artifact_hash.clone(),''',
    '''            sequence: id.sequence(),
            environment: environment.to_string(),
            runtime_image: provenance
                .as_ref()
                .map(|provenance| provenance.runtime_image.clone()),
            source_id: provenance
                .as_ref()
                .map(|provenance| provenance.source_id.clone()),
            capability_abi: provenance
                .as_ref()
                .map(|provenance| provenance.capability_abi),
            environment_generation: provenance
                .as_ref()
                .map(|provenance| provenance.generation),
            artifact_hash: artifact_hash.clone(),''',
)
rep(
    runtime,
    '''                    id,
                    environment: environment.to_string(),
                    artifact_hash,''',
    '''                    id,
                    environment: environment.to_string(),
                    provenance,
                    artifact_hash,''',
)

# Focused recovery tests: new records preserve identity; pre-CAP-003 records are
# still readable but stay explicitly unattributed.
rep(
    runtime,
    '''    }
}
''',
    '''    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn journal_event(with_provenance: bool) -> JournalEvent {
        JournalEvent {
            kind: "queued".into(),
            epoch_ns: 7,
            sequence: 9,
            environment: "general-1".into(),
            runtime_image: with_provenance.then(|| "ab".repeat(32)),
            source_id: with_provenance.then(|| "route:api/me".into()),
            capability_abi: with_provenance.then_some(1),
            environment_generation: with_provenance.then_some(4),
            artifact_hash: "cd".repeat(32),
            cpu: 1,
            memory: 2,
            io: 3,
            network: 4,
            work_ms: 0,
            limit_cpu_millis: 100,
            limit_memory_bytes: 1024,
            limit_disk_bytes: 1024,
            limit_network_bytes: 1024,
            limit_max_processes: 1,
            limit_max_file_descriptors: 16,
            limit_wall_time_ms: 1000,
        }
    }

    #[test]
    fn journal_recovery_preserves_execution_provenance() {
        let task = event_to_task(journal_event(true)).expect("recover task");
        let provenance = task.provenance.expect("recover provenance");
        assert_eq!(provenance.runtime_image, "ab".repeat(32));
        assert_eq!(provenance.source_id, "route:api/me");
        assert_eq!(provenance.environment, "general-1");
        assert_eq!(provenance.generation, 4);
    }

    #[test]
    fn legacy_journal_recovery_remains_unattributed() {
        let task = event_to_task(journal_event(false)).expect("recover legacy task");
        assert!(task.provenance.is_none());
    }
}
''',
    count=1,
)


# ---------------------------------------------------------------------------
# Capability Broker: an exact identity key is immutable for its generation.
# Re-registering byte-for-byte-equivalent grants is idempotent; changing grants
# under the same key is rejected.
# ---------------------------------------------------------------------------
broker = "container-runtime/crates/container-runtime-core/src/control_plane.rs"
rep(
    broker,
    '''#[derive(Debug, Clone)]
struct CapabilityManifest {''',
    '''#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityManifest {''',
)
regex_rep(
    broker,
    r'''        self\.manifests\n            \.write\(\)\n            \.expect\("capability manifest table poisoned"\)\n            \.insert\(\n                key,\n                CapabilityManifest \{\n                    grants: request\.grants\.clone\(\),\n                \},\n            \);\n        Ok\(request\.grants\.len\(\)\)''',
    '''        let proposed = CapabilityManifest {
            grants: request.grants.clone(),
        };
        let mut manifests = self
            .manifests
            .write()
            .expect("capability manifest table poisoned");
        match manifests.get(&key) {
            Some(existing) if existing == &proposed => Ok(request.grants.len()),
            Some(_) => Err(CapabilityError {
                code: "CAPABILITY_MANIFEST_CONFLICT",
                message: "an exact Runtime Image/SourceId/Environment generation manifest is immutable"
                    .into(),
            }),
            None => {
                manifests.insert(key, proposed);
                Ok(request.grants.len())
            }
        }''',
)
rep(
    broker,
    '''    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {''',
    '''    #[test]
    fn manifest_registration_is_idempotent_but_immutable() {
        let broker = CapabilityBroker::new(false);
        let original = request(vec![service_grant()]);
        assert_eq!(broker.register_manifest(&original, 4).unwrap(), 1);
        assert_eq!(broker.register_manifest(&original, 4).unwrap(), 1);
        assert_eq!(broker.manifest_count(), 1);

        let mut changed = request(vec![service_grant()]);
        changed.grants[0].max_request_bytes = 2048;
        assert_eq!(
            broker.register_manifest(&changed, 4).unwrap_err().code,
            "CAPABILITY_MANIFEST_CONFLICT"
        );
        assert_eq!(broker.manifest_count(), 1);
    }

    #[test]
    fn production_controller_refuses_debug_and_host_file_grants() {''',
)


# ---------------------------------------------------------------------------
# Controller admission: build provenance after exact-manifest authorization and
# hand that stamped tuple into the scheduler.
# ---------------------------------------------------------------------------
main = "container-runtime/crates/container-bin/src/main.rs"
rep(
    main,
    '''use container_runtime_core::{
    CapabilityBroker, EnvironmentId, EnvironmentRegistry, Runtime, RuntimeConfig, WorkCost,
};''',
    '''use container_runtime_core::{
    CapabilityBroker, EnvironmentId, EnvironmentRegistry, ExecutionProvenance, Runtime,
    RuntimeConfig, WorkCost,
};''',
)
rep(
    main,
    '''                let cost = WorkCost {
                    cpu: request.declared_cost.cpu,
                    memory: request.declared_cost.memory,
                    io: request.declared_cost.io,
                    network: request.declared_cost.network,
                };
                let execution_id = runtime.submit_with_policy(
                    environment,
                    request.artifact_hash,
                    cost,
                    ResourceLimits::default(),
                    SandboxPolicy::default(),
                    0,
                    request.input,
                );''',
    '''                let cost = WorkCost {
                    cpu: request.declared_cost.cpu,
                    memory: request.declared_cost.memory,
                    io: request.declared_cost.io,
                    network: request.declared_cost.network,
                };
                let provenance = ExecutionProvenance {
                    runtime_image: request.runtime_image,
                    source_id: request.source_id,
                    capability_abi: request.capability_abi,
                    environment: request.environment.clone(),
                    generation,
                };
                let execution_id = runtime.submit_with_policy(
                    environment,
                    request.artifact_hash,
                    cost,
                    ResourceLimits::default(),
                    SandboxPolicy::default(),
                    Some(provenance),
                    0,
                    request.input,
                );''',
)


# ---------------------------------------------------------------------------
# Environment process boundary: provenance must match the currently live child
# generation before dispatch. The child independently validates the tuple and
# retains the identity while its disposable worker is active.
# ---------------------------------------------------------------------------
envp = "container-runtime/crates/container-bin/src/environment_process.rs"
rep(
    envp,
    '''    read_frame, read_worker_result, write_frame, write_worker_input, WorkerResultFrame,
    MAX_EXECUTION_INPUT_BYTES,
};''',
    '''    read_frame, read_worker_result, write_frame, write_worker_input, WorkerResultFrame,
    CAPABILITY_ABI_VERSION, MAX_EXECUTION_INPUT_BYTES,
};''',
)
rep(envp, "const CHILD_PROTOCOL_VERSION: u16 = 1;", "const CHILD_PROTOCOL_VERSION: u16 = 2;")
rep(
    envp,
    '''        session: String,
        environment: String,
        generation: u64,
        artifact_hash: String,''',
    '''        session: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        environment: String,
        generation: u64,
        artifact_hash: String,''',
)
rep(
    envp,
    '''#[derive(Debug, Clone, Copy)]
struct ExecutionOwner {
    environment: EnvironmentId,
    generation: u64,
}

struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, u64>>,''',
    '''#[derive(Debug, Clone, Copy)]
struct ExecutionOwner {
    environment: EnvironmentId,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveExecutionIdentity {
    generation: u64,
    runtime_image: String,
    source_id: String,
    capability_abi: u16,
}

struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,''',
)
rep(
    envp,
    '''struct WorkerExecution<'a> {
    execution_id: &'a str,
    generation: u64,
    artifact_hash: &'a str,''',
    '''struct WorkerExecution<'a> {
    execution_id: &'a str,
    generation: u64,
    runtime_image: &'a str,
    source_id: &'a str,
    capability_abi: u16,
    artifact_hash: &'a str,''',
)

# Validate the stamped task against the live child before opening its execution
# channel. A queued task from an older generation cannot inherit new authority.
rep(
    envp,
    '''        let mut stream = TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect Environment {}: {error}", task.environment))?;''',
    '''        let provenance = task
            .provenance
            .as_ref()
            .ok_or_else(|| "execution provenance is required for Environment dispatch".to_string())?;
        if provenance.environment != task.environment {
            return Err("execution provenance Environment does not match the queued task".into());
        }
        if provenance.generation != endpoint.generation {
            return Err(format!(
                "execution provenance generation {} is stale; live Environment generation is {}",
                provenance.generation, endpoint.generation
            ));
        }
        if provenance.capability_abi != CAPABILITY_ABI_VERSION
            || !valid_runtime_image(&provenance.runtime_image)
            || !valid_source_id(&provenance.source_id)
        {
            return Err("execution provenance identity is invalid".into());
        }

        let mut stream = TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect Environment {}: {error}", task.environment))?;''',
)
rep(
    envp,
    '''        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session,
            environment: task.environment.clone(),
            generation: endpoint.generation,
            artifact_hash: task.artifact_hash.clone(),''',
    '''        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session,
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: task.environment.clone(),
            generation: provenance.generation,
            artifact_hash: task.artifact_hash.clone(),''',
)

# Child independently validates the provenance tuple from the authenticated
# Controller session before a disposable worker is spawned.
rep(
    envp,
    '''        ChildRequest::Execute {
            request_id,
            session,
            environment,
            generation,
            artifact_hash,''',
    '''        ChildRequest::Execute {
            request_id,
            session,
            runtime_image,
            source_id,
            capability_abi,
            environment,
            generation,
            artifact_hash,''',
)
rep(
    envp,
    '''            } else if input.len() > MAX_EXECUTION_INPUT_BYTES {
                child_error(''',
    '''            } else if capability_abi != CAPABILITY_ABI_VERSION
                || !valid_runtime_image(&runtime_image)
                || !valid_source_id(&source_id)
            {
                child_error(
                    Some(request_id),
                    "EXECUTION_PROVENANCE_INVALID",
                    "Runtime Image/SourceId/capability ABI provenance is invalid",
                )
            } else if input.len() > MAX_EXECUTION_INPUT_BYTES {
                child_error(''',
)
rep(
    envp,
    '''                let worker = WorkerExecution {
                    execution_id: &request_id,
                    generation,
                    artifact_hash: &artifact_hash,''',
    '''                let worker = WorkerExecution {
                    execution_id: &request_id,
                    generation,
                    runtime_image: &runtime_image,
                    source_id: &source_id,
                    capability_abi,
                    artifact_hash: &artifact_hash,''',
)
rep(
    envp,
    '''        execution_id,
        generation,
        artifact_hash,''',
    '''        execution_id,
        generation,
        runtime_image,
        source_id,
        capability_abi,
        artifact_hash,''',
)
rep(
    envp,
    '''        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .insert(execution_id.to_string(), generation);''',
    '''        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .insert(
            execution_id.to_string(),
            ActiveExecutionIdentity {
                generation,
                runtime_image: runtime_image.to_string(),
                source_id: source_id.to_string(),
                capability_abi,
            },
        );''',
)
rep(
    envp,
    '''                    .get(&execution_id)
                    .copied()
                    == Some(generation);''',
    '''                    .get(&execution_id)
                    .map(|identity| {
                        identity.generation == generation
                            && identity.capability_abi == CAPABILITY_ABI_VERSION
                            && valid_runtime_image(&identity.runtime_image)
                            && valid_source_id(&identity.source_id)
                    })
                    .unwrap_or(false);''',
)

# Shared format validation is intentionally local to the Environment process;
# it does not consult the broker or any host-owned target table.
rep(
    envp,
    '''fn child_error(request_id: Option<String>, code: &str, message: &str) -> ChildResponse {''',
    '''fn valid_runtime_image(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains('\\0')
        && !value.chars().any(char::is_control)
}

fn child_error(request_id: Option<String>, code: &str, message: &str) -> ChildResponse {''',
)
rep(
    envp,
    '''    #[test]
    fn session_comparison_rejects_wrong_value() {''',
    '''    #[test]
    fn execution_provenance_format_is_strict() {
        assert!(valid_runtime_image(&"ab".repeat(32)));
        assert!(!valid_runtime_image(&"AB".repeat(32)));
        assert!(valid_source_id("route:api/me"));
        assert!(!valid_source_id(""));
        assert!(!valid_source_id("route:\\napi"));
    }

    #[test]
    fn session_comparison_rejects_wrong_value() {''',
)

# Basic static sanity checks before CI spends time compiling.
checks = {
    execution: ["pub struct ExecutionProvenance", "pub provenance: Option<ExecutionProvenance>"],
    runtime: ["environment_generation: Option<u64>", "journal_recovery_preserves_execution_provenance"],
    broker: ["CAPABILITY_MANIFEST_CONFLICT", "manifest_registration_is_idempotent_but_immutable"],
    main: ["Some(provenance)", "ExecutionProvenance {"],
    envp: ["const CHILD_PROTOCOL_VERSION: u16 = 2", "execution provenance is required for Environment dispatch"],
}
for path, needles in checks.items():
    text = Path(path).read_text(encoding="utf-8")
    for needle in needles:
        if needle not in text:
            raise SystemExit(f"{path}: CAP-003 invariant missing: {needle}")
