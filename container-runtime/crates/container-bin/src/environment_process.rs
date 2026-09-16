use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use container_runtime_core::{
    dispatch_storage_capability_with_project_root, Canceller, CapabilityBroker, CapabilityCall,
    EnvironmentId, EnvironmentProfile, EnvironmentStorageManager, ExecutionProvenance,
    ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
use ipc_protocol::{
    read_frame, read_worker_output, write_frame, write_worker_capability_result,
    write_worker_input, CapabilityKind, HostCapabilityRequest, HostCapabilityResponse,
    WorkerCapabilityCall, WorkerCapabilityResult, WorkerOutputFrame, WorkerResultFrame,
    CAPABILITY_ABI_VERSION, HOST_CAPABILITY_PROTOCOL_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CHILD_PROTOCOL_VERSION: u16 = 5;
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_MAX_BYTES: usize = 128;
const PENDING_CANCEL_TTL: Duration = Duration::from_secs(30);
const WORKER_CRASH_THRESHOLD: u32 = 3;
const WORKER_CRASH_WINDOW: Duration = Duration::from_secs(30);
const WORKER_CRASH_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bootstrap {
    version: u16,
    environment: String,
    generation: u64,
    storage_limit_bytes: u64,
    project_root: String,
    debug: bool,
    session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ChildRequest {
    Bootstrap(Bootstrap),
    Execute {
        request_id: String,
        session: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        environment: String,
        generation: u64,
        artifact_hash: String,
        fuel: u64,
        max_memory_bytes: u64,
        timeout_ms: u64,
        input: Vec<u8>,
    },
    Ping {
        request_id: String,
        session: String,
    },
    ResetVolatile {
        request_id: String,
        session: String,
    },
    Cancel {
        request_id: String,
        session: String,
        environment: String,
        generation: u64,
        execution_id: String,
    },
    StorageCapability {
        request_id: String,
        session: String,
        execution_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        environment: String,
        generation: u64,
        call: WorkerCapabilityCall,
        max_response_bytes: u64,
    },
    CapabilityResult {
        session: String,
        execution_id: String,
        result: WorkerCapabilityResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ChildResponse {
    Ready {
        environment: String,
        generation: u64,
        pid: u32,
        address: String,
        session: String,
        debug: bool,
        storage_root: String,
    },
    Finished {
        request_id: String,
        output: Vec<u8>,
    },
    Pong {
        request_id: String,
        pid: u32,
        generation: u64,
    },
    VolatileReset {
        request_id: String,
    },
    CancelAccepted {
        request_id: String,
        execution_id: String,
        active: bool,
    },
    CapabilityCall {
        request_id: String,
        call: WorkerCapabilityCall,
    },
    StorageCapabilityResult {
        request_id: String,
        execution_id: String,
        result: WorkerCapabilityResult,
    },
    Error {
        request_id: Option<String>,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
struct Endpoint {
    address: SocketAddr,
    session: String,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
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
    project_root: Arc<PathBuf>,
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
                state.probe_in_flight || state.open_until.is_some_and(|deadline| deadline > now)
            })
            .count()
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct WorkerExecution<'a> {
    execution_id: &'a str,
    generation: u64,
    runtime_image: &'a str,
    source_id: &'a str,
    capability_abi: u16,
    artifact_hash: &'a str,
    fuel: u64,
    max_memory_bytes: u64,
    timeout_ms: u64,
    input: &'a [u8],
    debug: bool,
    environment: &'a str,
}

#[derive(Debug, Clone)]
pub struct CapabilityDispatchRequest {
    pub execution_id: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
    pub call_id: u64,
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CapabilityDispatchError {
    pub code: String,
    pub message: String,
}

pub type CapabilityDispatcher = Arc<
    dyn Fn(CapabilityDispatchRequest) -> Result<Vec<u8>, CapabilityDispatchError> + Send + Sync,
>;

pub fn unavailable_capability_dispatcher() -> CapabilityDispatcher {
    Arc::new(|request| {
        let _consumed_metadata = (
            request.execution_id.as_str(),
            request.runtime_image.as_str(),
            request.source_id.as_str(),
            request.capability_abi,
            request.environment.as_str(),
            request.generation,
            request.call_id,
            request.kind,
            request.target.as_str(),
            request.operation.as_str(),
            request.payload.len(),
            request.max_response_bytes,
        );
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}

pub fn authenticated_host_capability_dispatcher(
    address: SocketAddr,
    token: String,
) -> Result<CapabilityDispatcher> {
    if !address.ip().is_loopback() {
        bail!("trusted host capability endpoint must be loopback");
    }
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("trusted host capability token must be 256-bit hexadecimal");
    }

    Ok(Arc::new(move |request| {
        let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|_| {
            CapabilityDispatchError {
                code: "CAPABILITY_HOST_UNAVAILABLE".into(),
                message: "trusted host capability endpoint is unavailable".into(),
            }
        })?;
        stream
            .set_nodelay(true)
            .map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_IO".into(),
                message: "failed to configure trusted host capability channel".into(),
            })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_IO".into(),
                message: "failed to configure trusted host capability channel".into(),
            })?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_IO".into(),
                message: "failed to configure trusted host capability channel".into(),
            })?;

        let execution_id = request.execution_id.clone();
        let call_id = request.call_id;
        let host_request = HostCapabilityRequest {
            version: HOST_CAPABILITY_PROTOCOL_VERSION,
            auth_token: token.clone(),
            execution_id: execution_id.clone(),
            runtime_image: request.runtime_image,
            source_id: request.source_id,
            capability_abi: request.capability_abi,
            environment: request.environment,
            generation: request.generation,
            call_id,
            kind: request.kind,
            target: request.target,
            operation: request.operation,
            payload: request.payload,
            max_response_bytes: request.max_response_bytes,
        };
        write_frame(&mut stream, &host_request).map_err(|_| CapabilityDispatchError {
            code: "CAPABILITY_HOST_IO".into(),
            message: "failed to send trusted host capability request".into(),
        })?;
        let frame = read_frame(&mut stream).map_err(|_| CapabilityDispatchError {
            code: "CAPABILITY_HOST_IO".into(),
            message: "failed to read trusted host capability response".into(),
        })?;
        let response: HostCapabilityResponse =
            serde_json::from_slice(&frame).map_err(|_| CapabilityDispatchError {
                code: "CAPABILITY_HOST_PROTOCOL".into(),
                message: "trusted host capability response was malformed".into(),
            })?;
        match response {
            HostCapabilityResponse::Success {
                execution_id: returned_execution,
                call_id: returned_call,
                payload,
            } if returned_execution == execution_id && returned_call == call_id => Ok(payload),
            HostCapabilityResponse::Error {
                execution_id: returned_execution,
                call_id: returned_call,
                code,
                message,
            } if returned_execution == execution_id && returned_call == call_id => {
                Err(CapabilityDispatchError { code, message })
            }
            _ => Err(CapabilityDispatchError {
                code: "CAPABILITY_HOST_PROTOCOL".into(),
                message: "trusted host capability response identity did not match the request"
                    .into(),
            }),
        }
    }))
}

struct ManagedEnvironment {
    child: Child,
    // Keeping the write side alive is the parent-liveness lease. The child has
    // a thread blocked on stdin; controller death/Drop closes this pipe and the
    // child exits even if its loopback listener is otherwise idle.
    _liveness: ChildStdin,
    endpoint: Endpoint,
    debug: bool,
}

#[derive(Debug, Clone)]
pub struct EnvironmentProcessSnapshot {
    pub environment: String,
    pub pid: u32,
    pub generation: u64,
    pub address: String,
    pub alive: bool,
    pub debug: bool,
}

/// Owns one long-lived `container --environment-child` process per configured
/// Environment. Swamps remain controller-side schedulers for now, but executable
/// work crosses this OS-process boundary before a disposable WASM worker is
/// created. This is the first real Environment process boundary and the same
/// listener is intentionally suitable for the later authenticated debug shell.
pub struct EnvironmentProcessSupervisor {
    debug: bool,
    session_root: [u8; 32],
    next_session: AtomicU64,
    processes: Mutex<HashMap<EnvironmentId, ManagedEnvironment>>,
    executions: Mutex<HashMap<String, ExecutionOwner>>,
    cancelled: Mutex<HashMap<String, Instant>>,
    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
    project_root: Arc<PathBuf>,
    artifact_crash_circuit: ArtifactCrashCircuit,
}

impl EnvironmentProcessSupervisor {
    pub fn start(
        general_environments: usize,
        debug: bool,
        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
        project_root: PathBuf,
    ) -> Result<Arc<Self>> {
        let project_root = project_root
            .canonicalize()
            .context("canonicalize Environment supervisor ProjectRoot")?;
        if !project_root.is_dir() {
            bail!(
                "Environment supervisor ProjectRoot is not a directory: {}",
                project_root.display()
            );
        }
        let supervisor = Arc::new(Self {
            debug,
            session_root: make_session_root(controller_token),
            next_session: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
            executions: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashMap::new()),
            capability_broker,
            capability_dispatcher,
            project_root: Arc::new(project_root),
            artifact_crash_circuit: ArtifactCrashCircuit::default(),
        });
        for id in active_environment_ids(general_environments) {
            let managed = supervisor.spawn_one(id, 0)?;
            supervisor
                .processes
                .lock()
                .expect("Environment process table poisoned")
                .insert(id, managed);
        }
        Ok(supervisor)
    }

    pub fn runner(self: &Arc<Self>) -> Runner {
        let supervisor = Arc::clone(self);
        Arc::new(move |task| supervisor.execute(task))
    }

    pub fn canceller(self: &Arc<Self>) -> Canceller {
        let supervisor = Arc::clone(self);
        Arc::new(move |execution_id| supervisor.cancel_execution(execution_id))
    }

    fn cancel_execution(&self, execution_id: &str) -> Result<bool, String> {
        mark_cancelled(&self.cancelled, execution_id)?;
        let owner = self
            .executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .get(execution_id)
            .copied();
        let Some(owner) = owner else {
            return Ok(true);
        };
        let endpoint = {
            let table = self
                .processes
                .lock()
                .map_err(|_| "Environment process table poisoned".to_string())?;
            let managed = table.get(&owner.environment).ok_or_else(|| {
                format!("Environment process {} is unavailable", owner.environment)
            })?;
            if managed.endpoint.generation != owner.generation {
                return Ok(false);
            }
            managed.endpoint.clone()
        };
        let mut stream = TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect Environment for cancellation: {error}"))?;
        stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let request_id = format!("cancel-{execution_id}");
        write_frame(
            &mut stream,
            &ChildRequest::Cancel {
                request_id: request_id.clone(),
                session: endpoint.session,
                environment: owner.environment.to_string(),
                generation: owner.generation,
                execution_id: execution_id.to_string(),
            },
        )
        .map_err(|e| format!("send Environment cancellation: {e}"))?;
        match read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
            .map_err(|e| format!("read Environment cancellation: {e}"))?
        {
            ChildResponse::CancelAccepted {
                request_id: returned,
                execution_id: returned_exec,
                ..
            } if returned == request_id && returned_exec == execution_id => Ok(true),
            ChildResponse::Error {
                request_id: Some(returned),
                code,
                message,
            } if returned == request_id => Err(format!("{code}: {message}")),
            _ => Err("Environment returned a mismatched cancellation response".into()),
        }
    }

    pub fn restart(&self, id: EnvironmentId, generation: u64) -> Result<()> {
        let old = self
            .processes
            .lock()
            .expect("Environment process table poisoned")
            .remove(&id);
        if let Some(mut old) = old {
            let _ = old.child.kill();
            let _ = old.child.wait();
        }
        let managed = self.spawn_one(id, generation)?;
        self.processes
            .lock()
            .expect("Environment process table poisoned")
            .insert(id, managed);
        Ok(())
    }

    pub fn snapshots(&self) -> Vec<EnvironmentProcessSnapshot> {
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

    fn execute(&self, task: &ExecutionTask) -> Result<Vec<u8>, String> {
        if task.payload.len() > MAX_EXECUTION_INPUT_BYTES {
            return Err("Environment invocation input exceeds Container limit".into());
        }
        let id = parse_environment(&task.environment)
            .ok_or_else(|| format!("unknown Environment {}", task.environment))?;
        let endpoint = {
            let mut table = self
                .processes
                .lock()
                .map_err(|_| "Environment process table poisoned".to_string())?;
            let managed = table.get_mut(&id).ok_or_else(|| {
                format!("Environment process {} is unavailable", task.environment)
            })?;
            match managed.child.try_wait() {
                Ok(None) => managed.endpoint.clone(),
                Ok(Some(status)) => {
                    return Err(format!(
                        "Environment process {} exited before execution with status {status}",
                        task.environment
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "Environment process {} liveness check failed: {error}",
                        task.environment
                    ));
                }
            }
        };

        let provenance = task.provenance.as_ref().ok_or_else(|| {
            "execution provenance is required for Environment dispatch".to_string()
        })?;
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
            .map_err(|error| format!("connect Environment {}: {error}", task.environment))?;
        let io_timeout =
            Duration::from_millis(task.limits.wall_time_ms.max(1).saturating_add(2_000));
        stream
            .set_read_timeout(Some(io_timeout))
            .map_err(|error| format!("set Environment read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|error| format!("set Environment write timeout: {error}"))?;

        let request_id = task.id.to_string();
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
        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session.clone(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: task.environment.clone(),
            generation: provenance.generation,
            artifact_hash: task.artifact_hash.clone(),
            fuel: task.limits.cpu_millis.saturating_mul(10_000).max(1_000_000),
            max_memory_bytes: task.limits.memory_bytes.max(64 * 1024),
            timeout_ms: task.limits.wall_time_ms.max(1),
            input: task.payload.clone(),
        };
        #[derive(Clone, Copy)]
        enum CircuitOutcome {
            Infrastructure,
            WorkerResponded,
            WorkerCrash,
        }
        let mut circuit_outcome = CircuitOutcome::Infrastructure;
        let result = (|| -> Result<Vec<u8>, String> {
            write_frame(&mut stream, &request)
                .map_err(|error| format!("send Environment execution: {error}"))?;
            let mut reader = BufReader::new(
                stream
                    .try_clone()
                    .map_err(|error| format!("clone Environment execution socket: {error}"))?,
            );
            loop {
                let response = read_typed::<ChildResponse, _>(&mut reader)
                    .map_err(|error| format!("read Environment execution result: {error}"))?;
                match response {
                    ChildResponse::CapabilityCall {
                        request_id: returned,
                        call,
                    } if returned == request_id => {
                        let result = self.dispatch_capability(task, call);
                        write_frame(
                            &mut stream,
                            &ChildRequest::CapabilityResult {
                                session: endpoint.session.clone(),
                                execution_id: request_id.clone(),
                                result,
                            },
                        )
                        .map_err(|error| format!("send Environment capability result: {error}"))?;
                    }
                    ChildResponse::Finished {
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
                    }
                    _ => return Err("Environment process returned a mismatched response".into()),
                }
            }
        })();
        self.executions
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

    fn dispatch_capability(
        &self,
        task: &ExecutionTask,
        call: WorkerCapabilityCall,
    ) -> WorkerCapabilityResult {
        let Some(provenance) = task.provenance.as_ref() else {
            return WorkerCapabilityResult::Error {
                call_id: call.call_id,
                code: "CAPABILITY_PROVENANCE_MISSING".into(),
                message: "execution has no trusted capability provenance".into(),
            };
        };
        let authorized = match self.capability_broker.authorize(CapabilityCall {
            runtime_image: &provenance.runtime_image,
            source_id: &provenance.source_id,
            environment: &provenance.environment,
            generation: provenance.generation,
            kind: call.kind,
            target: &call.target,
            operation: &call.operation,
            request_bytes: call.payload.len(),
        }) {
            Ok(authorized) => authorized,
            Err(error) => {
                return WorkerCapabilityResult::Error {
                    call_id: call.call_id,
                    code: error.code.into(),
                    message: error.message,
                };
            }
        };
        if environment_owned_capability(call.kind) {
            return self.dispatch_environment_storage(
                task,
                provenance,
                call,
                authorized.max_response_bytes,
            );
        }

        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: provenance.environment.clone(),
            generation: provenance.generation,
            call_id: call.call_id,
            kind: call.kind,
            target: call.target,
            operation: call.operation,
            payload: call.payload,
            max_response_bytes: authorized.max_response_bytes,
        };
        match (self.capability_dispatcher)(request) {
            Ok(payload)
                if payload.len() <= MAX_CAPABILITY_PAYLOAD_BYTES
                    && payload.len() as u64 <= authorized.max_response_bytes =>
            {
                WorkerCapabilityResult::Success {
                    call_id: call.call_id,
                    payload,
                }
            }
            Ok(_) => WorkerCapabilityResult::Error {
                call_id: call.call_id,
                code: "CAPABILITY_RESPONSE_TOO_LARGE".into(),
                message: "trusted dispatcher response exceeded the capability grant".into(),
            },
            Err(error) => WorkerCapabilityResult::Error {
                call_id: call.call_id,
                code: error.code,
                message: error.message,
            },
        }
    }

    fn dispatch_environment_storage(
        &self,
        task: &ExecutionTask,
        provenance: &ExecutionProvenance,
        call: WorkerCapabilityCall,
        max_response_bytes: u64,
    ) -> WorkerCapabilityResult {
        let call_id = call.call_id;
        let environment = match parse_environment(&provenance.environment) {
            Some(environment) => environment,
            None => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_INVALID",
                    "Storage capability provenance names an unknown Environment",
                )
            }
        };
        let endpoint = {
            let table = match self.processes.lock() {
                Ok(table) => table,
                Err(_) => {
                    return capability_error(
                        call_id,
                        "CAPABILITY_ENVIRONMENT_STATE_FAILED",
                        "Environment process table is unavailable",
                    )
                }
            };
            let Some(managed) = table.get(&environment) else {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_UNAVAILABLE",
                    "owning Environment process is unavailable",
                );
            };
            if managed.endpoint.generation != provenance.generation {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_STALE",
                    "Storage capability provenance targets a stale Environment generation",
                );
            }
            managed.endpoint.clone()
        };

        let mut stream = match TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(_) => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_UNAVAILABLE",
                    "owning Environment process could not be reached",
                )
            }
        };
        let io_timeout = Duration::from_millis(task.limits.wall_time_ms.clamp(1, 30_000));
        if stream.set_read_timeout(Some(io_timeout)).is_err()
            || stream.set_write_timeout(Some(CONNECT_TIMEOUT)).is_err()
        {
            return capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_IO",
                "failed to configure Environment Storage channel",
            );
        }

        let execution_id = task.id.to_string();
        let request_id = format!("storage-{execution_id}-{call_id}");
        let request = ChildRequest::StorageCapability {
            request_id: request_id.clone(),
            session: endpoint.session,
            execution_id: execution_id.clone(),
            runtime_image: provenance.runtime_image.clone(),
            source_id: provenance.source_id.clone(),
            capability_abi: provenance.capability_abi,
            environment: provenance.environment.clone(),
            generation: provenance.generation,
            call,
            max_response_bytes,
        };
        if write_frame(&mut stream, &request).is_err() {
            return capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_IO",
                "failed to send Environment Storage capability request",
            );
        }
        let response = match read_typed::<ChildResponse, _>(&mut BufReader::new(stream)) {
            Ok(response) => response,
            Err(_) => {
                return capability_error(
                    call_id,
                    "CAPABILITY_ENVIRONMENT_IO",
                    "failed to read Environment Storage capability response",
                )
            }
        };
        match response {
            ChildResponse::StorageCapabilityResult {
                request_id: returned_request,
                execution_id: returned_execution,
                result,
            } if returned_request == request_id
                && returned_execution == execution_id
                && capability_result_id(&result) == call_id =>
            {
                result
            }
            ChildResponse::Error {
                request_id: Some(returned_request),
                code,
                message,
            } if returned_request == request_id => capability_error(call_id, code, message),
            _ => capability_error(
                call_id,
                "CAPABILITY_ENVIRONMENT_PROTOCOL",
                "Environment Storage capability response identity did not match the request",
            ),
        }
    }

    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {
        let session = self.new_session(id, generation);
        let child_debug = environment_debug_enabled(self.debug, id);
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--environment-child")
            .env_remove("RBE_CONTAINER_TOKEN")
            .env_remove("RBE_HOST_CAPABILITY_ADDR")
            .env_remove("RBE_HOST_CAPABILITY_TOKEN");
        if child_debug {
            // Visible diagnostic propagation only. The session capability on the
            // inherited bootstrap pipe remains the actual authority.
            command.arg("--debug");
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        if child_debug {
            command.stderr(Stdio::inherit());
        } else {
            command.stderr(Stdio::null());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn Environment process {id}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Environment process {id} has no bootstrap stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Environment process {id} has no bootstrap stdout"))?;

        let bootstrap = ChildRequest::Bootstrap(Bootstrap {
            version: CHILD_PROTOCOL_VERSION,
            environment: id.to_string(),
            generation,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: self
                .project_root
                .to_str()
                .ok_or_else(|| anyhow!("RBE ProjectRoot is not valid UTF-8"))?
                .to_string(),
            debug: child_debug,
            session: session.clone(),
        });
        if let Err(error) = write_frame(&mut stdin, &bootstrap) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("write Environment bootstrap");
        }

        let (tx, rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(format!("rbe-env-ready-{id}"))
            .spawn(move || {
                let response = read_typed::<ChildResponse, _>(&mut BufReader::new(stdout));
                let _ = tx.send(response);
            })?;
        let response = match rx.recv_timeout(READY_TIMEOUT) {
            Ok(response) => response?,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Environment process {id} did not become ready: {error}");
            }
        };
        let ChildResponse::Ready {
            environment,
            generation: ready_generation,
            pid,
            address,
            session: echoed_session,
            debug,
            ..
        } = response
        else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Environment process {id} returned an invalid bootstrap response");
        };
        if environment != id.to_string()
            || ready_generation != generation
            || pid != child.id()
            || echoed_session != session
            || debug != child_debug
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Environment process {id} bootstrap identity mismatch");
        }
        let address = address
            .parse::<SocketAddr>()
            .with_context(|| format!("Environment process {id} returned invalid address"))?;

        Ok(ManagedEnvironment {
            child,
            _liveness: stdin,
            endpoint: Endpoint {
                address,
                session,
                generation,
            },
            debug: child_debug,
        })
    }

    fn new_session(&self, id: EnvironmentId, generation: u64) -> String {
        let sequence = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut hash = Sha256::new();
        hash.update(b"RBE-ENV-SESSION-V1\0");
        hash.update(self.session_root);
        hash.update(id.to_string().as_bytes());
        hash.update(generation.to_be_bytes());
        hash.update(sequence.to_be_bytes());
        hex::encode(hash.finalize())
    }
}

impl Drop for EnvironmentProcessSupervisor {
    fn drop(&mut self) {
        let Ok(mut table) = self.processes.lock() else {
            return;
        };
        for managed in table.values_mut() {
            let _ = managed.child.kill();
            let _ = managed.child.wait();
        }
    }
}

pub fn run_environment_child() -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let request: ChildRequest = read_typed(&mut stdin).context("read Environment bootstrap")?;
    drop(stdin);
    let ChildRequest::Bootstrap(bootstrap) = request else {
        bail!("Environment child requires bootstrap as its first frame");
    };
    validate_bootstrap(&bootstrap)?;
    let environment = parse_environment(&bootstrap.environment)
        .ok_or_else(|| anyhow!("invalid Environment {}", bootstrap.environment))?;
    let project_root = PathBuf::from(&bootstrap.project_root)
        .canonicalize()
        .context("canonicalize Environment ProjectRoot")?;
    if !project_root.is_dir() {
        bail!(
            "Environment ProjectRoot is not a directory: {}",
            project_root.display()
        );
    }
    let storage_root = runtime_paths::binary_dir()
        .join("data")
        .join("container-runtime")
        .join("environments")
        .join(environment.to_string());
    let storage =
        EnvironmentStorageManager::open(storage_root.clone(), bootstrap.storage_limit_bytes)
            .context("open Environment transactional storage")?;
    storage
        .reset_volatile()
        .context("reset Environment volatile storage")?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let mut stdout = std::io::stdout().lock();
    write_frame(
        &mut stdout,
        &ChildResponse::Ready {
            environment: bootstrap.environment.clone(),
            generation: bootstrap.generation,
            pid: std::process::id(),
            address: address.to_string(),
            session: bootstrap.session.clone(),
            debug: bootstrap.debug,
            storage_root: storage_root.display().to_string(),
        },
    )?;
    drop(stdout);

    // The bootstrap stdin pipe is intentionally otherwise unused. EOF means the
    // Controller vanished, so the Environment must not outlive its authority.
    thread::Builder::new()
        .name(format!("rbe-env-parent-{}", bootstrap.environment))
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut byte = [0u8; 1];
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => {}
                }
            }
        })?;

    let bootstrap = Arc::new(bootstrap);
    let state = Arc::new(EnvironmentChildState {
        storage,
        project_root: Arc::new(project_root),
        active_executions: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashMap::new()),
    });
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let bootstrap = Arc::clone(&bootstrap);
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle_child_connection(stream, &bootstrap, &state) {
                        tracing::warn!(%error, "Environment child connection failed");
                    }
                });
            }
            Err(error) => tracing::warn!(%error, "Environment child accept failed"),
        }
    }
    Ok(())
}

fn handle_child_connection(
    mut stream: TcpStream,
    bootstrap: &Bootstrap,
    state: &EnvironmentChildState,
) -> Result<()> {
    let request: ChildRequest = read_typed(&mut BufReader::new(stream.try_clone()?))?;
    let response = match request {
        ChildRequest::Execute {
            request_id,
            session,
            runtime_image,
            source_id,
            capability_abi,
            environment,
            generation,
            artifact_hash,
            fuel,
            max_memory_bytes,
            timeout_ms,
            input,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(
                    Some(request_id),
                    "ENVIRONMENT_IDENTITY_MISMATCH",
                    "Environment/generation does not match the child process",
                )
            } else if capability_abi != CAPABILITY_ABI_VERSION
                || !valid_runtime_image(&runtime_image)
                || !valid_source_id(&source_id)
            {
                child_error(
                    Some(request_id),
                    "EXECUTION_PROVENANCE_INVALID",
                    "Runtime Image/SourceId/capability ABI provenance is invalid",
                )
            } else if input.len() > MAX_EXECUTION_INPUT_BYTES {
                child_error(
                    Some(request_id),
                    "EXECUTION_INPUT_TOO_LARGE",
                    "Environment invocation exceeds Container input limit",
                )
            } else if take_cancelled(&state.cancelled, &request_id).unwrap_or(false) {
                child_error(
                    Some(request_id),
                    "EXECUTION_CANCELLED",
                    "execution cancelled before worker start",
                )
            } else {
                let worker = WorkerExecution {
                    execution_id: &request_id,
                    generation,
                    runtime_image: &runtime_image,
                    source_id: &source_id,
                    capability_abi,
                    artifact_hash: &artifact_hash,
                    fuel,
                    max_memory_bytes,
                    timeout_ms,
                    input: &input,
                    debug: bootstrap.debug,
                    environment: &bootstrap.environment,
                };
                match execute_isolated_worker(worker, state, &mut stream, &bootstrap.session) {
                    Ok(output) => ChildResponse::Finished { request_id, output },
                    Err(error) => child_error(Some(request_id), error.code, &error.message),
                }
            }
        }
        ChildRequest::Ping {
            request_id,
            session,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else {
                ChildResponse::Pong {
                    request_id,
                    pid: std::process::id(),
                    generation: bootstrap.generation,
                }
            }
        }
        ChildRequest::ResetVolatile {
            request_id,
            session,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else {
                match state.storage.reset_volatile() {
                    Ok(()) => ChildResponse::VolatileReset { request_id },
                    Err(error) => {
                        child_error(Some(request_id), "STORAGE_RESET_FAILED", &error.to_string())
                    }
                }
            }
        }
        ChildRequest::Cancel {
            request_id,
            session,
            environment,
            generation,
            execution_id,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(
                    Some(request_id),
                    "ENVIRONMENT_IDENTITY_MISMATCH",
                    "Environment/generation does not match the child process",
                )
            } else if let Err(error) = mark_cancelled(&state.cancelled, &execution_id) {
                child_error(Some(request_id), "CANCEL_STATE_FAILED", &error)
            } else {
                let active = state
                    .active_executions
                    .lock()
                    .map_err(|_| anyhow!("Environment active execution table poisoned"))?
                    .get(&execution_id)
                    .map(|identity| {
                        identity.generation == generation
                            && identity.capability_abi == CAPABILITY_ABI_VERSION
                            && valid_runtime_image(&identity.runtime_image)
                            && valid_source_id(&identity.source_id)
                    })
                    .unwrap_or(false);
                ChildResponse::CancelAccepted {
                    request_id,
                    execution_id,
                    active,
                }
            }
        }
        ChildRequest::StorageCapability {
            request_id,
            session,
            execution_id,
            runtime_image,
            source_id,
            capability_abi,
            environment,
            generation,
            call,
            max_response_bytes,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(
                    Some(request_id),
                    "AUTH_FAILED",
                    "Environment session rejected",
                )
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(
                    Some(request_id),
                    "ENVIRONMENT_IDENTITY_MISMATCH",
                    "Environment/generation does not match the child process",
                )
            } else if capability_abi != CAPABILITY_ABI_VERSION
                || !valid_runtime_image(&runtime_image)
                || !valid_source_id(&source_id)
            {
                child_error(
                    Some(request_id),
                    "CAPABILITY_PROVENANCE_INVALID",
                    "Storage capability provenance is invalid",
                )
            } else if call.kind != CapabilityKind::Storage {
                child_error(
                    Some(request_id),
                    "CAPABILITY_KIND_INVALID",
                    "Environment Storage channel accepts only Storage capabilities",
                )
            } else if max_response_bytes == 0
                || max_response_bytes > MAX_CAPABILITY_PAYLOAD_BYTES as u64
            {
                child_error(
                    Some(request_id),
                    "CAPABILITY_RESPONSE_LIMIT_INVALID",
                    "Storage capability response limit is invalid",
                )
            } else {
                match active_execution_identity_matches(
                    state,
                    &execution_id,
                    generation,
                    &runtime_image,
                    &source_id,
                    capability_abi,
                ) {
                    Err(_) => child_error(
                        Some(request_id),
                        "CAPABILITY_STATE_FAILED",
                        "Environment active execution state is unavailable",
                    ),
                    Ok(false) => child_error(
                        Some(request_id),
                        "CAPABILITY_EXECUTION_MISMATCH",
                        "Storage capability is not bound to the active execution provenance",
                    ),
                    Ok(true) => {
                        let result = match dispatch_storage_capability_with_project_root(
                            &state.storage,
                            &state.project_root,
                            &call.target,
                            &call.operation,
                            &call.payload,
                            max_response_bytes,
                        ) {
                            Ok(payload) => WorkerCapabilityResult::Success {
                                call_id: call.call_id,
                                payload,
                            },
                            Err(error) => WorkerCapabilityResult::Error {
                                call_id: call.call_id,
                                code: error.code.to_string(),
                                message: error.message,
                            },
                        };
                        ChildResponse::StorageCapabilityResult {
                            request_id,
                            execution_id,
                            result,
                        }
                    }
                }
            }
        }
        ChildRequest::CapabilityResult { .. } => child_error(
            None,
            "CAPABILITY_RESULT_UNEXPECTED",
            "capability results are only accepted during an active execution",
        ),
        ChildRequest::Bootstrap(_) => child_error(
            None,
            "BOOTSTRAP_REPLAY",
            "Environment bootstrap is only accepted on inherited stdin",
        ),
    };
    write_frame(&mut stream, &response)?;
    Ok(())
}

struct WorkerExecutionFailure {
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

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        self.terminate();
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
            WorkerExecutionFailure::infrastructure("Environment active execution table poisoned")
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
                child.terminate();
                let _ = reader.join();
                return Err(WorkerExecutionFailure::cancelled());
            }
            if started.elapsed() >= timeout {
                child.terminate();
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

fn environment_owned_capability(kind: CapabilityKind) -> bool {
    kind == CapabilityKind::Storage
}

fn active_execution_identity_matches(
    state: &EnvironmentChildState,
    execution_id: &str,
    generation: u64,
    runtime_image: &str,
    source_id: &str,
    capability_abi: u16,
) -> Result<bool, String> {
    let active = state
        .active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?;
    Ok(active.get(execution_id).is_some_and(|identity| {
        identity.generation == generation
            && identity.runtime_image == runtime_image
            && identity.source_id == source_id
            && identity.capability_abi == capability_abi
    }))
}

fn capability_error(
    call_id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
) -> WorkerCapabilityResult {
    WorkerCapabilityResult::Error {
        call_id,
        code: code.into(),
        message: message.into(),
    }
}

fn capability_result_id(result: &WorkerCapabilityResult) -> u64 {
    match result {
        WorkerCapabilityResult::Success { call_id, .. }
        | WorkerCapabilityResult::Error { call_id, .. } => *call_id,
    }
}

fn mark_cancelled(
    table: &Mutex<HashMap<String, Instant>>,
    execution_id: &str,
) -> Result<(), String> {
    let mut table = table
        .lock()
        .map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    table.insert(execution_id.to_string(), Instant::now());
    Ok(())
}

fn take_cancelled(
    table: &Mutex<HashMap<String, Instant>>,
    execution_id: &str,
) -> Result<bool, String> {
    let mut table = table
        .lock()
        .map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    Ok(table.remove(execution_id).is_some())
}

fn is_cancelled(
    table: &Mutex<HashMap<String, Instant>>,
    execution_id: &str,
) -> Result<bool, String> {
    let mut table = table
        .lock()
        .map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    Ok(table.contains_key(execution_id))
}

fn valid_runtime_image(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

fn child_error(request_id: Option<String>, code: &str, message: &str) -> ChildResponse {
    ChildResponse::Error {
        request_id,
        code: code.to_string(),
        message: message.to_string(),
    }
}

fn valid_session(bootstrap: &Bootstrap, session: &str) -> bool {
    session.len() == bootstrap.session.len()
        && session
            .as_bytes()
            .iter()
            .zip(bootstrap.session.as_bytes())
            .fold(0u8, |diff, (left, right)| diff | (left ^ right))
            == 0
}

fn validate_bootstrap(bootstrap: &Bootstrap) -> Result<()> {
    if bootstrap.version != CHILD_PROTOCOL_VERSION {
        bail!(
            "unsupported Environment child protocol {}; expected {}",
            bootstrap.version,
            CHILD_PROTOCOL_VERSION
        );
    }
    if parse_environment(&bootstrap.environment).is_none() {
        bail!("unknown Environment {}", bootstrap.environment);
    }
    if bootstrap.storage_limit_bytes == 0 {
        bail!("Environment storage limit must be non-zero");
    }
    if bootstrap.project_root.is_empty() {
        bail!("Environment ProjectRoot must be non-empty");
    }
    if bootstrap.session.len() != 64
        || bootstrap.session.len() > SESSION_MAX_BYTES
        || !bootstrap
            .session
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Environment session identity is invalid");
    }
    Ok(())
}

fn make_session_root(controller_token: Option<&str>) -> [u8; 32] {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hash = Sha256::new();
    hash.update(b"RBE-ENV-ROOT-V1\0");
    if let Some(token) = controller_token {
        hash.update(token.as_bytes());
    }
    hash.update(std::process::id().to_be_bytes());
    hash.update(now.to_be_bytes());
    hash.finalize().into()
}

fn environment_debug_enabled(controller_debug: bool, id: EnvironmentId) -> bool {
    controller_debug && id.profile() != EnvironmentProfile::Secure
}

fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {
    let mut ids =
        EnvironmentId::GENERAL[..general_count.clamp(1, EnvironmentId::GENERAL.len())].to_vec();
    ids.push(EnvironmentId::Payment);
    ids
}

fn parse_environment(value: &str) -> Option<EnvironmentId> {
    match value {
        "general-1" => Some(EnvironmentId::General1),
        "general-2" => Some(EnvironmentId::General2),
        "general-3" => Some(EnvironmentId::General3),
        "general-4" => Some(EnvironmentId::General4),
        "general-5" => Some(EnvironmentId::General5),
        "payment" => Some(EnvironmentId::Payment),
        _ => None,
    }
}

fn read_typed<T: DeserializeOwned, R: Read>(reader: &mut R) -> Result<T> {
    let bytes = read_frame(reader)?;
    serde_json::from_slice(&bytes).context("decode Environment process frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_parser_is_closed_set() {
        assert_eq!(
            parse_environment("general-1"),
            Some(EnvironmentId::General1)
        );
        assert_eq!(parse_environment("payment"), Some(EnvironmentId::Payment));
        assert_eq!(parse_environment("visitor-ip-123"), None);
    }

    #[test]
    fn execution_provenance_format_is_strict() {
        assert!(valid_runtime_image(&"ab".repeat(32)));
        assert!(!valid_runtime_image(&"AB".repeat(32)));
        assert!(valid_source_id("route:api/me"));
        assert!(!valid_source_id(""));
        assert!(!valid_source_id("route:\napi"));
    }

    #[test]
    fn secure_environment_never_inherits_controller_debug() {
        assert!(environment_debug_enabled(true, EnvironmentId::General1));
        assert!(!environment_debug_enabled(true, EnvironmentId::Payment));
        assert!(!environment_debug_enabled(false, EnvironmentId::General1));
        assert!(!environment_debug_enabled(false, EnvironmentId::Payment));
    }

    #[test]
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
        assert_eq!(WorkerExecutionFailure::guest("x").code, "EXECUTION_FAILED");
    }

    fn test_storage_state(name: &str) -> (std::path::PathBuf, Arc<EnvironmentChildState>) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-environment-storage-transport-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let storage = EnvironmentStorageManager::open(root.clone(), 4096).unwrap();
        (
            root.clone(),
            Arc::new(EnvironmentChildState {
                storage,
                project_root: Arc::new(root.clone()),
                active_executions: Mutex::new(HashMap::new()),
                cancelled: Mutex::new(HashMap::new()),
            }),
        )
    }

    fn child_round_trip(
        bootstrap: Arc<Bootstrap>,
        state: Arc<EnvironmentChildState>,
        request: ChildRequest,
    ) -> ChildResponse {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_child_connection(stream, &bootstrap, &state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        write_frame(&mut client, &request).unwrap();
        let response = read_typed(&mut BufReader::new(client)).unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn storage_capability_is_environment_owned() {
        assert!(environment_owned_capability(CapabilityKind::Storage));
        assert!(!environment_owned_capability(CapabilityKind::Service));
        assert!(!environment_owned_capability(CapabilityKind::Network));
    }

    #[test]
    fn storage_child_transport_commits_only_for_exact_active_provenance() {
        let (root, state) = test_storage_state("exact");
        let runtime_image = "ab".repeat(32);
        let source_id = "route:storage-test".to_string();
        let execution_id = "exec-storage-test".to_string();
        let generation = 7;
        state.active_executions.lock().unwrap().insert(
            execution_id.clone(),
            ActiveExecutionIdentity {
                generation,
                runtime_image: runtime_image.clone(),
                source_id: source_id.clone(),
                capability_abi: CAPABILITY_ABI_VERSION,
            },
        );
        let bootstrap = Arc::new(Bootstrap {
            version: CHILD_PROTOCOL_VERSION,
            environment: "general-1".into(),
            generation,
            storage_limit_bytes: 4096,
            project_root: root.to_string_lossy().into_owned(),
            debug: false,
            session: "ab".repeat(32),
        });
        let payload = serde_json::to_vec(&serde_json::json!([[
            {"op":"put", "path":"users/kate", "data_hex":"6b617465"}
        ]]))
        .unwrap();
        let call = WorkerCapabilityCall {
            call_id: 42,
            kind: CapabilityKind::Storage,
            target: "storage:uac".into(),
            operation: "commit".into(),
            payload,
        };
        let request = ChildRequest::StorageCapability {
            request_id: "storage-ok".into(),
            session: bootstrap.session.clone(),
            execution_id: execution_id.clone(),
            runtime_image: runtime_image.clone(),
            source_id: source_id.clone(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: bootstrap.environment.clone(),
            generation,
            call: call.clone(),
            max_response_bytes: 4096,
        };
        let response = child_round_trip(Arc::clone(&bootstrap), Arc::clone(&state), request);
        match response {
            ChildResponse::StorageCapabilityResult {
                request_id,
                execution_id: returned_execution,
                result: WorkerCapabilityResult::Success { call_id, .. },
            } => {
                assert_eq!(request_id, "storage-ok");
                assert_eq!(returned_execution, execution_id);
                assert_eq!(call_id, 42);
            }
            other => panic!("unexpected Storage response: {other:?}"),
        }
        assert_eq!(
            state.storage.read("uac", "users/kate").unwrap(),
            Some(b"kate".to_vec())
        );

        let stale_request = ChildRequest::StorageCapability {
            request_id: "storage-stale".into(),
            session: bootstrap.session.clone(),
            execution_id,
            runtime_image,
            source_id: "route:wrong-source".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: bootstrap.environment.clone(),
            generation,
            call,
            max_response_bytes: 4096,
        };
        let stale = child_round_trip(bootstrap, Arc::clone(&state), stale_request);
        match stale {
            ChildResponse::Error {
                request_id: Some(request_id),
                code,
                ..
            } => {
                assert_eq!(request_id, "storage-stale");
                assert_eq!(code, "CAPABILITY_EXECUTION_MISMATCH");
            }
            other => panic!("unexpected stale Storage response: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_comparison_rejects_wrong_value() {
        let bootstrap = Bootstrap {
            version: CHILD_PROTOCOL_VERSION,
            environment: "general-1".into(),
            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            project_root: std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            debug: false,
            session: "ab".repeat(32),
        };
        assert!(valid_session(&bootstrap, &"ab".repeat(32)));
        assert!(!valid_session(&bootstrap, &"ac".repeat(32)));
    }
}
