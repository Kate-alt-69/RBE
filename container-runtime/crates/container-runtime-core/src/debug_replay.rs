use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_io::AtomicIo;
use environments::EnvironmentId;
use ipc_protocol::{CapabilityKind, WorkerCapabilityCall, WorkerCapabilityResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ExecutionTask;

pub const DEBUG_REPLAY_TRACE_VERSION: u16 = 1;
pub const MAX_DEBUG_REPLAY_INPUT_BYTES: usize = 512 * 1024;
pub const MAX_DEBUG_REPLAY_EVENTS: usize = 128;
pub const MAX_DEBUG_REPLAY_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_DEBUG_REPLAY_TOTAL_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_DEBUG_REPLAY_ERROR_BYTES: usize = 8 * 1024;
pub const MAX_DEBUG_REPLAY_TRACE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DEBUG_REPLAY_FILES: usize = 32;

const TRACE_HASH_DOMAIN: &[u8] = b"RBE_DEBUG_REPLAY_TRACE_V1\0";
const VALUE_HASH_DOMAIN: &[u8] = b"RBE_DEBUG_REPLAY_VALUE_V1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugReplayError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for DebugReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DebugReplayError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugReplayTrace {
    pub version: u16,
    pub created_unix_ms: u64,
    pub execution_id: String,
    pub artifact_hash: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
    pub input_hex: String,
    pub input_sha256: String,
    pub capability_events: Vec<DebugReplayCapabilityEvent>,
    pub terminal: DebugReplayTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugReplayCapabilityEvent {
    pub call_id: u64,
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub request_bytes: usize,
    pub request_sha256: String,
    pub result: DebugReplayCapabilityResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DebugReplayCapabilityResult {
    Success {
        payload_hex: String,
        payload_sha256: String,
    },
    Error {
        code: String,
        message: String,
        message_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DebugReplayTerminal {
    Success {
        output_bytes: usize,
        output_sha256: String,
    },
    Error {
        code: String,
        message_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DebugReplayEnvelope {
    payload: DebugReplayTrace,
    sha256: String,
}

pub struct DebugReplayRecorder {
    created_unix_ms: u64,
    execution_id: String,
    artifact_hash: String,
    runtime_image: String,
    source_id: String,
    capability_abi: u16,
    environment: String,
    generation: u64,
    input_hex: String,
    input_sha256: String,
    capability_events: Vec<DebugReplayCapabilityEvent>,
    result_bytes: usize,
    aborted: Option<DebugReplayError>,
}

impl DebugReplayRecorder {
    /// Start a trusted deterministic-debug transcript. Recording is deliberately
    /// unavailable for Secure-profile Environments even when Controller debug is
    /// enabled; Payment/Secure data must never enter general debug traces.
    pub fn start(
        controller_debug: bool,
        environment: EnvironmentId,
        task: &ExecutionTask,
    ) -> Result<Option<Self>, DebugReplayError> {
        if !controller_debug || !is_general_environment(environment) {
            return Ok(None);
        }
        let provenance = task.provenance.as_ref().ok_or_else(|| {
            replay_error(
                "DEBUG_REPLAY_PROVENANCE_MISSING",
                "debug replay recording requires Controller-stamped execution provenance",
            )
        })?;
        let environment_label = environment.to_string();
        if task.environment != environment_label || provenance.environment != environment_label {
            return Err(replay_error(
                "DEBUG_REPLAY_PROVENANCE_MISMATCH",
                "debug replay Environment does not match the execution provenance",
            ));
        }
        if task.payload.len() > MAX_DEBUG_REPLAY_INPUT_BYTES {
            return Err(replay_error(
                "DEBUG_REPLAY_INPUT_TOO_LARGE",
                "execution input exceeds the bounded debug replay trace limit",
            ));
        }
        Ok(Some(Self {
            created_unix_ms: now_unix_ms(),
            execution_id: task.id.to_string(),
            artifact_hash: task.artifact_hash.clone(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: environment_label,
            generation: provenance.generation,
            input_hex: hex::encode(&task.payload),
            input_sha256: value_sha256(&task.payload),
            capability_events: Vec::new(),
            result_bytes: 0,
            aborted: None,
        }))
    }

    /// Record one already-authorized capability result in exact call order. If
    /// any bound is exceeded the recorder permanently aborts: it never persists
    /// a truncated transcript and labels it replayable.
    pub fn record_capability(
        &mut self,
        call: &WorkerCapabilityCall,
        result: &WorkerCapabilityResult,
    ) -> Result<(), DebugReplayError> {
        self.ensure_active()?;
        if self.capability_events.len() >= MAX_DEBUG_REPLAY_EVENTS {
            return self.abort(
                "DEBUG_REPLAY_EVENT_LIMIT",
                "debug replay capability event limit was exceeded",
            );
        }
        if result_call_id(result) != call.call_id {
            return self.abort(
                "DEBUG_REPLAY_RESULT_ID_MISMATCH",
                "capability result call identity did not match the recorded request",
            );
        }

        let recorded_result = match result {
            WorkerCapabilityResult::Success { payload, .. } => {
                if payload.len() > MAX_DEBUG_REPLAY_RESULT_BYTES
                    || self.result_bytes.saturating_add(payload.len())
                        > MAX_DEBUG_REPLAY_TOTAL_RESULT_BYTES
                {
                    return self.abort(
                        "DEBUG_REPLAY_RESULT_LIMIT",
                        "capability result exceeds the bounded debug replay trace limit",
                    );
                }
                self.result_bytes = self.result_bytes.saturating_add(payload.len());
                DebugReplayCapabilityResult::Success {
                    payload_hex: hex::encode(payload),
                    payload_sha256: value_sha256(payload),
                }
            }
            WorkerCapabilityResult::Error { code, message, .. } => {
                let error_bytes = code.len().saturating_add(message.len());
                if error_bytes > MAX_DEBUG_REPLAY_ERROR_BYTES
                    || self.result_bytes.saturating_add(error_bytes)
                        > MAX_DEBUG_REPLAY_TOTAL_RESULT_BYTES
                {
                    return self.abort(
                        "DEBUG_REPLAY_RESULT_LIMIT",
                        "capability error exceeds the bounded debug replay trace limit",
                    );
                }
                self.result_bytes = self.result_bytes.saturating_add(error_bytes);
                DebugReplayCapabilityResult::Error {
                    code: code.clone(),
                    message: message.clone(),
                    message_sha256: value_sha256(message.as_bytes()),
                }
            }
        };

        self.capability_events.push(DebugReplayCapabilityEvent {
            call_id: call.call_id,
            kind: call.kind,
            target: call.target.clone(),
            operation: call.operation.clone(),
            request_bytes: call.payload.len(),
            request_sha256: value_sha256(&call.payload),
            result: recorded_result,
        });
        Ok(())
    }

    pub fn finish_success(
        self,
        output: &[u8],
        store: &DebugReplayStore,
    ) -> Result<PathBuf, DebugReplayError> {
        self.finish(
            DebugReplayTerminal::Success {
                output_bytes: output.len(),
                output_sha256: value_sha256(output),
            },
            store,
        )
    }

    pub fn finish_error(
        self,
        code: impl Into<String>,
        message: &str,
        store: &DebugReplayStore,
    ) -> Result<PathBuf, DebugReplayError> {
        self.finish(
            DebugReplayTerminal::Error {
                code: code.into(),
                message_sha256: value_sha256(message.as_bytes()),
            },
            store,
        )
    }

    fn finish(
        self,
        terminal: DebugReplayTerminal,
        store: &DebugReplayStore,
    ) -> Result<PathBuf, DebugReplayError> {
        if let Some(error) = self.aborted {
            return Err(error);
        }
        let trace = DebugReplayTrace {
            version: DEBUG_REPLAY_TRACE_VERSION,
            created_unix_ms: self.created_unix_ms,
            execution_id: self.execution_id,
            artifact_hash: self.artifact_hash,
            runtime_image: self.runtime_image,
            source_id: self.source_id,
            capability_abi: self.capability_abi,
            environment: self.environment,
            generation: self.generation,
            input_hex: self.input_hex,
            input_sha256: self.input_sha256,
            capability_events: self.capability_events,
            terminal,
        };
        store.persist(&trace)
    }

    fn ensure_active(&self) -> Result<(), DebugReplayError> {
        match &self.aborted {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn abort<T>(&mut self, code: &'static str, message: &str) -> Result<T, DebugReplayError> {
        let error = replay_error(code, message);
        self.aborted = Some(error.clone());
        Err(error)
    }
}

#[derive(Clone)]
pub struct DebugReplayStore {
    io: AtomicIo,
    root: PathBuf,
}

impl Default for DebugReplayStore {
    fn default() -> Self {
        Self::new(runtime_paths::binary_dir().join("data/container-runtime/debug-replay"))
    }
}

impl DebugReplayStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            io: AtomicIo::new(),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_verified(&self, path: &Path) -> Result<DebugReplayTrace, DebugReplayError> {
        let bytes = self.io.read(path).map_err(|_| {
            replay_error(
                "DEBUG_REPLAY_READ_FAILED",
                "debug replay trace could not be read",
            )
        })?;
        if bytes.len() > MAX_DEBUG_REPLAY_TRACE_BYTES {
            return Err(replay_error(
                "DEBUG_REPLAY_TRACE_TOO_LARGE",
                "debug replay trace exceeds the format size limit",
            ));
        }
        let envelope: DebugReplayEnvelope = serde_json::from_slice(&bytes).map_err(|_| {
            replay_error(
                "DEBUG_REPLAY_TRACE_INVALID",
                "debug replay trace envelope is malformed",
            )
        })?;
        validate_trace(&envelope.payload)?;
        let payload_bytes = encode_trace(&envelope.payload)?;
        if trace_sha256(&payload_bytes) != envelope.sha256 {
            return Err(replay_error(
                "DEBUG_REPLAY_INTEGRITY_MISMATCH",
                "debug replay trace integrity hash did not match its payload",
            ));
        }
        Ok(envelope.payload)
    }

    fn persist(&self, trace: &DebugReplayTrace) -> Result<PathBuf, DebugReplayError> {
        validate_trace(trace)?;
        fs::create_dir_all(&self.root).map_err(|_| {
            replay_error(
                "DEBUG_REPLAY_WRITE_FAILED",
                "debug replay trace directory could not be created",
            )
        })?;
        let path = self.root.join(format!(
            "{}.json",
            value_sha256(trace.execution_id.as_bytes())
        ));
        if !path.exists() && trace_file_count(&self.root) >= MAX_DEBUG_REPLAY_FILES {
            return Err(replay_error(
                "DEBUG_REPLAY_RETENTION_FULL",
                "debug replay retention is full; no partial eviction was performed",
            ));
        }
        let payload_bytes = encode_trace(trace)?;
        let envelope = DebugReplayEnvelope {
            payload: trace.clone(),
            sha256: trace_sha256(&payload_bytes),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|_| {
            replay_error(
                "DEBUG_REPLAY_TRACE_INVALID",
                "debug replay trace envelope could not be encoded",
            )
        })?;
        if bytes.len() > MAX_DEBUG_REPLAY_TRACE_BYTES {
            return Err(replay_error(
                "DEBUG_REPLAY_TRACE_TOO_LARGE",
                "debug replay trace exceeds the format size limit",
            ));
        }
        self.io.write_atomic(&path, &bytes).map_err(|_| {
            replay_error(
                "DEBUG_REPLAY_WRITE_FAILED",
                "debug replay trace could not be written atomically",
            )
        })?;
        Ok(path)
    }
}

fn validate_trace(trace: &DebugReplayTrace) -> Result<(), DebugReplayError> {
    if trace.version != DEBUG_REPLAY_TRACE_VERSION {
        return Err(replay_error(
            "DEBUG_REPLAY_VERSION_UNSUPPORTED",
            "debug replay trace version is unsupported",
        ));
    }
    if !is_general_environment_label(&trace.environment) {
        return Err(replay_error(
            "DEBUG_REPLAY_SECURE_FORBIDDEN",
            "debug replay traces are forbidden for Secure-profile Environments",
        ));
    }
    if trace.capability_events.len() > MAX_DEBUG_REPLAY_EVENTS
        || trace.input_hex.len() > MAX_DEBUG_REPLAY_INPUT_BYTES.saturating_mul(2)
    {
        return Err(replay_error(
            "DEBUG_REPLAY_TRACE_INVALID",
            "debug replay trace exceeds structural bounds",
        ));
    }
    let input = hex::decode(&trace.input_hex).map_err(|_| {
        replay_error(
            "DEBUG_REPLAY_TRACE_INVALID",
            "debug replay input is not valid hexadecimal",
        )
    })?;
    if value_sha256(&input) != trace.input_sha256 {
        return Err(replay_error(
            "DEBUG_REPLAY_INTEGRITY_MISMATCH",
            "debug replay input hash did not match its bytes",
        ));
    }

    let mut result_bytes = 0usize;
    for event in &trace.capability_events {
        match &event.result {
            DebugReplayCapabilityResult::Success {
                payload_hex,
                payload_sha256,
            } => {
                let payload = hex::decode(payload_hex).map_err(|_| {
                    replay_error(
                        "DEBUG_REPLAY_TRACE_INVALID",
                        "debug replay capability result is not valid hexadecimal",
                    )
                })?;
                if payload.len() > MAX_DEBUG_REPLAY_RESULT_BYTES
                    || value_sha256(&payload) != *payload_sha256
                {
                    return Err(replay_error(
                        "DEBUG_REPLAY_INTEGRITY_MISMATCH",
                        "debug replay capability result failed integrity validation",
                    ));
                }
                result_bytes = result_bytes.saturating_add(payload.len());
            }
            DebugReplayCapabilityResult::Error {
                code,
                message,
                message_sha256,
            } => {
                let bytes = code.len().saturating_add(message.len());
                if bytes > MAX_DEBUG_REPLAY_ERROR_BYTES
                    || value_sha256(message.as_bytes()) != *message_sha256
                {
                    return Err(replay_error(
                        "DEBUG_REPLAY_INTEGRITY_MISMATCH",
                        "debug replay capability error failed integrity validation",
                    ));
                }
                result_bytes = result_bytes.saturating_add(bytes);
            }
        }
        if result_bytes > MAX_DEBUG_REPLAY_TOTAL_RESULT_BYTES {
            return Err(replay_error(
                "DEBUG_REPLAY_RESULT_LIMIT",
                "debug replay capability results exceed the cumulative trace limit",
            ));
        }
    }
    Ok(())
}

fn encode_trace(trace: &DebugReplayTrace) -> Result<Vec<u8>, DebugReplayError> {
    serde_json::to_vec(trace).map_err(|_| {
        replay_error(
            "DEBUG_REPLAY_TRACE_INVALID",
            "debug replay trace payload could not be encoded",
        )
    })
}

fn result_call_id(result: &WorkerCapabilityResult) -> u64 {
    match result {
        WorkerCapabilityResult::Success { call_id, .. }
        | WorkerCapabilityResult::Error { call_id, .. } => *call_id,
    }
}

fn is_general_environment(environment: EnvironmentId) -> bool {
    matches!(
        environment,
        EnvironmentId::General1
            | EnvironmentId::General2
            | EnvironmentId::General3
            | EnvironmentId::General4
            | EnvironmentId::General5
    )
}

fn is_general_environment_label(value: &str) -> bool {
    matches!(
        value,
        "general-1" | "general-2" | "general-3" | "general-4" | "general-5"
    )
}

fn trace_file_count(root: &Path) -> usize {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("json")
                })
                .count()
        })
        .unwrap_or(0)
}

fn value_sha256(value: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(VALUE_HASH_DOMAIN);
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
    hex::encode(hash.finalize())
}

fn trace_sha256(payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(TRACE_HASH_DOMAIN);
    hash.update((payload.len() as u64).to_be_bytes());
    hash.update(payload);
    hex::encode(hash.finalize())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn replay_error(code: &'static str, message: &str) -> DebugReplayError {
    DebugReplayError {
        code,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionId, ExecutionProvenance, WorkCost};
    use ipc_protocol::CAPABILITY_ABI_VERSION;
    use resource_limits::ResourceLimits;
    use sandbox_primitives::SandboxPolicy;

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-debug-replay-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn task(environment: EnvironmentId, sequence: u64, input: Vec<u8>) -> ExecutionTask {
        let environment_label = environment.to_string();
        ExecutionTask {
            id: ExecutionId::from_parts(1, sequence),
            environment: environment_label.clone(),
            provenance: Some(ExecutionProvenance {
                runtime_image: "ab".repeat(32),
                source_id: "route:debug-replay".into(),
                capability_abi: CAPABILITY_ABI_VERSION,
                environment: environment_label,
                generation: 7,
            }),
            artifact_hash: "cd".repeat(32),
            declared_cost: WorkCost::default(),
            limits: ResourceLimits::default(),
            sandbox: SandboxPolicy::default(),
            work_ms: 0,
            payload: input,
        }
    }

    #[test]
    fn secure_profile_is_never_recorded_even_in_debug() {
        let payment = task(EnvironmentId::Payment, 1, b"secret".to_vec());
        assert!(DebugReplayRecorder::start(true, EnvironmentId::Payment, &payment)
            .unwrap()
            .is_none());
        let general = task(EnvironmentId::General1, 2, Vec::new());
        assert!(DebugReplayRecorder::start(false, EnvironmentId::General1, &general)
            .unwrap()
            .is_none());
    }

    #[test]
    fn trace_round_trip_preserves_authorized_capability_result() {
        let root = temp_root("roundtrip");
        let store = DebugReplayStore::new(root.clone());
        let task = task(EnvironmentId::General1, 3, b"hello".to_vec());
        let mut recorder = DebugReplayRecorder::start(true, EnvironmentId::General1, &task)
            .unwrap()
            .unwrap();
        let call = WorkerCapabilityCall {
            call_id: 9,
            kind: CapabilityKind::Service,
            target: "service:uac".into(),
            operation: "get_user".into(),
            payload: br#"["kate"]"#.to_vec(),
        };
        let result = WorkerCapabilityResult::Success {
            call_id: 9,
            payload: vec![0, 1, 2, 255],
        };
        recorder.record_capability(&call, &result).unwrap();
        let path = recorder.finish_success(b"done", &store).unwrap();
        let loaded = store.load_verified(&path).unwrap();
        assert_eq!(loaded.execution_id, task.id.to_string());
        assert_eq!(loaded.input_hex, hex::encode(b"hello"));
        assert_eq!(loaded.capability_events.len(), 1);
        assert_eq!(loaded.capability_events[0].call_id, 9);
        match &loaded.capability_events[0].result {
            DebugReplayCapabilityResult::Success { payload_hex, .. } => {
                assert_eq!(payload_hex, "000102ff");
            }
            other => panic!("unexpected replay result: {other:?}"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_trace_fails_integrity_validation() {
        let root = temp_root("tamper");
        let store = DebugReplayStore::new(root.clone());
        let task = task(EnvironmentId::General2, 4, b"input".to_vec());
        let recorder = DebugReplayRecorder::start(true, EnvironmentId::General2, &task)
            .unwrap()
            .unwrap();
        let path = recorder.finish_success(b"output", &store).unwrap();
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        envelope["payload"]["source_id"] = serde_json::json!("route:tampered");
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let error = store.load_verified(&path).unwrap_err();
        assert_eq!(error.code, "DEBUG_REPLAY_INTEGRITY_MISMATCH");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn overflow_aborts_and_cannot_persist_a_partial_trace() {
        let root = temp_root("overflow");
        let store = DebugReplayStore::new(root.clone());
        let task = task(EnvironmentId::General3, 5, Vec::new());
        let mut recorder = DebugReplayRecorder::start(true, EnvironmentId::General3, &task)
            .unwrap()
            .unwrap();
        let call = WorkerCapabilityCall {
            call_id: 1,
            kind: CapabilityKind::Network,
            target: "net.fetch".into(),
            operation: "fetch".into(),
            payload: Vec::new(),
        };
        let result = WorkerCapabilityResult::Success {
            call_id: 1,
            payload: vec![0; MAX_DEBUG_REPLAY_RESULT_BYTES + 1],
        };
        assert_eq!(
            recorder.record_capability(&call, &result).unwrap_err().code,
            "DEBUG_REPLAY_RESULT_LIMIT"
        );
        assert_eq!(
            recorder.finish_success(b"ignored", &store).unwrap_err().code,
            "DEBUG_REPLAY_RESULT_LIMIT"
        );
        assert!(!root.exists() || trace_file_count(&root) == 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_refuses_new_trace_instead_of_unbounded_growth() {
        let root = temp_root("retention");
        let store = DebugReplayStore::new(root.clone());
        for sequence in 0..MAX_DEBUG_REPLAY_FILES {
            let task = task(EnvironmentId::General4, sequence as u64 + 10, Vec::new());
            DebugReplayRecorder::start(true, EnvironmentId::General4, &task)
                .unwrap()
                .unwrap()
                .finish_success(&[], &store)
                .unwrap();
        }
        let task = task(EnvironmentId::General4, 999, Vec::new());
        let error = DebugReplayRecorder::start(true, EnvironmentId::General4, &task)
            .unwrap()
            .unwrap()
            .finish_success(&[], &store)
            .unwrap_err();
        assert_eq!(error.code, "DEBUG_REPLAY_RETENTION_FULL");
        assert_eq!(trace_file_count(&root), MAX_DEBUG_REPLAY_FILES);
        let _ = fs::remove_dir_all(root);
    }
}
