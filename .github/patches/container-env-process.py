from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


# container-bin gets a small private child-process protocol. The controller
# session token is derived, never forwarded as the backend/container token.
replace_once(
    "container-runtime/crates/container-bin/Cargo.toml",
    'serde_json = "1"\ntracing = "0.1"\n',
    'serde = { version = "1", features = ["derive"] }\nserde_json = "1"\nsha2 = "0.10"\nhex = "0.4"\ntracing = "0.1"\n',
)

# Export the storage default so controller and Environment child cannot drift.
replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    "const DEFAULT_ENVIRONMENT_STORAGE_BYTES: u64 = 100 * 1024 * 1024;",
    "pub const DEFAULT_ENVIRONMENT_STORAGE_BYTES: u64 = 100 * 1024 * 1024;",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/lib.rs",
    "pub use runtime::{Runtime, RuntimeConfig};",
    "pub use runtime::{Runtime, RuntimeConfig, DEFAULT_ENVIRONMENT_STORAGE_BYTES};",
)

# Let the standalone controller inject the per-Environment process runner while
# preserving the in-process/default runner for unit tests and embedders.
replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    "impl Runtime {\n    pub fn new(config: RuntimeConfig) -> Arc<Self> {\n        let config = RuntimeConfig {",
    "impl Runtime {\n    pub fn new(config: RuntimeConfig) -> Arc<Self> {\n        Self::build(config, None)\n    }\n\n    /// Build the scheduler with an external artifact runner. The standalone\n    /// Container Controller uses this to route executable work through the\n    /// dedicated per-Environment `container` child process. Tests and library\n    /// embedders may keep using [`Runtime::new`] and the legacy local runner.\n    pub fn new_with_runner(config: RuntimeConfig, artifact_runner: Runner) -> Arc<Self> {\n        Self::build(config, Some(artifact_runner))\n    }\n\n    fn build(config: RuntimeConfig, artifact_runner: Option<Runner>) -> Arc<Self> {\n        let config = RuntimeConfig {",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/runtime.rs",
    "                let output = if cache.contains_artifact(&task.artifact_hash) {\n                    run_isolated_worker(task, &cancelled)?\n                } else if task.work_ms > 0 {",
    "                let output = if cache.contains_artifact(&task.artifact_hash) {\n                    match artifact_runner.as_ref() {\n                        Some(runner) => runner(task)?,\n                        None => run_isolated_worker(task, &cancelled)?,\n                    }\n                } else if task.work_ms > 0 {",
)

module = r'''use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use container_runtime_core::{
    EnvironmentId, EnvironmentStorageManager, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
use execution_engine::WasmExecutor;
use ipc_protocol::{
    read_frame, read_worker_result, write_frame, write_worker_input, WorkerResultFrame,
    MAX_EXECUTION_INPUT_BYTES,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CHILD_PROTOCOL_VERSION: u16 = 1;
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_MAX_BYTES: usize = 128;

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
}

impl EnvironmentProcessSupervisor {
    pub fn start(
        general_environments: usize,
        debug: bool,
        controller_token: Option<&str>,
    ) -> Result<Arc<Self>> {
        let supervisor = Arc::new(Self {
            debug,
            session_root: make_session_root(controller_token),
            next_session: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
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
            let managed = table
                .get_mut(&id)
                .ok_or_else(|| format!("Environment process {} is unavailable", task.environment))?;
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

        let mut stream = TcpStream::connect_timeout(&endpoint.address, CONNECT_TIMEOUT)
            .map_err(|error| format!("connect Environment {}: {error}", task.environment))?;
        let io_timeout = Duration::from_millis(task.limits.wall_time_ms.max(1).saturating_add(2_000));
        stream
            .set_read_timeout(Some(io_timeout))
            .map_err(|error| format!("set Environment read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|error| format!("set Environment write timeout: {error}"))?;

        let request_id = task.id.to_string();
        let request = ChildRequest::Execute {
            request_id: request_id.clone(),
            session: endpoint.session,
            environment: task.environment.clone(),
            generation: endpoint.generation,
            artifact_hash: task.artifact_hash.clone(),
            fuel: task.limits.cpu_millis.saturating_mul(10_000).max(1_000_000),
            max_memory_bytes: task.limits.memory_bytes.max(64 * 1024),
            timeout_ms: task.limits.wall_time_ms.max(1),
            input: task.payload.clone(),
        };
        write_frame(&mut stream, &request)
            .map_err(|error| format!("send Environment execution: {error}"))?;
        let response: ChildResponse = read_typed(&mut BufReader::new(stream))
            .map_err(|error| format!("read Environment execution result: {error}"))?;
        match response {
            ChildResponse::Finished {
                request_id: returned,
                output,
            } if returned == request_id => Ok(output),
            ChildResponse::Error {
                request_id: Some(returned),
                code,
                message,
            } if returned == request_id => Err(format!("{code}: {message}")),
            _ => Err("Environment process returned a mismatched response".into()),
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
    let storage = EnvironmentStorageManager::open(storage_root.clone(), bootstrap.storage_limit_bytes)
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
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let bootstrap = Arc::clone(&bootstrap);
                let storage = Arc::clone(&storage);
                thread::spawn(move || {
                    if let Err(error) = handle_child_connection(stream, &bootstrap, &storage) {
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
    storage: &EnvironmentStorageManager,
) -> Result<()> {
    let request: ChildRequest = read_typed(&mut BufReader::new(stream.try_clone()?))?;
    let response = match request {
        ChildRequest::Execute {
            request_id,
            session,
            environment,
            generation,
            artifact_hash,
            fuel,
            max_memory_bytes,
            timeout_ms,
            input,
        } => {
            if !valid_session(bootstrap, &session) {
                child_error(Some(request_id), "AUTH_FAILED", "Environment session rejected")
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(
                    Some(request_id),
                    "ENVIRONMENT_IDENTITY_MISMATCH",
                    "Environment/generation does not match the child process",
                )
            } else if input.len() > MAX_EXECUTION_INPUT_BYTES {
                child_error(
                    Some(request_id),
                    "EXECUTION_INPUT_TOO_LARGE",
                    "Environment invocation exceeds Container input limit",
                )
            } else {
                match execute_isolated_worker(
                    &artifact_hash,
                    fuel,
                    max_memory_bytes,
                    timeout_ms,
                    &input,
                    bootstrap.debug,
                    &bootstrap.environment,
                ) {
                    Ok(output) => ChildResponse::Finished { request_id, output },
                    Err(error) => child_error(Some(request_id), "EXECUTION_FAILED", &error),
                }
            }
        }
        ChildRequest::Ping { request_id, session } => {
            if !valid_session(bootstrap, &session) {
                child_error(Some(request_id), "AUTH_FAILED", "Environment session rejected")
            } else {
                ChildResponse::Pong {
                    request_id,
                    pid: std::process::id(),
                    generation: bootstrap.generation,
                }
            }
        }
        ChildRequest::ResetVolatile { request_id, session } => {
            if !valid_session(bootstrap, &session) {
                child_error(Some(request_id), "AUTH_FAILED", "Environment session rejected")
            } else {
                match storage.reset_volatile() {
                    Ok(()) => ChildResponse::VolatileReset { request_id },
                    Err(error) => child_error(
                        Some(request_id),
                        "STORAGE_RESET_FAILED",
                        &error.to_string(),
                    ),
                }
            }
        }
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
    artifact_hash: &str,
    fuel: u64,
    max_memory_bytes: u64,
    timeout_ms: u64,
    input: &[u8],
    debug: bool,
    environment: &str,
) -> Result<Vec<u8>, String> {
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
        .stderr(if debug { Stdio::inherit() } else { Stdio::null() });
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut worker_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Environment worker stdin unavailable".to_string())?;
    if let Err(error) = write_worker_input(&mut worker_stdin, input) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("write Environment worker input: {error}"));
    }
    drop(worker_stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Environment worker stdout unavailable".to_string())?;
    let reader = thread::Builder::new()
        .name("rbe-env-worker-result".into())
        .spawn(move || read_worker_result(&mut BufReader::new(stdout)))
        .map_err(|error| error.to_string())?;
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(timeout_ms.max(1));
    loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(format!("Environment worker timed out after {timeout_ms} ms"));
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => {
                let frame = reader
                    .join()
                    .map_err(|_| "Environment worker result reader panicked".to_string())?
                    .map_err(|error| format!("invalid Environment worker result: {error}"))?;
                if !status.success() {
                    return Err(format!("Environment worker exited with status {status}"));
                }
                return match frame {
                    WorkerResultFrame::Success(output) => Ok(output),
                    WorkerResultFrame::Error(message) => Err(message),
                };
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
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
        || !bootstrap.session.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    let mut ids = EnvironmentId::GENERAL
        [..general_count.clamp(1, EnvironmentId::GENERAL.len())]
        .to_vec();
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
        assert_eq!(parse_environment("general-1"), Some(EnvironmentId::General1));
        assert_eq!(parse_environment("payment"), Some(EnvironmentId::Payment));
        assert_eq!(parse_environment("visitor-ip-123"), None);
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
'''
Path("container-runtime/crates/container-bin/src/environment_process.rs").write_text(module, encoding="utf-8")

# Wire the controller startup, health, inspect and restart paths.
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "mod dashboard;\n",
    "mod dashboard;\nmod environment_process;\n",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "    if args.iter().any(|arg| arg == \"--monitor\") {\n        return run_monitor(&args);\n    }\n",
    "    if args.iter().any(|arg| arg == \"--environment-child\") {\n        return environment_process::run_environment_child();\n    }\n    if args.iter().any(|arg| arg == \"--monitor\") {\n        return run_monitor(&args);\n    }\n",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "    let runtime = Runtime::new(RuntimeConfig {\n        general_environments,\n        swamps_per_environment,\n        workers_per_swamp,\n        rebalance_interval_ms: 25,\n    });\n    let capability_broker = Arc::new(CapabilityBroker::new(debug));\n    let accepting = Arc::new(AtomicBool::new(true));",
    "    let token = env::var(\"RBE_CONTAINER_TOKEN\").ok();\n    let environment_processes = environment_process::EnvironmentProcessSupervisor::start(\n        general_environments,\n        debug,\n        token.as_deref(),\n    )?;\n    let runtime = Runtime::new_with_runner(\n        RuntimeConfig {\n            general_environments,\n            swamps_per_environment,\n            workers_per_swamp,\n            rebalance_interval_ms: 25,\n        },\n        environment_processes.runner(),\n    );\n    let capability_broker = Arc::new(CapabilityBroker::new(debug));\n    let accepting = Arc::new(AtomicBool::new(true));",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "    let token = env::var(\"RBE_CONTAINER_TOKEN\").ok();\n    if !dashboard_disabled {",
    "    if !dashboard_disabled {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "            capability_broker,\n        )?;",
    "            capability_broker,\n            environment_processes,\n        )?;",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "fn run_control_server(\n    address: &str,\n    token: String,\n    runtime: Arc<Runtime>,\n    accepting: Arc<AtomicBool>,\n    capability_broker: Arc<CapabilityBroker>,\n) -> anyhow::Result<()> {",
    "fn run_control_server(\n    address: &str,\n    token: String,\n    runtime: Arc<Runtime>,\n    accepting: Arc<AtomicBool>,\n    capability_broker: Arc<CapabilityBroker>,\n    environment_processes: Arc<environment_process::EnvironmentProcessSupervisor>,\n) -> anyhow::Result<()> {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "                let capability_broker = Arc::clone(&capability_broker);\n                thread::spawn(move || {\n                    if let Err(err) =\n                        handle_connection(stream, &token, &runtime, &accepting, &capability_broker)\n                    {",
    "                let capability_broker = Arc::clone(&capability_broker);\n                let environment_processes = Arc::clone(&environment_processes);\n                thread::spawn(move || {\n                    if let Err(err) = handle_connection(\n                        stream,\n                        &token,\n                        &runtime,\n                        &accepting,\n                        &capability_broker,\n                        &environment_processes,\n                    ) {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "fn handle_connection(\n    mut stream: TcpStream,\n    token: &str,\n    runtime: &Runtime,\n    accepting: &AtomicBool,\n    capability_broker: &CapabilityBroker,\n) -> anyhow::Result<()> {",
    "fn handle_connection(\n    mut stream: TcpStream,\n    token: &str,\n    runtime: &Runtime,\n    accepting: &AtomicBool,\n    capability_broker: &CapabilityBroker,\n    environment_processes: &environment_process::EnvironmentProcessSupervisor,\n) -> anyhow::Result<()> {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "                let snapshots = runtime.snapshots();\n                let (swamps, workers, busy, completed, failed) = topology_totals(&snapshots);\n                Response::Health {",
    "                let snapshots = runtime.snapshots();\n                let process_snapshots = environment_processes.snapshots();\n                let environment_processes_alive =\n                    process_snapshots.iter().filter(|process| process.alive).count();\n                let (swamps, workers, busy, completed, failed) = topology_totals(&snapshots);\n                Response::Health {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    '                        "capability_manifests": capability_broker.manifest_count(),\n                        "process": "container",',
    '                        "capability_manifests": capability_broker.manifest_count(),\n                        "environment_processes": process_snapshots.len(),\n                        "environment_processes_alive": environment_processes_alive,\n                        "process": "container",',
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "                let requeued = runtime.restart_environment(environment);\n                emit_event(\n                    \"environment_restart\",\n                    &format!(\n                        \"environment={environment} requeued={requeued} revoked_manifests={revoked}\"\n                    ),\n                );\n                Response::Restarted {",
    "                let requeued = runtime.restart_environment(environment);\n                let generation = runtime.environment_generation(environment);\n                if let Err(error) = environment_processes.restart(environment, generation) {\n                    emit_event(\n                        \"environment_process_restart_failed\",\n                        &format!(\"environment={environment} generation={generation} error={error}\"),\n                    );\n                    return write_frame(\n                        &mut stream,\n                        &Response::Error {\n                            request_id: Some(request.request_id),\n                            code: \"ENVIRONMENT_PROCESS_RESTART_FAILED\".into(),\n                            message: error.to_string(),\n                        },\n                    )\n                    .map_err(Into::into);\n                }\n                emit_event(\n                    \"environment_restart\",\n                    &format!(\n                        \"environment={environment} generation={generation} requeued={requeued} revoked_manifests={revoked}\"\n                    ),\n                );\n                Response::Restarted {",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "                    inspection_body(\n                        runtime,\n                        request.execution_id,\n                        accepting.load(Ordering::Acquire),\n                    ),",
    "                    inspection_body(\n                        runtime,\n                        request.execution_id,\n                        accepting.load(Ordering::Acquire),\n                        environment_processes,\n                    ),",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    "fn inspection_body(\n    runtime: &Runtime,\n    execution_id: Option<String>,\n    accepting: bool,\n) -> serde_json::Value {\n    let snapshots = runtime.snapshots();",
    "fn inspection_body(\n    runtime: &Runtime,\n    execution_id: Option<String>,\n    accepting: bool,\n    environment_processes: &environment_process::EnvironmentProcessSupervisor,\n) -> serde_json::Value {\n    let snapshots = runtime.snapshots();\n    let environment_processes = environment_processes\n        .snapshots()\n        .into_iter()\n        .map(|process| serde_json::json!({\n            \"environment\": process.environment,\n            \"pid\": process.pid,\n            \"generation\": process.generation,\n            \"address\": process.address,\n            \"alive\": process.alive,\n            \"debug\": process.debug\n        }))\n        .collect::<Vec<_>>();",
)
replace_once(
    "container-runtime/crates/container-bin/src/main.rs",
    '        "security": {\n            "policy": "deny-by-default",',
    '        "environment_processes": environment_processes,\n        "security": {\n            "policy": "deny-by-default",\n            "environment_boundary": "controller -> Environment process -> disposable WASM worker",',
)

# Keep docs honest: Swamps are still controller schedulers in this slice, while
# executable work now crosses a persistent Environment process boundary.
readme = Path("container-runtime/README.md")
text = readme.read_text(encoding="utf-8")
anchor = "## Swamps and workers\n"
if anchor not in text:
    raise SystemExit("missing README Swamps anchor")
insert = '''## Environment process boundary\n\nThe standalone Container Controller now launches one persistent child `container` process per configured Environment. Swamps remain controller-side schedulers in this slice, but real WASM execution is forwarded over a localhost, session-authenticated child channel and the Environment process launches the disposable worker. The session capability is delivered over inherited bootstrap stdin, is distinct from `RBE_CONTAINER_TOKEN`, and the child exits when the Controller liveness pipe closes. `--debug` is propagated visibly to children/workers but is not itself the authority.\n\nThis is intentionally an intermediate ownership step: transactional Environment storage is initialized in the Environment process and the child listener is the future attachment point for the authenticated Unix-like debug shell and Container Controller capability calls. Later slices can move Swamp ownership itself behind the same process boundary without changing the external Container IPC.\n\n'''
readme.write_text(text.replace(anchor, insert + anchor, 1), encoding="utf-8")
