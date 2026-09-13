from pathlib import Path


def one(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


def section(text: str, start: str, end: str, new: str, label: str) -> str:
    if text.count(start) != 1:
        raise SystemExit(f"{label}: start marker count={text.count(start)}")
    begin = text.index(start)
    finish = text.find(end, begin)
    if finish < 0:
        raise SystemExit(f"{label}: end marker missing")
    return text[:begin] + new + text[finish:]


env = Path("container-runtime/crates/container-bin/src/environment_process.rs")
text = env.read_text(encoding="utf-8")
text = one(
    text,
    '''use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};''',
    '''use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::process::{Child, ChildStdin, Command, Stdio};''',
    "worker guard imports",
)
text = one(
    text,
    '''const PENDING_CANCEL_TTL: Duration = Duration::from_secs(30);''',
    '''const PENDING_CANCEL_TTL: Duration = Duration::from_secs(30);
const WORKER_CRASH_THRESHOLD: u32 = 3;
const WORKER_CRASH_WINDOW: Duration = Duration::from_secs(30);
const WORKER_CRASH_COOLDOWN: Duration = Duration::from_secs(60);''',
    "crash circuit constants",
)
text = one(
    text,
    '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}

struct WorkerExecution<'a> {''',
    '''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitPermit {
    Normal,
    HalfOpenProbe,
}

#[derive(Debug, Clone)]
struct ArtifactCrashState {
    crashes: u32,
    window_started: Instant,
    open_until: Option<Instant>,
    probe_in_flight: bool,
}

#[derive(Default)]
struct ArtifactCrashCircuit {
    states: Mutex<HashMap<String, ArtifactCrashState>>,
}

#[derive(Debug, Clone)]
pub struct ArtifactCrashCircuitSnapshot {
    pub artifact_hash: String,
    pub consecutive_crashes: u32,
    pub open_remaining_ms: u64,
    pub half_open_probe: bool,
}

impl ArtifactCrashCircuit {
    fn acquire(&self, artifact_hash: &str) -> Result<CircuitPermit, u64> {
        self.acquire_at(artifact_hash, Instant::now())
    }

    fn acquire_at(&self, artifact_hash: &str, now: Instant) -> Result<CircuitPermit, u64> {
        let mut states = self.states.lock().expect("artifact crash circuit poisoned");
        let Some(state) = states.get_mut(artifact_hash) else {
            return Ok(CircuitPermit::Normal);
        };
        let Some(open_until) = state.open_until else {
            return Ok(CircuitPermit::Normal);
        };
        if now < open_until {
            return Err(duration_millis(open_until.duration_since(now)).max(1));
        }
        if state.probe_in_flight {
            return Err(1);
        }
        state.probe_in_flight = true;
        Ok(CircuitPermit::HalfOpenProbe)
    }

    fn record_crash(&self, artifact_hash: &str, permit: CircuitPermit) {
        self.record_crash_at(artifact_hash, permit, Instant::now());
    }

    fn record_crash_at(&self, artifact_hash: &str, permit: CircuitPermit, now: Instant) {
        let mut states = self.states.lock().expect("artifact crash circuit poisoned");
        let state = states
            .entry(artifact_hash.to_string())
            .or_insert(ArtifactCrashState {
                crashes: 0,
                window_started: now,
                open_until: None,
                probe_in_flight: false,
            });
        if permit == CircuitPermit::HalfOpenProbe {
            state.crashes = WORKER_CRASH_THRESHOLD;
            state.window_started = now;
            state.open_until = Some(now + WORKER_CRASH_COOLDOWN);
            state.probe_in_flight = false;
            return;
        }
        if now.duration_since(state.window_started) > WORKER_CRASH_WINDOW {
            state.crashes = 0;
            state.window_started = now;
        }
        state.crashes = state.crashes.saturating_add(1);
        if state.crashes >= WORKER_CRASH_THRESHOLD {
            state.open_until = Some(now + WORKER_CRASH_COOLDOWN);
            state.probe_in_flight = false;
        }
    }

    fn record_non_crash(&self, artifact_hash: &str) {
        self.states
            .lock()
            .expect("artifact crash circuit poisoned")
            .remove(artifact_hash);
    }

    fn abort(&self, artifact_hash: &str, permit: CircuitPermit) {
        if permit != CircuitPermit::HalfOpenProbe {
            return;
        }
        let now = Instant::now();
        if let Some(state) = self
            .states
            .lock()
            .expect("artifact crash circuit poisoned")
            .get_mut(artifact_hash)
        {
            state.probe_in_flight = false;
            state.open_until = Some(now + WORKER_CRASH_COOLDOWN);
        }
    }

    fn snapshots(&self) -> Vec<ArtifactCrashCircuitSnapshot> {
        let now = Instant::now();
        let mut snapshots = self
            .states
            .lock()
            .expect("artifact crash circuit poisoned")
            .iter()
            .map(|(artifact_hash, state)| ArtifactCrashCircuitSnapshot {
                artifact_hash: artifact_hash.clone(),
                consecutive_crashes: state.crashes,
                open_remaining_ms: state
                    .open_until
                    .and_then(|deadline| deadline.checked_duration_since(now))
                    .map(duration_millis)
                    .unwrap_or(0),
                half_open_probe: state.probe_in_flight,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.artifact_hash.cmp(&right.artifact_hash));
        snapshots
    }

    fn open_count(&self) -> usize {
        let now = Instant::now();
        self.states
            .lock()
            .expect("artifact crash circuit poisoned")
            .values()
            .filter(|state| {
                state.probe_in_flight
                    || state.open_until.is_some_and(|deadline| deadline > now)
            })
            .count()
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct WorkerExecution<'a> {''',
    "artifact crash circuit types",
)
text = one(
    text,
    '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
}''',
    '''    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    artifact_crash_circuit: ArtifactCrashCircuit,
}''',
    "supervisor circuit field",
)
text = one(
    text,
    '''            capability_broker,
            capability_dispatcher,
        });''',
    '''            capability_broker,
            capability_dispatcher,
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
        });''',
    "supervisor circuit initialization",
)
text = one(
    text,
    '''    pub fn snapshots(&self) -> Vec<EnvironmentProcessSnapshot> {
        let mut table = self
            .processes
            .lock()
            .expect("Environment process table poisoned");
        let mut snapshots = Vec::with_capacity(table.len());
        for (id, managed) in table.iter_mut() {
            let alive = matches!(managed.child.try_wait(), Ok(None));
            snapshots.push(EnvironmentProcessSnapshot {
                environment: id.to_string(),
                pid: managed.child.id(),
                generation: managed.endpoint.generation,
                address: managed.endpoint.address.to_string(),
                alive,
                debug: managed.debug,
            });
        }
        snapshots.sort_by(|a, b| a.environment.cmp(&b.environment));
        snapshots
    }

    fn execute(&self, task: &ExecutionTask) -> Result<Vec<u8>, String> {''',
    '''    pub fn snapshots(&self) -> Vec<EnvironmentProcessSnapshot> {
        let mut table = self
            .processes
            .lock()
            .expect("Environment process table poisoned");
        let mut snapshots = Vec::with_capacity(table.len());
        for (id, managed) in table.iter_mut() {
            let alive = matches!(managed.child.try_wait(), Ok(None));
            snapshots.push(EnvironmentProcessSnapshot {
                environment: id.to_string(),
                pid: managed.child.id(),
                generation: managed.endpoint.generation,
                address: managed.endpoint.address.to_string(),
                alive,
                debug: managed.debug,
            });
        }
        snapshots.sort_by(|a, b| a.environment.cmp(&b.environment));
        snapshots
    }

    pub fn artifact_crash_circuits(&self) -> Vec<ArtifactCrashCircuitSnapshot> {
        self.artifact_crash_circuit.snapshots()
    }

    pub fn open_artifact_crash_circuit_count(&self) -> usize {
        self.artifact_crash_circuit.open_count()
    }

    fn execute(&self, task: &ExecutionTask) -> Result<Vec<u8>, String> {''',
    "supervisor circuit snapshots",
)
text = one(
    text,
    '''        let request_id = task.id.to_string();
        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .insert(
                request_id.clone(),
                ExecutionOwner {
                    environment: id,
                    generation: endpoint.generation,
                },
            );
        if take_cancelled(&self.cancelled, &request_id)? {
            self.executions
                .lock()
                .map_err(|_| "Environment execution ownership table poisoned".to_string())?
                .remove(&request_id);
            return Err("execution cancelled before Environment dispatch".into());
        }
        let request = ChildRequest::Execute {''',
    '''        let request_id = task.id.to_string();
        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .insert(
                request_id.clone(),
                ExecutionOwner {
                    environment: id,
                    generation: endpoint.generation,
                },
            );
        if take_cancelled(&self.cancelled, &request_id)? {
            self.executions
                .lock()
                .map_err(|_| "Environment execution ownership table poisoned".to_string())?
                .remove(&request_id);
            return Err("execution cancelled before Environment dispatch".into());
        }
        let circuit_permit = match self.artifact_crash_circuit.acquire(&task.artifact_hash) {
            Ok(permit) => permit,
            Err(remaining_ms) => {
                self.executions
                    .lock()
                    .map_err(|_| "Environment execution ownership table poisoned".to_string())?
                    .remove(&request_id);
                return Err(format!(
                    "ARTIFACT_CRASH_CIRCUIT_OPEN: artifact {} is quarantined for approximately {remaining_ms} ms",
                    task.artifact_hash
                ));
            }
        };
        let request = ChildRequest::Execute {''',
    "circuit admission before worker dispatch",
)
text = one(
    text,
    '''        let result = (|| -> Result<Vec<u8>, String> {
            write_frame(&mut stream, &request)
                .map_err(|error| format!("send Environment execution: {error}"))?;''',
    '''        #[derive(Clone, Copy)]
        enum CircuitOutcome {
            Infrastructure,
            WorkerResponded,
            WorkerCrash,
        }
        let mut circuit_outcome = CircuitOutcome::Infrastructure;
        let result = (|| -> Result<Vec<u8>, String> {
            write_frame(&mut stream, &request)
                .map_err(|error| format!("send Environment execution: {error}"))?;''',
    "circuit outcome tracking",
)
text = one(
    text,
    '''                    ChildResponse::Finished {
                        request_id: returned,
                        output,
                    } if returned == request_id => return Ok(output),
                    ChildResponse::Error {
                        request_id: Some(returned),
                        code,
                        message,
                    } if returned == request_id => return Err(format!("{code}: {message}")),''',
    '''                    ChildResponse::Finished {
                        request_id: returned,
                        output,
                    } if returned == request_id => {
                        circuit_outcome = CircuitOutcome::WorkerResponded;
                        return Ok(output);
                    }
                    ChildResponse::Error {
                        request_id: Some(returned),
                        code,
                        message,
                    } if returned == request_id => {
                        circuit_outcome = if code == "WORKER_CRASH" {
                            CircuitOutcome::WorkerCrash
                        } else if matches!(
                            code.as_str(),
                            "EXECUTION_FAILED" | "EXECUTION_CANCELLED" | "EXECUTION_TIMED_OUT"
                        ) {
                            CircuitOutcome::WorkerResponded
                        } else {
                            CircuitOutcome::Infrastructure
                        };
                        return Err(format!("{code}: {message}"));
                    }''',
    "classify Environment worker response",
)
text = one(
    text,
    '''        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .remove(&request_id);
        let _ = take_cancelled(&self.cancelled, &request_id);
        result
    }
''',
    '''        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .remove(&request_id);
        let _ = take_cancelled(&self.cancelled, &request_id);
        match circuit_outcome {
            CircuitOutcome::WorkerCrash => {
                self.artifact_crash_circuit
                    .record_crash(&task.artifact_hash, circuit_permit);
            }
            CircuitOutcome::WorkerResponded => {
                self.artifact_crash_circuit
                    .record_non_crash(&task.artifact_hash);
            }
            CircuitOutcome::Infrastructure => {
                self.artifact_crash_circuit
                    .abort(&task.artifact_hash, circuit_permit);
            }
        }
        result
    }
''',
    "record circuit execution outcome",
)
text = one(
    text,
    '''                match execute_isolated_worker(worker, state, &mut stream, &bootstrap.session) {
                    Ok(output) => ChildResponse::Finished { request_id, output },
                    Err(error) if error == "execution cancelled" => {
                        child_error(Some(request_id), "EXECUTION_CANCELLED", &error)
                    }
                    Err(error) => child_error(Some(request_id), "EXECUTION_FAILED", &error),
                }''',
    '''                match execute_isolated_worker(worker, state, &mut stream, &bootstrap.session) {
                    Ok(output) => ChildResponse::Finished { request_id, output },
                    Err(error) => child_error(Some(request_id), error.code, &error.message),
                }''',
    "typed child worker failures",
)
replacement = r'''struct WorkerExecutionFailure {
    code: &'static str,
    message: String,
}

impl WorkerExecutionFailure {
    fn cancelled() -> Self {
        Self {
            code: "EXECUTION_CANCELLED",
            message: "execution cancelled".into(),
        }
    }

    fn timed_out(timeout_ms: u64) -> Self {
        Self {
            code: "EXECUTION_TIMED_OUT",
            message: format!("Environment worker timed out after {timeout_ms} ms"),
        }
    }

    fn guest(message: impl Into<String>) -> Self {
        Self {
            code: "EXECUTION_FAILED",
            message: message.into(),
        }
    }

    fn crash(message: impl Into<String>) -> Self {
        Self {
            code: "WORKER_CRASH",
            message: message.into(),
        }
    }

    fn infrastructure(message: impl Into<String>) -> Self {
        Self {
            code: "WORKER_INFRASTRUCTURE_FAILED",
            message: message.into(),
        }
    }
}

/// `std::process::Child` does not kill on Drop. This guard makes worker teardown
/// fail-safe: every early-return path kills/reaps the disposable worker unless
/// it has already exited, preventing capability-relay/setup errors from leaving
/// an unowned sandbox process behind.
struct WorkerChildGuard {
    child: Child,
}

impl WorkerChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Deref for WorkerChildGuard {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for WorkerChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for WorkerChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn execute_isolated_worker(
    worker: WorkerExecution<'_>,
    state: &EnvironmentChildState,
    controller: &mut TcpStream,
    session: &str,
) -> Result<Vec<u8>, WorkerExecutionFailure> {
    let WorkerExecution {
        execution_id,
        generation,
        runtime_image,
        source_id,
        capability_abi,
        artifact_hash,
        fuel,
        max_memory_bytes,
        timeout_ms,
        input,
        debug,
        environment,
    } = worker;
    let active_executions = &state.active_executions;
    let cancelled = &state.cancelled;
    let exe = std::env::current_exe().map_err(|error| {
        WorkerExecutionFailure::infrastructure(format!("resolve worker executable: {error}"))
    })?;
    let mut command = Command::new(exe);
    command
        .arg("--worker")
        .arg("--artifact")
        .arg(artifact_hash)
        .arg("--fuel")
        .arg(fuel.to_string())
        .arg("--memory")
        .arg(max_memory_bytes.to_string())
        .arg("--environment")
        .arg(environment);
    if debug {
        command.arg("--debug");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if debug {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    let mut child = WorkerChildGuard::new(command.spawn().map_err(|error| {
        WorkerExecutionFailure::infrastructure(format!("spawn Environment worker: {error}"))
    })?);
    active_executions
        .lock()
        .map_err(|_| {
            WorkerExecutionFailure::infrastructure(
                "Environment active execution table poisoned",
            )
        })?
        .insert(
            execution_id.to_string(),
            ActiveExecutionIdentity {
                generation,
                runtime_image: runtime_image.to_string(),
                source_id: source_id.to_string(),
                capability_abi,
            },
        );
    let result = (|| -> Result<Vec<u8>, WorkerExecutionFailure> {
        if take_cancelled(cancelled, execution_id)
            .map_err(WorkerExecutionFailure::infrastructure)?
        {
            return Err(WorkerExecutionFailure::cancelled());
        }
        let mut worker_stdin = child.stdin.take().ok_or_else(|| {
            WorkerExecutionFailure::infrastructure("Environment worker stdin unavailable")
        })?;
        write_worker_input(&mut worker_stdin, input).map_err(|error| {
            WorkerExecutionFailure::infrastructure(format!(
                "write Environment worker input: {error}"
            ))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            WorkerExecutionFailure::infrastructure("Environment worker stdout unavailable")
        })?;
        let (frame_tx, frame_rx) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("rbe-env-worker-output".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let frame = read_worker_output(&mut reader);
                    let terminal = matches!(&frame, Ok(WorkerOutputFrame::Result(_)) | Err(_));
                    if frame_tx.send(frame).is_err() || terminal {
                        break;
                    }
                }
            })
            .map_err(|error| {
                WorkerExecutionFailure::infrastructure(format!(
                    "spawn worker output reader: {error}"
                ))
            })?;
        let started = Instant::now();
        let timeout = Duration::from_millis(timeout_ms.max(1));
        loop {
            if is_cancelled(cancelled, execution_id)
                .map_err(WorkerExecutionFailure::infrastructure)?
            {
                let _ = reader.join();
                return Err(WorkerExecutionFailure::cancelled());
            }
            if started.elapsed() >= timeout {
                let _ = reader.join();
                return Err(WorkerExecutionFailure::timed_out(timeout_ms));
            }
            match frame_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok(WorkerOutputFrame::CapabilityCall(call))) => {
                    write_frame(
                        controller,
                        &ChildResponse::CapabilityCall {
                            request_id: execution_id.to_string(),
                            call: call.clone(),
                        },
                    )
                    .map_err(|error| {
                        WorkerExecutionFailure::infrastructure(format!(
                            "relay capability call to Controller: {error}"
                        ))
                    })?;
                    let response: ChildRequest = read_typed(&mut BufReader::new(
                        controller.try_clone().map_err(|error| {
                            WorkerExecutionFailure::infrastructure(format!(
                                "clone capability relay socket: {error}"
                            ))
                        })?,
                    ))
                    .map_err(|error| {
                        WorkerExecutionFailure::infrastructure(format!(
                            "read Controller capability response: {error}"
                        ))
                    })?;
                    let result = match response {
                        ChildRequest::CapabilityResult {
                            session: returned_session,
                            execution_id: returned_execution,
                            result,
                        } if returned_session == session
                            && returned_execution == execution_id
                            && capability_result_id(&result) == call.call_id =>
                        {
                            result
                        }
                        _ => {
                            return Err(WorkerExecutionFailure::infrastructure(
                                "Controller capability response identity mismatch",
                            ))
                        }
                    };
                    write_worker_capability_result(&mut worker_stdin, &result).map_err(
                        |error| {
                            WorkerExecutionFailure::infrastructure(format!(
                                "send capability result to worker: {error}"
                            ))
                        },
                    )?;
                }
                Ok(Ok(WorkerOutputFrame::Result(frame))) => {
                    let status = child.wait().map_err(|error| {
                        WorkerExecutionFailure::infrastructure(format!(
                            "wait for Environment worker: {error}"
                        ))
                    })?;
                    reader.join().map_err(|_| {
                        WorkerExecutionFailure::infrastructure(
                            "Environment worker output reader panicked",
                        )
                    })?;
                    if !status.success() {
                        return Err(WorkerExecutionFailure::crash(format!(
                            "Environment worker exited with status {status}"
                        )));
                    }
                    return match frame {
                        WorkerResultFrame::Success(output) => Ok(output),
                        WorkerResultFrame::Error(message) => {
                            Err(WorkerExecutionFailure::guest(message))
                        }
                    };
                }
                Ok(Err(error)) => {
                    let _ = reader.join();
                    return Err(WorkerExecutionFailure::crash(format!(
                        "invalid Environment worker output: {error}"
                    )));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child.try_wait().map_err(|error| {
                        WorkerExecutionFailure::infrastructure(format!(
                            "inspect Environment worker status: {error}"
                        ))
                    })? {
                        let _ = reader.join();
                        return Err(WorkerExecutionFailure::crash(format!(
                            "Environment worker exited without terminal result: {status}"
                        )));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = reader.join();
                    return Err(WorkerExecutionFailure::crash(
                        "Environment worker output channel disconnected",
                    ));
                }
            }
        }
    })();
    active_executions
        .lock()
        .map_err(|_| {
            WorkerExecutionFailure::infrastructure(
                "Environment active execution table poisoned during cleanup",
            )
        })?
        .remove(execution_id);
    let _ = take_cancelled(cancelled, execution_id);
    result
}

'''
text = section(
    text,
    "fn execute_isolated_worker(\n",
    "fn capability_result_id(result: &WorkerCapabilityResult) -> u64 {",
    replacement,
    "typed isolated worker execution",
)
text = one(
    text,
    '''    #[test]
    fn session_comparison_rejects_wrong_value() {''',
    '''    #[test]
    fn crash_circuit_opens_then_allows_one_half_open_probe() {
        let circuit = ArtifactCrashCircuit::default();
        let hash = "ab".repeat(32);
        let start = Instant::now();
        for offset in 0..WORKER_CRASH_THRESHOLD {
            let now = start + Duration::from_millis(offset as u64);
            let permit = circuit.acquire_at(&hash, now).unwrap();
            assert_eq!(permit, CircuitPermit::Normal);
            circuit.record_crash_at(&hash, permit, now);
        }
        assert!(circuit
            .acquire_at(&hash, start + Duration::from_secs(1))
            .is_err());
        let probe_at = start + WORKER_CRASH_COOLDOWN + Duration::from_secs(1);
        assert_eq!(
            circuit.acquire_at(&hash, probe_at).unwrap(),
            CircuitPermit::HalfOpenProbe
        );
        assert!(circuit.acquire_at(&hash, probe_at).is_err());
        circuit.record_non_crash(&hash);
        assert_eq!(
            circuit.acquire_at(&hash, probe_at).unwrap(),
            CircuitPermit::Normal
        );
    }

    #[test]
    fn non_crash_response_resets_crash_streak() {
        let circuit = ArtifactCrashCircuit::default();
        let hash = "cd".repeat(32);
        let start = Instant::now();
        for offset in 0..2 {
            let now = start + Duration::from_millis(offset);
            let permit = circuit.acquire_at(&hash, now).unwrap();
            circuit.record_crash_at(&hash, permit, now);
        }
        circuit.record_non_crash(&hash);
        assert!(circuit.snapshots().is_empty());
        assert_eq!(circuit.open_count(), 0);
    }

    #[test]
    fn worker_failure_codes_distinguish_crash_timeout_and_guest_error() {
        assert_eq!(WorkerExecutionFailure::crash("x").code, "WORKER_CRASH");
        assert_eq!(
            WorkerExecutionFailure::timed_out(1).code,
            "EXECUTION_TIMED_OUT"
        );
        assert_eq!(
            WorkerExecutionFailure::guest("x").code,
            "EXECUTION_FAILED"
        );
    }

    #[test]
    fn session_comparison_rejects_wrong_value() {''',
    "crash circuit tests",
)
env.write_text(text, encoding="utf-8")

main = Path("container-runtime/crates/container-bin/src/main.rs")
m = main.read_text(encoding="utf-8")
m = one(
    m,
    '''                        "artifact_cache": runtime.cache().artifact_count(),
                        "profile_cache": runtime.cache().len()
                    }),''',
    '''                        "artifact_cache": runtime.cache().artifact_count(),
                        "profile_cache": runtime.cache().len(),
                        "artifact_crash_circuits_open": environment_processes.open_artifact_crash_circuit_count()
                    }),''',
    "health crash circuit count",
)
m = one(
    m,
    '''    let cache_profiles = runtime
        .cache()''',
    '''    let artifact_crash_circuits = environment_processes
        .artifact_crash_circuits()
        .into_iter()
        .map(|circuit| {
            serde_json::json!({
                "artifact_hash": circuit.artifact_hash,
                "consecutive_crashes": circuit.consecutive_crashes,
                "open_remaining_ms": circuit.open_remaining_ms,
                "half_open_probe": circuit.half_open_probe
            })
        })
        .collect::<Vec<_>>();

    let cache_profiles = runtime
        .cache()''',
    "inspection circuit snapshots",
)
m = one(
    m,
    '''            "durable_profiles": true,
            "durable_artifacts": true
        },
        "environment_processes": environment_processes,''',
    '''            "durable_profiles": true,
            "durable_artifacts": true
        },
        "artifact_crash_circuits": artifact_crash_circuits,
        "environment_processes": environment_processes,''',
    "inspection circuit output",
)
main.write_text(m, encoding="utf-8")
