from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:140]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# IPC v4: executions can be awaited without conflating queue acceptance with a
# completed result. The wait is bounded and explicitly reports pending state.
# ---------------------------------------------------------------------------
protocol = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
text = protocol.read_text(encoding="utf-8")
text = text.replace("pub const PROTOCOL_VERSION: u16 = 3;", "pub const PROTOCOL_VERSION: u16 = 4;", 1)
text = text.replace(
    "pub const MAX_EXECUTION_INPUT_BYTES: usize = 2 * 1024 * 1024;",
    "pub const MAX_EXECUTION_INPUT_BYTES: usize = 2 * 1024 * 1024;\n"
    "pub const MAX_EXECUTION_OUTPUT_BYTES: usize = 2 * 1024 * 1024;\n"
    "pub const MAX_AWAIT_RESULT_MS: u64 = 30_000;",
    1,
)
anchor = """#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
"""
insert = """#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaitResultRequest {
    pub request_id: String,
    pub auth_token: String,
    pub execution_id: String,
    pub timeout_ms: u64,
}

"""
if anchor not in text:
    raise SystemExit("AwaitResult struct anchor missing")
text = text.replace(anchor, insert + anchor, 1)
text = text.replace(
    "    Execute(ExecuteRequest),\n    Cancel(CancelRequest),",
    "    Execute(ExecuteRequest),\n    AwaitResult(AwaitResultRequest),\n    Cancel(CancelRequest),",
    1,
)
anchor = """    Accepted {
        request_id: String,
        execution_id: String,
    },
    Cancelled {
"""
replacement = """    Accepted {
        request_id: String,
        execution_id: String,
    },
    ExecutionFinished {
        request_id: String,
        execution_id: String,
        output: Vec<u8>,
        elapsed_ms: u64,
    },
    ExecutionFailed {
        request_id: String,
        execution_id: String,
        code: String,
        message: String,
        elapsed_ms: u64,
    },
    ExecutionPending {
        request_id: String,
        execution_id: String,
    },
    Cancelled {
"""
if anchor not in text:
    raise SystemExit("Response Accepted anchor missing")
text = text.replace(anchor, replacement, 1)
marker = "\n    #[test]\n    fn rejects_zero_length_frame() {"
insert_test = r'''

    #[test]
    fn await_result_round_trip_preserves_execution_identity_and_timeout() {
        let request = Request::AwaitResult(AwaitResultRequest {
            request_id: "wait-1".into(),
            auth_token: "secret".into(),
            execution_id: "exec-0000000000000001-0000000000000002".into(),
            timeout_ms: 250,
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::AwaitResult(decoded) = decoded else {
            panic!("expected await-result request");
        };
        assert_eq!(decoded.timeout_ms, 250);
        assert!(decoded.execution_id.starts_with("exec-"));
    }
'''
if marker not in text:
    raise SystemExit("IPC await test insertion anchor missing")
text = text.replace(marker, insert_test + marker, 1)
protocol.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Runtime execution outcome type.
# ---------------------------------------------------------------------------
execution = Path("container-runtime/crates/container-runtime-core/src/execution.rs")
text = execution.read_text(encoding="utf-8")
anchor = """#[derive(Debug, Clone)]
pub struct ExecutionRecord {
"""
insert = """#[derive(Debug, Clone)]
pub struct ExecutionOutcome {
    pub output: Vec<u8>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub cancelled: bool,
}

"""
if anchor not in text:
    raise SystemExit("ExecutionOutcome insertion anchor missing")
text = text.replace(anchor, insert + anchor, 1)
execution.write_text(text, encoding="utf-8")

lib = Path("container-runtime/crates/container-runtime-core/src/lib.rs")
text = lib.read_text(encoding="utf-8")
text = text.replace(
    "pub use execution::{ExecutionId, ExecutionRecord, ExecutionState, ExecutionTask, WorkCost};",
    "pub use execution::{\n"
    "    ExecutionId, ExecutionOutcome, ExecutionRecord, ExecutionState, ExecutionTask, WorkCost,\n"
    "};",
    1,
)
lib.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Runtime result table: bounded by count and retained bytes. Results are cloned
# on read so a transient IPC write failure cannot destroy the only result copy.
# ---------------------------------------------------------------------------
runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
text = runtime.read_text(encoding="utf-8")
text = text.replace(
    "use crate::execution::{ExecutionId, ExecutionTask, WorkCost};",
    "use crate::execution::{ExecutionId, ExecutionOutcome, ExecutionTask, WorkCost};",
    1,
)
text = text.replace(
    "const JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;",
    "const JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;\n"
    "const RESULT_STORE_MAX_RECORDS: usize = 1024;\n"
    "const RESULT_STORE_MAX_BYTES: usize = 16 * 1024 * 1024;",
    1,
)
anchor = """fn journal_path() -> PathBuf {
"""
insert = """#[derive(Default)]
struct ResultStore {
    outcomes: HashMap<String, ExecutionOutcome>,
    order: VecDeque<String>,
    retained_bytes: usize,
}

type SharedResults = Arc<(Mutex<ResultStore>, Condvar)>;

fn outcome_retained_bytes(outcome: &ExecutionOutcome) -> usize {
    outcome
        .output
        .len()
        .saturating_add(outcome.error.as_ref().map_or(0, String::len))
        .saturating_add(64)
}

fn record_execution_outcome(results: &SharedResults, id: &str, outcome: ExecutionOutcome) {
    let (lock, changed) = &**results;
    let mut store = lock.lock().expect("execution result store poisoned");
    if let Some(previous) = store.outcomes.remove(id) {
        store.retained_bytes = store
            .retained_bytes
            .saturating_sub(outcome_retained_bytes(&previous));
        store.order.retain(|candidate| candidate != id);
    }
    store.retained_bytes = store
        .retained_bytes
        .saturating_add(outcome_retained_bytes(&outcome));
    store.outcomes.insert(id.to_string(), outcome);
    store.order.push_back(id.to_string());

    while store.outcomes.len() > RESULT_STORE_MAX_RECORDS
        || store.retained_bytes > RESULT_STORE_MAX_BYTES
    {
        let Some(oldest) = store.order.pop_front() else {
            break;
        };
        if let Some(removed) = store.outcomes.remove(&oldest) {
            store.retained_bytes = store
                .retained_bytes
                .saturating_sub(outcome_retained_bytes(&removed));
        }
    }
    drop(store);
    changed.notify_all();
}

"""
if anchor not in text:
    raise SystemExit("result store insertion anchor missing")
text = text.replace(anchor, insert + anchor, 1)
text = text.replace(
    "    journal: Arc<Journal>,\n}",
    "    journal: Arc<Journal>,\n    results: SharedResults,\n}",
    1,
)
text = text.replace(
    "        let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));\n\n        let runner: Runner = {",
    "        let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));\n"
    "        let results: SharedResults = Arc::new((Mutex::new(ResultStore::default()), Condvar::new()));\n\n"
    "        let runner: Runner = {",
    1,
)
old = """        let completion: Completion = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            let journal = Arc::clone(&journal);
            Arc::new(move |task, elapsed_ms, result| {
                let succeeded = result.is_ok();
                let was_cancelled = cancelled
                    .lock()
                    .expect("cancel table poisoned")
                    .remove(&task.id.to_string());
                if succeeded && !was_cancelled {
                    cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                }
                journal.append(JournalEvent {
"""
new = """        let completion: Completion = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            let journal = Arc::clone(&journal);
            let results = Arc::clone(&results);
            Arc::new(move |task, elapsed_ms, result| {
                let succeeded = result.is_ok();
                let was_cancelled = cancelled
                    .lock()
                    .expect("cancel table poisoned")
                    .remove(&task.id.to_string());
                if succeeded && !was_cancelled {
                    cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                }
                let outcome = if was_cancelled {
                    ExecutionOutcome {
                        output: Vec::new(),
                        error: Some("execution cancelled".into()),
                        elapsed_ms,
                        cancelled: true,
                    }
                } else {
                    ExecutionOutcome {
                        output: Vec::new(),
                        error: result.as_ref().err().cloned(),
                        elapsed_ms,
                        cancelled: false,
                    }
                };
                record_execution_outcome(&results, &task.id.to_string(), outcome);
                journal.append(JournalEvent {
"""
if old not in text:
    raise SystemExit("runtime completion anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    "            journal,\n        });",
    "            journal,\n            results,\n        });",
    1,
)
old = """        if removed_queued || running {
            self.journal.append_cancel_string(execution_id);
            true
        } else {
            false
        }
    }

    pub fn restart_environment(&self, id: EnvironmentId) -> usize {
"""
new = """        if removed_queued || running {
            self.journal.append_cancel_string(execution_id);
            if removed_queued && !running {
                record_execution_outcome(
                    &self.results,
                    execution_id,
                    ExecutionOutcome {
                        output: Vec::new(),
                        error: Some("execution cancelled before start".into()),
                        elapsed_ms: 0,
                        cancelled: true,
                    },
                );
            }
            true
        } else {
            false
        }
    }

    pub fn wait_for_result(
        &self,
        execution_id: &str,
        timeout: Duration,
    ) -> Option<ExecutionOutcome> {
        let timeout = timeout.max(Duration::from_millis(1));
        let started = Instant::now();
        let (lock, changed) = &*self.results;
        let mut store = lock.lock().expect("execution result store poisoned");
        loop {
            if let Some(outcome) = store.outcomes.get(execution_id) {
                return Some(outcome.clone());
            }
            let remaining = timeout.checked_sub(started.elapsed())?;
            let (next, wait) = changed
                .wait_timeout(store, remaining)
                .expect("execution result store poisoned");
            store = next;
            if wait.timed_out() {
                return store.outcomes.get(execution_id).cloned();
            }
        }
    }

    pub fn restart_environment(&self, id: EnvironmentId) -> usize {
"""
if old not in text:
    raise SystemExit("runtime cancel/result wait anchor missing")
text = text.replace(old, new, 1)
runtime.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Container control plane AwaitResult branch.
# ---------------------------------------------------------------------------
container = Path("container-runtime/crates/container-bin/src/main.rs")
text = container.read_text(encoding="utf-8")
old = """use ipc_protocol::{
    decode_request, read_frame, write_frame, Request, Response, MAX_ARTIFACT_BYTES,
    MAX_EXECUTION_INPUT_BYTES, PROTOCOL_VERSION,
};
"""
new = """use ipc_protocol::{
    decode_request, read_frame, write_frame, Request, Response, MAX_ARTIFACT_BYTES,
    MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,
};
"""
if old not in text:
    raise SystemExit("container IPC import anchor missing")
text = text.replace(old, new, 1)
anchor = """        Request::Health(request) => {
"""
branch = """        Request::AwaitResult(request) => {
            if request.auth_token != token {
                Response::Error {
                    request_id: Some(request.request_id),
                    code: "AUTH_FAILED".into(),
                    message: "container control authentication failed".into(),
                }
            } else {
                let timeout = Duration::from_millis(request.timeout_ms.clamp(1, MAX_AWAIT_RESULT_MS));
                match runtime.wait_for_result(&request.execution_id, timeout) {
                    Some(outcome) if outcome.cancelled => Response::ExecutionFailed {
                        request_id: request.request_id,
                        execution_id: request.execution_id,
                        code: "EXECUTION_CANCELLED".into(),
                        message: outcome
                            .error
                            .unwrap_or_else(|| "execution was cancelled".into()),
                        elapsed_ms: outcome.elapsed_ms,
                    },
                    Some(outcome) if outcome.error.is_some() => Response::ExecutionFailed {
                        request_id: request.request_id,
                        execution_id: request.execution_id,
                        code: "EXECUTION_FAILED".into(),
                        message: outcome.error.unwrap_or_else(|| "execution failed".into()),
                        elapsed_ms: outcome.elapsed_ms,
                    },
                    Some(outcome) if outcome.output.len() > MAX_EXECUTION_OUTPUT_BYTES => {
                        Response::ExecutionFailed {
                            request_id: request.request_id,
                            execution_id: request.execution_id,
                            code: "EXECUTION_OUTPUT_TOO_LARGE".into(),
                            message: "execution output exceeds the Container IPC limit".into(),
                            elapsed_ms: outcome.elapsed_ms,
                        }
                    }
                    Some(outcome) => Response::ExecutionFinished {
                        request_id: request.request_id,
                        execution_id: request.execution_id,
                        output: outcome.output,
                        elapsed_ms: outcome.elapsed_ms,
                    },
                    None => Response::ExecutionPending {
                        request_id: request.request_id,
                        execution_id: request.execution_id,
                    },
                }
            }
        }
"""
if anchor not in text:
    raise SystemExit("container AwaitResult branch anchor missing")
text = text.replace(anchor, branch + anchor, 1)
container.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Backend client wait and convenience execute-and-wait path.
# ---------------------------------------------------------------------------
client = Path("engine/crates/core/src/container_client.rs")
text = client.read_text(encoding="utf-8")
old = """use ipc_protocol::{
    decode_response, read_frame, write_frame, ExecuteRequest, HealthRequest, InspectRequest,
    PrepareRefreshRequest, RegisterArtifactRequest, Request, Response, ResumeRequest,
    WorkCost as IpcWorkCost, MAX_ARTIFACT_BYTES, MAX_EXECUTION_INPUT_BYTES,
};
"""
new = """use ipc_protocol::{
    decode_response, read_frame, write_frame, AwaitResultRequest, ExecuteRequest, HealthRequest,
    InspectRequest, PrepareRefreshRequest, RegisterArtifactRequest, Request, Response, ResumeRequest,
    WorkCost as IpcWorkCost, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES,
    MAX_EXECUTION_OUTPUT_BYTES,
};
"""
if old not in text:
    raise SystemExit("ContainerClient result import anchor missing")
text = text.replace(old, new, 1)
anchor = """    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
"""
methods = """    pub async fn await_result(
        &self,
        execution_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let timeout_ms = timeout
            .as_millis()
            .clamp(1, u128::from(MAX_AWAIT_RESULT_MS)) as u64;
        let endpoint = self
            .endpoint
            .read()
            .expect("container endpoint lock poisoned")
            .clone();
        let request = Request::AwaitResult(AwaitResultRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            execution_id: execution_id.to_string(),
            timeout_ms,
        });
        let call_timeout = Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(3));
        match call(endpoint, request, call_timeout).await? {
            Response::ExecutionFinished { output, .. } => {
                if output.len() > MAX_EXECUTION_OUTPUT_BYTES {
                    anyhow::bail!("Container returned an oversized execution result");
                }
                Ok(Some(output))
            }
            Response::ExecutionPending { .. } => Ok(None),
            Response::ExecutionFailed { code, message, .. } => {
                anyhow::bail!("container execution failed [{code}]: {message}")
            }
            Response::Error { code, message, .. } => {
                anyhow::bail!("container result wait failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected container result response: {other:?}"),
        }
    }

    pub async fn execute_and_wait(
        &self,
        environment: &str,
        artifact_hash: &str,
        input: Vec<u8>,
        declared_cost: IpcWorkCost,
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let execution_id = self
            .execute(environment, artifact_hash, input, declared_cost)
            .await?;
        match self.await_result(&execution_id, timeout).await? {
            Some(output) => Ok(output),
            None => anyhow::bail!("container execution {execution_id} is still pending after timeout"),
        }
    }

"""
if anchor not in text:
    raise SystemExit("ContainerClient result method anchor missing")
text = text.replace(anchor, methods + anchor, 1)
client.write_text(text, encoding="utf-8")

print("bounded WASM result channel applied")
