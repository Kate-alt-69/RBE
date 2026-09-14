use std::path::Path;

use ipc_protocol::{WorkerCapabilityCall, WorkerCapabilityResult};
use sha2::{Digest, Sha256};

use crate::{
    DebugReplayCapabilityResult, DebugReplayError, DebugReplayStore, DebugReplayTerminal,
    DebugReplayTrace,
};

const VALUE_HASH_DOMAIN: &[u8] = b"RBE_DEBUG_REPLAY_VALUE_V1\0";

pub struct DebugReplayCursor {
    trace: DebugReplayTrace,
    next_event: usize,
    failed: Option<DebugReplayError>,
    complete: bool,
}

impl DebugReplayStore {
    /// Open a cursor only after the store has verified the trace envelope and
    /// all embedded payload/result hashes. Replay never starts from raw JSON.
    pub fn open_cursor(&self, path: &Path) -> Result<DebugReplayCursor, DebugReplayError> {
        Ok(DebugReplayCursor {
            trace: self.load_verified(path)?,
            next_event: 0,
            failed: None,
            complete: false,
        })
    }
}

impl DebugReplayCursor {
    pub fn trace(&self) -> &DebugReplayTrace {
        &self.trace
    }

    pub fn remaining_capabilities(&self) -> usize {
        self.trace
            .capability_events
            .len()
            .saturating_sub(self.next_event)
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Validate one guest capability request against the next immutable trace
    /// event and return the recorded result. No real host dispatcher is touched.
    pub fn replay_capability(
        &mut self,
        call: &WorkerCapabilityCall,
    ) -> Result<WorkerCapabilityResult, DebugReplayError> {
        self.ensure_usable()?;
        let Some(event) = self.trace.capability_events.get(self.next_event).cloned() else {
            return self.fail(
                "DEBUG_REPLAY_CALL_EXHAUSTED",
                "guest emitted a capability call after the recorded transcript ended",
            );
        };
        let matches = event.call_id == call.call_id
            && event.kind == call.kind
            && event.target == call.target
            && event.operation == call.operation
            && event.request_bytes == call.payload.len()
            && event.request_sha256 == value_sha256(&call.payload);
        if !matches {
            return self.fail(
                "DEBUG_REPLAY_CALL_MISMATCH",
                "guest capability call did not match the next recorded deterministic event",
            );
        }

        let result = match &event.result {
            DebugReplayCapabilityResult::Success { payload_hex, .. } => {
                let payload = hex::decode(payload_hex).map_err(|_| {
                    replay_error(
                        "DEBUG_REPLAY_TRACE_INVALID",
                        "verified replay capability payload could not be decoded",
                    )
                })?;
                WorkerCapabilityResult::Success {
                    call_id: event.call_id,
                    payload,
                }
            }
            DebugReplayCapabilityResult::Error { code, message, .. } => {
                WorkerCapabilityResult::Error {
                    call_id: event.call_id,
                    code: code.clone(),
                    message: message.clone(),
                }
            }
        };
        self.next_event = self.next_event.saturating_add(1);
        Ok(result)
    }

    pub fn verify_terminal_success(&mut self, output: &[u8]) -> Result<(), DebugReplayError> {
        self.ensure_usable()?;
        self.ensure_capabilities_consumed()?;
        match self.trace.terminal.clone() {
            DebugReplayTerminal::Success {
                output_bytes,
                output_sha256,
            } if output_bytes == output.len() && output_sha256 == value_sha256(output) => {
                self.complete = true;
                Ok(())
            }
            _ => self.fail(
                "DEBUG_REPLAY_TERMINAL_MISMATCH",
                "guest terminal success did not match the recorded deterministic result",
            ),
        }
    }

    pub fn verify_terminal_error(
        &mut self,
        code: &str,
        message: &str,
    ) -> Result<(), DebugReplayError> {
        self.ensure_usable()?;
        self.ensure_capabilities_consumed()?;
        match self.trace.terminal.clone() {
            DebugReplayTerminal::Error {
                code: recorded_code,
                message_sha256,
            } if recorded_code == code && message_sha256 == value_sha256(message.as_bytes()) => {
                self.complete = true;
                Ok(())
            }
            _ => self.fail(
                "DEBUG_REPLAY_TERMINAL_MISMATCH",
                "guest terminal error did not match the recorded deterministic result",
            ),
        }
    }

    fn ensure_usable(&self) -> Result<(), DebugReplayError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if self.complete {
            return Err(replay_error(
                "DEBUG_REPLAY_CURSOR_COMPLETE",
                "deterministic replay cursor has already completed",
            ));
        }
        Ok(())
    }

    fn ensure_capabilities_consumed(&mut self) -> Result<(), DebugReplayError> {
        if self.next_event != self.trace.capability_events.len() {
            return self.fail(
                "DEBUG_REPLAY_EVENTS_REMAIN",
                "guest terminated before consuming every recorded capability event",
            );
        }
        Ok(())
    }

    fn fail<T>(&mut self, code: &'static str, message: &str) -> Result<T, DebugReplayError> {
        let error = replay_error(code, message);
        self.failed = Some(error.clone());
        Err(error)
    }
}

fn value_sha256(value: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(VALUE_HASH_DOMAIN);
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
    hex::encode(hash.finalize())
}

fn replay_error(code: &'static str, message: &str) -> DebugReplayError {
    DebugReplayError {
        code,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use environments::EnvironmentId;
    use ipc_protocol::{CapabilityKind, CAPABILITY_ABI_VERSION};
    use resource_limits::ResourceLimits;
    use sandbox_primitives::SandboxPolicy;

    use super::*;
    use crate::{
        DebugReplayRecorder, ExecutionId, ExecutionProvenance, ExecutionTask, WorkCost,
    };

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-debug-replay-cursor-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn task(sequence: u64) -> ExecutionTask {
        ExecutionTask {
            id: ExecutionId::from_parts(1, sequence),
            environment: EnvironmentId::General1.to_string(),
            provenance: Some(ExecutionProvenance {
                runtime_image: "ab".repeat(32),
                source_id: "route:cursor".into(),
                capability_abi: CAPABILITY_ABI_VERSION,
                environment: EnvironmentId::General1.to_string(),
                generation: 4,
            }),
            artifact_hash: "cd".repeat(32),
            declared_cost: WorkCost::default(),
            limits: ResourceLimits::default(),
            sandbox: SandboxPolicy::default(),
            work_ms: 0,
            payload: b"request".to_vec(),
        }
    }

    fn recorded_trace(
        name: &str,
    ) -> (PathBuf, DebugReplayStore, WorkerCapabilityCall, PathBuf) {
        let root = temp_root(name);
        let store = DebugReplayStore::new(root.clone());
        let task = task(10);
        let call = WorkerCapabilityCall {
            call_id: 77,
            kind: CapabilityKind::Service,
            target: "service:uac".into(),
            operation: "get_user".into(),
            payload: b"kate".to_vec(),
        };
        let result = WorkerCapabilityResult::Success {
            call_id: 77,
            payload: b"user-42".to_vec(),
        };
        let mut recorder = DebugReplayRecorder::start(true, EnvironmentId::General1, &task)
            .unwrap()
            .unwrap();
        recorder.record_capability(&call, &result).unwrap();
        let path = recorder.finish_success(b"done", &store).unwrap();
        (root, store, call, path)
    }

    #[test]
    fn exact_capability_call_replays_recorded_result_without_host_dispatch() {
        let (root, store, call, path) = recorded_trace("exact");
        let mut cursor = store.open_cursor(&path).unwrap();
        match cursor.replay_capability(&call).unwrap() {
            WorkerCapabilityResult::Success { call_id, payload } => {
                assert_eq!(call_id, 77);
                assert_eq!(payload, b"user-42");
            }
            other => panic!("unexpected replay result: {other:?}"),
        }
        cursor.verify_terminal_success(b"done").unwrap();
        assert!(cursor.is_complete());
        assert_eq!(cursor.remaining_capabilities(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn capability_mismatch_poison_cursor_permanently() {
        let (root, store, mut call, path) = recorded_trace("mismatch");
        let mut cursor = store.open_cursor(&path).unwrap();
        call.payload.push(b'!');
        let first = cursor.replay_capability(&call).unwrap_err();
        assert_eq!(first.code, "DEBUG_REPLAY_CALL_MISMATCH");
        call.payload.pop();
        let second = cursor.replay_capability(&call).unwrap_err();
        assert_eq!(second.code, "DEBUG_REPLAY_CALL_MISMATCH");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn terminal_result_cannot_skip_recorded_capability_events() {
        let (root, store, _, path) = recorded_trace("terminal");
        let mut cursor = store.open_cursor(&path).unwrap();
        let error = cursor.verify_terminal_success(b"done").unwrap_err();
        assert_eq!(error.code, "DEBUG_REPLAY_EVENTS_REMAIN");
        assert!(!cursor.is_complete());
        let _ = fs::remove_dir_all(root);
    }
}
