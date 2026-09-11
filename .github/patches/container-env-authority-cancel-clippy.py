from pathlib import Path

path = Path('container-runtime/crates/container-bin/src/environment_process.rs')
text = path.read_text(encoding='utf-8')


def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f'expected one repair anchor, found {count}: {old[:120]!r}')
    text = text.replace(old, new, 1)


replace_once(
'''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, u64>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}

struct ManagedEnvironment {''',
'''struct EnvironmentChildState {
    storage: Arc<EnvironmentStorageManager>,
    active_executions: Mutex<HashMap<String, u64>>,
    cancelled: Mutex<HashMap<String, Instant>>,
}

struct WorkerExecution<'a> {
    execution_id: &'a str,
    generation: u64,
    artifact_hash: &'a str,
    fuel: u64,
    max_memory_bytes: u64,
    timeout_ms: u64,
    input: &'a [u8],
    debug: bool,
    environment: &'a str,
}

struct ManagedEnvironment {''')

replace_once(
'''                match execute_isolated_worker(
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
                ) {''',
'''                let worker = WorkerExecution {
                    execution_id: &request_id,
                    generation,
                    artifact_hash: &artifact_hash,
                    fuel,
                    max_memory_bytes,
                    timeout_ms,
                    input: &input,
                    debug: bootstrap.debug,
                    environment: &bootstrap.environment,
                };
                match execute_isolated_worker(worker, state) {''')

replace_once(
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
) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;''',
'''fn execute_isolated_worker(
    worker: WorkerExecution<'_>,
    state: &EnvironmentChildState,
) -> Result<Vec<u8>, String> {
    let WorkerExecution {
        execution_id,
        generation,
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
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;''')

# Once an execution is registered as active, every early worker-input failure
# must release that ownership before returning. Otherwise inspect/cancel state
# can retain a ghost execution after a broken pipe.
replace_once(
'''    if let Err(error) = write_worker_input(&mut worker_stdin, input) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("write Environment worker input: {error}"));
    }''',
'''    if let Err(error) = write_worker_input(&mut worker_stdin, input) {
        let _ = child.kill();
        let _ = child.wait();
        active_executions
            .lock()
            .map_err(|_| "Environment active execution table poisoned".to_string())?
            .remove(execution_id);
        let _ = take_cancelled(cancelled, execution_id);
        return Err(format!("write Environment worker input: {error}"));
    }''')

path.write_text(text, encoding='utf-8')
