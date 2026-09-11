use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use container_runtime_core::{
    Canceller, CapabilityBroker, CapabilityCall, EnvironmentId, EnvironmentStorageManager,
    ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
use ipc_protocol::{
    read_frame, read_worker_output, write_frame, write_worker_capability_result,
    write_worker_input, CapabilityKind, WorkerCapabilityCall, WorkerCapabilityResult,
    WorkerOutputFrame, WorkerResultFrame, CAPABILITY_ABI_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CHILD_PROTOCOL_VERSION: u16 = 3;
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_MAX_BYTES: usize = 128;
const PENDING_CANCEL_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bootstrap {
    version: u16,
    environment: String,
    generation: u64,
    storage_limit_bytes: u64,
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
    active_executions: Mutex<HashMap<String, ActiveExecutionIdentity>>,
    cancelled: Mutex<HashMap<String, Instant>>,
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
}

impl EnvironmentProcessSupervisor {
    pub fn start(
        general_environments: usize,
        debug: bool,
        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
    ) -> Result<Arc<Self>> {
        let supervisor = Arc::new(Self {
            debug,
            session_root: make_session_root(controller_token),
            next_session: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
            executions: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashMap::new()),
            capability_broker,
            capability_dispatcher,
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
                    } if returned == request_id => return Ok(output),
                    ChildResponse::Error {
                        request_id: Some(returned),
                        code,
                        message,
                    } if returned == request_id => return Err(format!("{code}: {message}")),
                    _ => return Err("Environment process returned a mismatched response".into()),
                }
            }
        })();
        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .remove(&request_id);
        let _ = take_cancelled(&self.cancelled, &request_id);
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
        let request = CapabilityDispatchRequest {
            execution_id: task.id.to_string(),
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

    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {
        let session = self.new_session(id, generation);
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--environment-child");
        if self.debug {
            // Visible diagnostic propagation only. The session capability on the
            // inherited bootstrap pipe remains the actual authority.
            command.arg("--debug");
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        if self.debug {
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
            debug: self.debug,
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
            || debug != self.debug
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
            debug: self.debug,
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
                    Err(error) if error == "execution cancelled" => {
                        child_error(Some(request_id), "EXECUTION_CANCELLED", &error)
                    }
                    Err(error) => child_error(Some(request_id), "EXECUTION_FAILED", &error),
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

fn execute_isolated_worker(
    worker: WorkerExecution<'_>,
    state: &EnvironmentChildState,
    controller: &mut TcpStream,
    session: &str,
) -> Result<Vec<u8>, String> {
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
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
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
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .insert(
            execution_id.to_string(),
            ActiveExecutionIdentity {
                generation,
                runtime_image: runtime_image.to_string(),
                source_id: source_id.to_string(),
                capability_abi,
            },
        );
    let result = (|| -> Result<Vec<u8>, String> {
        if take_cancelled(cancelled, execution_id)? {
            let _ = child.kill();
            let _ = child.wait();
            return Err("execution cancelled".into());
        }
        let mut worker_stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Environment worker stdin unavailable".to_string())?;
        if let Err(error) = write_worker_input(&mut worker_stdin, input) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("write Environment worker input: {error}"));
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Environment worker stdout unavailable".to_string())?;
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
            .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms.max(1));
        loop {
            if is_cancelled(cancelled, execution_id)? {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("execution cancelled".into());
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!(
                    "Environment worker timed out after {timeout_ms} ms"
                ));
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
                    .map_err(|error| format!("relay capability call to Controller: {error}"))?;
                    let response: ChildRequest =
                        read_typed(&mut BufReader::new(controller.try_clone().map_err(
                            |error| format!("clone capability relay socket: {error}"),
                        )?))
                        .map_err(|error| format!("read Controller capability response: {error}"))?;
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
                        _ => return Err("Controller capability response identity mismatch".into()),
                    };
                    write_worker_capability_result(&mut worker_stdin, &result)
                        .map_err(|error| format!("send capability result to worker: {error}"))?;
                }
                Ok(Ok(WorkerOutputFrame::Result(frame))) => {
                    let status = child.wait().map_err(|error| error.to_string())?;
                    reader
                        .join()
                        .map_err(|_| "Environment worker output reader panicked".to_string())?;
                    if !status.success() {
                        return Err(format!("Environment worker exited with status {status}"));
                    }
                    return match frame {
                        WorkerResultFrame::Success(output) => Ok(output),
                        WorkerResultFrame::Error(message) => Err(message),
                    };
                }
                Ok(Err(error)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(format!("invalid Environment worker output: {error}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        let _ = reader.join();
                        return Err(format!(
                            "Environment worker exited without terminal result: {status}"
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err("Environment worker output channel disconnected".into());
                }
            }
        }
    })();
    active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .remove(execution_id);
    let _ = take_cancelled(cancelled, execution_id);
    result
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
    fn session_comparison_rejects_wrong_value() {
        let bootstrap = Bootstrap {
            version: CHILD_PROTOCOL_VERSION,
            environment: "general-1".into(),
            generation: 0,
            storage_limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
            debug: false,
            session: "ab".repeat(32),
        };
        assert!(valid_session(&bootstrap, &"ab".repeat(32)));
        assert!(!valid_session(&bootstrap, &"ac".repeat(32)));
    }
}
