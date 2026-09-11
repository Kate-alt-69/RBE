from pathlib import Path


def rep(path, old, new, count=1):
    p = Path(path)
    text = p.read_text(encoding='utf-8')
    found = text.count(old)
    if found != count:
        raise SystemExit(f'{path}: expected {count} anchors, found {found}: {old[:100]!r}')
    p.write_text(text.replace(old, new, count), encoding='utf-8')

# Runtime hook + external-storage mode.
worker = 'container-runtime/crates/container-runtime-core/src/worker.rs'
rep(worker,
    "pub type Runner = Arc<dyn Fn(&ExecutionTask) -> Result<Vec<u8>, String> + Send + Sync + 'static>;\n",
    "pub type Runner = Arc<dyn Fn(&ExecutionTask) -> Result<Vec<u8>, String> + Send + Sync + 'static>;\n"
    "pub type Canceller = Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync + 'static>;\n")

lib = 'container-runtime/crates/container-runtime-core/src/lib.rs'
rep(lib, 'pub use worker::{Runner, WorkerSnapshot, WorkerState};',
         'pub use worker::{Canceller, Runner, WorkerSnapshot, WorkerState};')

env = 'container-runtime/crates/container-runtime-core/src/environment.rs'
rep(env, '    storage_manager: Arc<EnvironmentStorageManager>,',
         '    storage_manager: Option<Arc<EnvironmentStorageManager>>,')
rep(env, '        storage: EnvironmentStorage,\n        runner: Runner,',
         '        storage: EnvironmentStorage,\n        manage_storage_locally: bool,\n        runner: Runner,')
rep(env,
'''        let storage_manager =
            EnvironmentStorageManager::open(storage_path.clone(), storage.limit_bytes)
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to initialize transactional storage for Environment {id}: {error}"
                    )
                });
''',
'''        let storage_manager = manage_storage_locally.then(|| {
            EnvironmentStorageManager::open(storage_path.clone(), storage.limit_bytes)
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to initialize transactional storage for Environment {id}: {error}"
                    )
                })
        });
''')
rep(env,
'''        if self.storage.ephemeral {
            self.storage_manager
                .reset_volatile()
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to reset volatile storage for Environment {}: {error}",
                        self.id
                    )
                });
        }
''',
'''        if self.storage.ephemeral {
            if let Some(storage_manager) = self.storage_manager.as_ref() {
                storage_manager.reset_volatile().unwrap_or_else(|error| {
                    panic!(
                        "failed to reset volatile storage for Environment {}: {error}",
                        self.id
                    )
                });
            }
        }
''')
rep(env,
'''    pub fn storage(&self) -> Arc<EnvironmentStorageManager> {
        Arc::clone(&self.storage_manager)
    }
''',
'''    pub fn storage(&self) -> Option<Arc<EnvironmentStorageManager>> {
        self.storage_manager.as_ref().map(Arc::clone)
    }
''')

runtime = 'container-runtime/crates/container-runtime-core/src/runtime.rs'
rep(runtime, 'use crate::worker::{Completion, Runner, WorkerState};',
             'use crate::worker::{Canceller, Completion, Runner, WorkerState};')
rep(runtime, '    results: SharedResults,\n}',
             '    results: SharedResults,\n    artifact_canceller: Option<Canceller>,\n}')
rep(runtime,
'''    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        Self::build(config, None)
    }

    /// Build the scheduler with an external artifact runner. The standalone
    /// Container Controller uses this to route executable work through the
    /// dedicated per-Environment `container` child process. Tests and library
    /// embedders may keep using [`Runtime::new`] and the legacy local runner.
    pub fn new_with_runner(config: RuntimeConfig, artifact_runner: Runner) -> Arc<Self> {
        Self::build(config, Some(artifact_runner))
    }

    fn build(config: RuntimeConfig, artifact_runner: Option<Runner>) -> Arc<Self> {
''',
'''    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        Self::build(config, None, None)
    }

    /// Build with an external Environment runner and hard-cancellation hook.
    pub fn new_with_runner(
        config: RuntimeConfig,
        artifact_runner: Runner,
        artifact_canceller: Canceller,
    ) -> Arc<Self> {
        Self::build(config, Some(artifact_runner), Some(artifact_canceller))
    }

    fn build(
        config: RuntimeConfig,
        artifact_runner: Option<Runner>,
        artifact_canceller: Option<Canceller>,
    ) -> Arc<Self> {
''')
rep(runtime,
    '        let active_ids = active_environment_ids(config.general_environments);\n        let cache = Arc::new(ArtifactCache::default());',
    '        let active_ids = active_environment_ids(config.general_environments);\n'
    '        let manage_storage_locally = artifact_runner.is_none();\n'
    '        let cache = Arc::new(ArtifactCache::default());')
rep(runtime,
'''                    EnvironmentStorage {
                        limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
                        ephemeral: true,
                    },
                    Arc::clone(&runner),
''',
'''                    EnvironmentStorage {
                        limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
                        ephemeral: true,
                    },
                    manage_storage_locally,
                    Arc::clone(&runner),
''')
rep(runtime,
'''            journal,
            results,
        });
''',
'''            journal,
            results,
            artifact_canceller,
        });
''')
rep(runtime,
'''        if running {
            self.cancelled
                .lock()
                .expect("cancel table poisoned")
                .insert(execution_id.to_string());
        }
''',
'''        if running {
            self.cancelled
                .lock()
                .expect("cancel table poisoned")
                .insert(execution_id.to_string());
            if let Some(canceller) = self.artifact_canceller.as_ref() {
                if let Err(error) = canceller(execution_id) {
                    tracing::warn!(execution = execution_id, "Environment hard-cancel failed: {error}");
                }
            }
        }
''')
rep(runtime,
'''    pub fn environment_storage(
        &self,
        id: EnvironmentId,
    ) -> Option<Arc<crate::storage::EnvironmentStorageManager>> {
        self.environment(id).map(EnvironmentRuntime::storage)
    }
''',
'''    pub fn environment_storage(
        &self,
        id: EnvironmentId,
    ) -> Option<Arc<crate::storage::EnvironmentStorageManager>> {
        self.environment(id).and_then(EnvironmentRuntime::storage)
    }
''')

main = 'container-runtime/crates/container-bin/src/main.rs'
rep(main,
'''        },
        environment_processes.runner(),
    );
''',
'''        },
        environment_processes.runner(),
        environment_processes.canceller(),
    );
''')

# Environment child protocol: authenticated, generation-bound cancellation.
ep = 'container-runtime/crates/container-bin/src/environment_process.rs'
rep(ep,
'''use container_runtime_core::{
    EnvironmentId, EnvironmentStorageManager, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};''',
'''use container_runtime_core::{
    Canceller, EnvironmentId, EnvironmentStorageManager, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};''')
rep(ep, 'use std::time::{Duration, SystemTime, UNIX_EPOCH};',
        'use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};')
rep(ep, 'const SESSION_MAX_BYTES: usize = 128;',
        'const SESSION_MAX_BYTES: usize = 128;\nconst PENDING_CANCEL_TTL: Duration = Duration::from_secs(30);')
rep(ep,
'''    ResetVolatile {
        request_id: String,
        session: String,
    },
}''',
'''    ResetVolatile {
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
}''')
rep(ep,
'''    VolatileReset {
        request_id: String,
    },
    Error {''',
'''    VolatileReset {
        request_id: String,
    },
    CancelAccepted {
        request_id: String,
        execution_id: String,
        active: bool,
    },
    Error {''')
rep(ep,
'''struct Endpoint {
    address: SocketAddr,
    session: String,
    generation: u64,
}

struct ManagedEnvironment {''',
'''struct Endpoint {
    address: SocketAddr,
    session: String,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct ExecutionOwner {
    environment: EnvironmentId,
    generation: u64,
}

struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, u64>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}

struct ManagedEnvironment {''')
rep(ep,
'''    next_session: AtomicU64,
    processes: Mutex<HashMap<EnvironmentId, ManagedEnvironment>>,
}''',
'''    next_session: AtomicU64,
    processes: Mutex<HashMap<EnvironmentId, ManagedEnvironment>>,
    executions: Mutex<HashMap<String, ExecutionOwner>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}''')
rep(ep,
'''            next_session: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
        });''',
'''            next_session: AtomicU64::new(1),
            processes: Mutex::new(HashMap::new()),
            executions: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashMap::new()),
        });''')
rep(ep,
'''    pub fn runner(self: &Arc<Self>) -> Runner {
        let supervisor = Arc::clone(self);
        Arc::new(move |task| supervisor.execute(task))
    }

    pub fn restart(&self, id: EnvironmentId, generation: u64) -> Result<()> {''',
'''    pub fn runner(self: &Arc<Self>) -> Runner {
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
        stream.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
        let request_id = format!("cancel-{execution_id}");
        write_frame(&mut stream, &ChildRequest::Cancel {
            request_id: request_id.clone(),
            session: endpoint.session,
            environment: owner.environment.to_string(),
            generation: owner.generation,
            execution_id: execution_id.to_string(),
        }).map_err(|e| format!("send Environment cancellation: {e}"))?;
        match read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
            .map_err(|e| format!("read Environment cancellation: {e}"))? {
            ChildResponse::CancelAccepted { request_id: returned, execution_id: returned_exec, .. }
                if returned == request_id && returned_exec == execution_id => Ok(true),
            ChildResponse::Error { request_id: Some(returned), code, message }
                if returned == request_id => Err(format!("{code}: {message}")),
            _ => Err("Environment returned a mismatched cancellation response".into()),
        }
    }

    pub fn restart(&self, id: EnvironmentId, generation: u64) -> Result<()> {''')

# Register Controller-side execution ownership around the existing IPC call.
rep(ep,
'''        let request_id = task.id.to_string();
        let request = ChildRequest::Execute {''',
'''        let request_id = task.id.to_string();
        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .insert(request_id.clone(), ExecutionOwner { environment: id, generation: endpoint.generation });
        if take_cancelled(&self.cancelled, &request_id)? {
            self.executions
                .lock()
                .map_err(|_| "Environment execution ownership table poisoned".to_string())?
                .remove(&request_id);
            return Err("execution cancelled before Environment dispatch".into());
        }
        let request = ChildRequest::Execute {''')
rep(ep,
'''        let response: ChildResponse = read_typed(&mut BufReader::new(stream))
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
''',
'''        let result = read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
            .map_err(|error| format!("read Environment execution result: {error}"))
            .and_then(|response| match response {
                ChildResponse::Finished { request_id: returned, output } if returned == request_id => Ok(output),
                ChildResponse::Error { request_id: Some(returned), code, message } if returned == request_id => Err(format!("{code}: {message}")),
                _ => Err("Environment process returned a mismatched response".into()),
            });
        self.executions
            .lock()
            .map_err(|_| "Environment execution ownership table poisoned".to_string())?
            .remove(&request_id);
        let _ = take_cancelled(&self.cancelled, &request_id);
        result
''')

# Make child storage + execution/cancel state process-owned.
rep(ep,
'''    let bootstrap = Arc::new(bootstrap);
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
''',
'''    let bootstrap = Arc::new(bootstrap);
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
''')
rep(ep,
'''fn handle_child_connection(
    mut stream: TcpStream,
    bootstrap: &Bootstrap,
    storage: &EnvironmentStorageManager,
) -> Result<()> {''',
'''fn handle_child_connection(
    mut stream: TcpStream,
    bootstrap: &Bootstrap,
    state: &EnvironmentChildState,
) -> Result<()> {''')
rep(ep, '                match storage.reset_volatile() {',
        '                match state.storage.reset_volatile() {')

# Execute checks pending cancellation, then worker polls it while running.
rep(ep,
'''            } else {
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
        ChildRequest::Ping {''',
'''            } else if take_cancelled(&state.cancelled, &request_id).unwrap_or(false) {
                child_error(Some(request_id), "EXECUTION_CANCELLED", "execution cancelled before worker start")
            } else {
                match execute_isolated_worker(
                    &request_id,
                    generation,
                    &artifact_hash,
                    fuel,
                    max_memory_bytes,
                    timeout_ms,
                    &input,
                    bootstrap.debug,
                    &bootstrap.environment,
                    &state.active_executions,
                    &state.cancelled,
                ) {
                    Ok(output) => ChildResponse::Finished { request_id, output },
                    Err(error) if error == "execution cancelled" => child_error(Some(request_id), "EXECUTION_CANCELLED", &error),
                    Err(error) => child_error(Some(request_id), "EXECUTION_FAILED", &error),
                }
            }
        }
        ChildRequest::Ping {''')
rep(ep,
'''        ChildRequest::Bootstrap(_) => child_error(
            None,
            "BOOTSTRAP_REPLAY",
            "Environment bootstrap is only accepted on inherited stdin",
        ),''',
'''        ChildRequest::Cancel { request_id, session, environment, generation, execution_id } => {
            if !valid_session(bootstrap, &session) {
                child_error(Some(request_id), "AUTH_FAILED", "Environment session rejected")
            } else if environment != bootstrap.environment || generation != bootstrap.generation {
                child_error(Some(request_id), "ENVIRONMENT_IDENTITY_MISMATCH", "Environment/generation does not match the child process")
            } else if let Err(error) = mark_cancelled(&state.cancelled, &execution_id) {
                child_error(Some(request_id), "CANCEL_STATE_FAILED", &error)
            } else {
                let active = state.active_executions
                    .lock()
                    .map_err(|_| anyhow!("Environment active execution table poisoned"))?
                    .get(&execution_id)
                    .copied() == Some(generation);
                ChildResponse::CancelAccepted { request_id, execution_id, active }
            }
        }
        ChildRequest::Bootstrap(_) => child_error(
            None,
            "BOOTSTRAP_REPLAY",
            "Environment bootstrap is only accepted on inherited stdin",
        ),''')

rep(ep,
'''fn execute_isolated_worker(
    artifact_hash: &str,
    fuel: u64,
    max_memory_bytes: u64,
    timeout_ms: u64,
    input: &[u8],
    debug: bool,
    environment: &str,
) -> Result<Vec<u8>, String> {''',
'''fn execute_isolated_worker(
    execution_id: &str,
    generation: u64,
    artifact_hash: &str,
    fuel: u64,
    max_memory_bytes: u64,
    timeout_ms: u64,
    input: &[u8],
    debug: bool,
    environment: &str,
    active_executions: &Mutex<HashMap<String, u64>>,
    cancelled: &Mutex<HashMap<String, Instant>>,
) -> Result<Vec<u8>, String> {''')
rep(ep,
'''    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut worker_stdin = child''',
'''    let mut child = command.spawn().map_err(|error| error.to_string())?;
    active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .insert(execution_id.to_string(), generation);
    if take_cancelled(cancelled, execution_id)? {
        let _ = child.kill();
        let _ = child.wait();
        active_executions.lock().map_err(|_| "Environment active execution table poisoned".to_string())?.remove(execution_id);
        return Err("execution cancelled".into());
    }
    let mut worker_stdin = child''')
rep(ep,
'''    loop {
        if started.elapsed() >= timeout {''',
'''    loop {
        if is_cancelled(cancelled, execution_id)? {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            active_executions.lock().map_err(|_| "Environment active execution table poisoned".to_string())?.remove(execution_id);
            let _ = take_cancelled(cancelled, execution_id);
            return Err("execution cancelled".into());
        }
        if started.elapsed() >= timeout {''')
rep(ep,
'''            let _ = reader.join();
            return Err(format!(
                "Environment worker timed out after {timeout_ms} ms"
            ));''',
'''            let _ = reader.join();
            active_executions.lock().map_err(|_| "Environment active execution table poisoned".to_string())?.remove(execution_id);
            return Err(format!(
                "Environment worker timed out after {timeout_ms} ms"
            ));''')
rep(ep,
'''                if !status.success() {
                    return Err(format!("Environment worker exited with status {status}"));
                }
                return match frame {
                    WorkerResultFrame::Success(output) => Ok(output),
                    WorkerResultFrame::Error(message) => Err(message),
                };''',
'''                active_executions.lock().map_err(|_| "Environment active execution table poisoned".to_string())?.remove(execution_id);
                let _ = take_cancelled(cancelled, execution_id);
                if !status.success() {
                    return Err(format!("Environment worker exited with status {status}"));
                }
                return match frame {
                    WorkerResultFrame::Success(output) => Ok(output),
                    WorkerResultFrame::Error(message) => Err(message),
                };''')

# Shared pending-cancel helpers close races before child dispatch/worker spawn.
rep(ep,
'''fn child_error(request_id: Option<String>, code: &str, message: &str) -> ChildResponse {''',
'''fn mark_cancelled(table: &Mutex<HashMap<String, Instant>>, execution_id: &str) -> Result<(), String> {
    let mut table = table.lock().map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    table.insert(execution_id.to_string(), Instant::now());
    Ok(())
}

fn take_cancelled(table: &Mutex<HashMap<String, Instant>>, execution_id: &str) -> Result<bool, String> {
    let mut table = table.lock().map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    Ok(table.remove(execution_id).is_some())
}

fn is_cancelled(table: &Mutex<HashMap<String, Instant>>, execution_id: &str) -> Result<bool, String> {
    let mut table = table.lock().map_err(|_| "Environment cancellation table poisoned".to_string())?;
    table.retain(|_, created| created.elapsed() < PENDING_CANCEL_TTL);
    Ok(table.contains_key(execution_id))
}

fn child_error(request_id: Option<String>, code: &str, message: &str) -> ChildResponse {''')

# Document the completed ownership boundary.
readme = 'container-runtime/README.md'
rep(readme,
'''This is intentionally an intermediate ownership step: transactional Environment storage is initialized in the Environment process and the child listener is the future attachment point for the authenticated Unix-like debug shell and Container Controller capability calls. Later slices can move Swamp ownership itself behind the same process boundary without changing the external Container IPC.
''',
'''Transactional Environment storage is initialized only in the Environment process when the standalone Controller uses the process runner; the Controller scheduler keeps only storage metadata and never opens a competing writer. Cancellation is generation-bound and crosses the authenticated child channel, where the Environment execution loop kills the matching disposable WASM worker. The child listener remains the attachment point for the authenticated Unix-like debug shell and Container Controller capability calls.
''')
