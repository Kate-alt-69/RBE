from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:160]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# Private parent <-> worker pipe protocol. Invocation bytes never travel through
# argv/env or temporary files, and result/error payloads are independently
# bounded.
# ---------------------------------------------------------------------------
ipc = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
text = ipc.read_text(encoding="utf-8")
text = text.replace(
    "pub const MAX_AWAIT_RESULT_MS: u64 = 30_000;",
    "pub const MAX_AWAIT_RESULT_MS: u64 = 30_000;\n"
    "pub const MAX_WORKER_ERROR_BYTES: usize = 64 * 1024;\n"
    "const WORKER_PIPE_MAGIC: [u8; 4] = *b\"RBW1\";\n"
    "const WORKER_STATUS_SUCCESS: u8 = 0;\n"
    "const WORKER_STATUS_ERROR: u8 = 1;",
    1,
)
anchor = """#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
"""
insert = """#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerResultFrame {
    Success(Vec<u8>),
    Error(String),
}

"""
if anchor not in text:
    raise SystemExit("worker result enum anchor missing")
text = text.replace(anchor, insert + anchor, 1)
anchor = """pub fn decode_request(bytes: &[u8]) -> io::Result<Request> {
"""
helpers = r'''pub fn write_worker_input<W: Write>(writer: &mut W, input: &[u8]) -> io::Result<()> {
    if input.len() > MAX_EXECUTION_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker invocation input exceeds maximum size",
        ));
    }
    writer.write_all(&WORKER_PIPE_MAGIC)?;
    writer.write_all(&(input.len() as u32).to_be_bytes())?;
    writer.write_all(input)?;
    writer.flush()
}

pub fn read_worker_input<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    expect_worker_magic(reader)?;
    let length = read_worker_length(reader, MAX_EXECUTION_INPUT_BYTES, "worker invocation input")?;
    let mut input = vec![0u8; length];
    reader.read_exact(&mut input)?;
    Ok(input)
}

pub fn write_worker_result<W: Write>(
    writer: &mut W,
    result: &WorkerResultFrame,
) -> io::Result<()> {
    writer.write_all(&WORKER_PIPE_MAGIC)?;
    match result {
        WorkerResultFrame::Success(output) => {
            if output.len() > MAX_EXECUTION_OUTPUT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker execution output exceeds maximum size",
                ));
            }
            writer.write_all(&[WORKER_STATUS_SUCCESS])?;
            writer.write_all(&(output.len() as u32).to_be_bytes())?;
            writer.write_all(output)?;
        }
        WorkerResultFrame::Error(message) => {
            let bytes = message.as_bytes();
            if bytes.len() > MAX_WORKER_ERROR_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker error payload exceeds maximum size",
                ));
            }
            writer.write_all(&[WORKER_STATUS_ERROR])?;
            writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
            writer.write_all(bytes)?;
        }
    }
    writer.flush()
}

pub fn read_worker_result<R: Read>(reader: &mut R) -> io::Result<WorkerResultFrame> {
    expect_worker_magic(reader)?;
    let mut status = [0u8; 1];
    reader.read_exact(&mut status)?;
    match status[0] {
        WORKER_STATUS_SUCCESS => {
            let length = read_worker_length(reader, MAX_EXECUTION_OUTPUT_BYTES, "worker output")?;
            let mut output = vec![0u8; length];
            reader.read_exact(&mut output)?;
            Ok(WorkerResultFrame::Success(output))
        }
        WORKER_STATUS_ERROR => {
            let length = read_worker_length(reader, MAX_WORKER_ERROR_BYTES, "worker error")?;
            let mut bytes = vec![0u8; length];
            reader.read_exact(&mut bytes)?;
            let message = String::from_utf8(bytes).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("worker error is not UTF-8: {error}"))
            })?;
            Ok(WorkerResultFrame::Error(message))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown worker result status {other}"),
        )),
    }
}

fn expect_worker_magic<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic != WORKER_PIPE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid worker pipe protocol magic",
        ));
    }
    Ok(())
}

fn read_worker_length<R: Read>(reader: &mut R, max: usize, label: &str) -> io::Result<usize> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} exceeds maximum size"),
        ));
    }
    Ok(length)
}

'''
if anchor not in text:
    raise SystemExit("worker pipe helper anchor missing")
text = text.replace(anchor, helpers + anchor, 1)
marker = "\n    #[test]\n    fn rejects_zero_length_frame() {"
tests = r'''

    #[test]
    fn worker_pipe_round_trips_binary_input_and_output() {
        let input = b"request\0bytes".to_vec();
        let mut encoded = Vec::new();
        write_worker_input(&mut encoded, &input).unwrap();
        assert_eq!(read_worker_input(&mut encoded.as_slice()).unwrap(), input);

        let frame = WorkerResultFrame::Success(b"response\0bytes".to_vec());
        let mut encoded = Vec::new();
        write_worker_result(&mut encoded, &frame).unwrap();
        assert_eq!(read_worker_result(&mut encoded.as_slice()).unwrap(), frame);
    }

    #[test]
    fn worker_pipe_rejects_oversized_lengths_before_allocating() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&WORKER_PIPE_MAGIC);
        encoded.extend_from_slice(&((MAX_EXECUTION_INPUT_BYTES as u32) + 1).to_be_bytes());
        assert!(read_worker_input(&mut encoded.as_slice()).is_err());
    }
'''
if marker not in text:
    raise SystemExit("worker pipe test insertion anchor missing")
text = text.replace(marker, tests + marker, 1)
ipc.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Wasmtime execution ABI. `run() -> i32` stays stable, while the module may use
# rbe.input_len/input_read/output_write host imports over its exported memory.
# ---------------------------------------------------------------------------
engine = Path("container-runtime/crates/execution-engine/src/lib.rs")
engine.write_text(r'''//! Real WASM execution for container workers.
//!
//! RBE modules export `run() -> i32`. Invocation data and bounded output are
//! exchanged through the small `rbe` host ABI; arbitrary host state, sockets,
//! environment variables, and files are not exposed to the guest.

use anyhow::Result;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub fuel: u64,
    pub max_memory_bytes: u64,
    pub max_output_bytes: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_output_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub fuel_consumed: u64,
    pub output: Vec<u8>,
}

struct ExecutionState {
    limits: StoreLimits,
    input: Vec<u8>,
    output: Vec<u8>,
    max_output_bytes: usize,
    abi_error: Option<String>,
}

pub struct WasmExecutor {
    engine: Engine,
}

impl WasmExecutor {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.cranelift_nan_canonicalization(true);
        let engine = Engine::new(&config)
            .map_err(|error| anyhow::anyhow!("initialize Wasmtime engine: {error}"))?;
        Ok(Self { engine })
    }

    pub fn execute(&self, wasm: &[u8], limits: ExecutionLimits) -> Result<ExecutionResult> {
        self.execute_with_input(wasm, &[], limits)
    }

    pub fn execute_with_input(
        &self,
        wasm: &[u8],
        input: &[u8],
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|error| anyhow::anyhow!("compile WASM artifact: {error}"))?;

        let memory_limit = usize::try_from(limits.max_memory_bytes).map_err(|_| {
            anyhow::anyhow!("WASM memory limit does not fit this platform's address space")
        })?;
        let max_output_bytes = usize::try_from(limits.max_output_bytes).map_err(|_| {
            anyhow::anyhow!("WASM output limit does not fit this platform's address space")
        })?;
        if input.len() > i32::MAX as usize {
            anyhow::bail!("WASM invocation input is too large for the RBE ABI");
        }

        let store_limits: StoreLimits = StoreLimitsBuilder::new().memory_size(memory_limit).build();
        let mut store = Store::new(
            &self.engine,
            ExecutionState {
                limits: store_limits,
                input: input.to_vec(),
                output: Vec::new(),
                max_output_bytes,
                abi_error: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|error| anyhow::anyhow!("configure WASM fuel limit: {error}"))?;

        let mut linker = Linker::new(&self.engine);
        linker.func_wrap(
            "rbe",
            "input_len",
            |caller: Caller<'_, ExecutionState>| -> i32 {
                i32::try_from(caller.data().input.len()).unwrap_or(i32::MAX)
            },
        )?;
        linker.func_wrap(
            "rbe",
            "input_read",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, capacity: i32| -> i32 {
                if ptr < 0 || capacity < 0 {
                    return set_abi_error(&mut caller, "input_read received a negative pointer or capacity");
                }
                let input = caller.data().input.clone();
                if capacity as usize < input.len() {
                    return set_abi_error(&mut caller, "input_read capacity is smaller than invocation input");
                }
                let Some(memory) = caller.get_export("memory").and_then(|export| export.into_memory()) else {
                    return set_abi_error(&mut caller, "module does not export memory for input_read");
                };
                if memory.write(&mut caller, ptr as usize, &input).is_err() {
                    return set_abi_error(&mut caller, "input_read points outside guest memory");
                }
                input.len() as i32
            },
        )?;
        linker.func_wrap(
            "rbe",
            "output_write",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, length: i32| -> i32 {
                if ptr < 0 || length < 0 {
                    return set_abi_error(&mut caller, "output_write received a negative pointer or length");
                }
                let length = length as usize;
                if length > caller.data().max_output_bytes {
                    return set_abi_error(&mut caller, "module output exceeds the configured execution limit");
                }
                let Some(memory) = caller.get_export("memory").and_then(|export| export.into_memory()) else {
                    return set_abi_error(&mut caller, "module does not export memory for output_write");
                };
                let mut output = vec![0u8; length];
                if memory.read(&caller, ptr as usize, &mut output).is_err() {
                    return set_abi_error(&mut caller, "output_write points outside guest memory");
                }
                caller.data_mut().output = output;
                length as i32
            },
        )?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| anyhow::anyhow!("instantiate WASM artifact: {error}"))?;
        let run = instance
            .get_typed_func::<(), i32>(&mut store, "run")
            .map_err(|error| anyhow::anyhow!("WASM artifact must export run() -> i32: {error}"))?;
        let exit_code = run
            .call(&mut store, ())
            .map_err(|error| anyhow::anyhow!("execute WASM run(): {error}"))?;
        if let Some(error) = store.data().abi_error.clone() {
            anyhow::bail!("WASM ABI violation: {error}");
        }
        let remaining = store.get_fuel().unwrap_or(0);
        let output = store.data().output.clone();

        Ok(ExecutionResult {
            exit_code,
            fuel_consumed: limits.fuel.saturating_sub(remaining),
            output,
        })
    }
}

fn set_abi_error(caller: &mut Caller<'_, ExecutionState>, message: &str) -> i32 {
    if caller.data().abi_error.is_none() {
        caller.data_mut().abi_error = Some(message.to_string());
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_module() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "rbe" "input_len" (func $input_len (result i32)))
                (import "rbe" "input_read" (func $input_read (param i32 i32) (result i32)))
                (import "rbe" "output_write" (func $output_write (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "run") (result i32)
                    (local $len i32)
                    call $input_len
                    local.set $len
                    i32.const 0
                    local.get $len
                    call $input_read
                    drop
                    i32.const 0
                    local.get $len
                    call $output_write
                    drop
                    i32.const 0))"#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_invalid_wasm() {
        let executor = WasmExecutor::new().unwrap();
        assert!(executor
            .execute(b"not wasm", ExecutionLimits::default())
            .is_err());
    }

    #[test]
    fn host_abi_round_trips_binary_invocation_data() {
        let executor = WasmExecutor::new().unwrap();
        let result = executor
            .execute_with_input(&echo_module(), b"hello\0rbe", ExecutionLimits::default())
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, b"hello\0rbe");
    }

    #[test]
    fn host_abi_enforces_output_limit_even_if_guest_ignores_return_code() {
        let executor = WasmExecutor::new().unwrap();
        let limits = ExecutionLimits {
            max_output_bytes: 4,
            ..ExecutionLimits::default()
        };
        let error = executor
            .execute_with_input(&echo_module(), b"hello", limits)
            .unwrap_err();
        assert!(error.to_string().contains("output exceeds"));
    }
}
''', encoding="utf-8")

manifest = Path("container-runtime/crates/execution-engine/Cargo.toml")
text = manifest.read_text(encoding="utf-8")
if "[dev-dependencies]" not in text:
    text = text.rstrip() + "\n\n[dev-dependencies]\nwat = \"1\"\n"
elif "wat = " not in text:
    text += "wat = \"1\"\n"
manifest.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Worker runner now returns bounded binary output all the way to Runtime.
# ---------------------------------------------------------------------------
replace_once(
    "container-runtime/crates/container-runtime-core/src/worker.rs",
    "pub type Runner = Arc<dyn Fn(&ExecutionTask) -> Result<(), String> + Send + Sync + 'static>;\n"
    "pub(crate) type Completion =\n"
    "    Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static>;",
    "pub type Runner = Arc<dyn Fn(&ExecutionTask) -> Result<Vec<u8>, String> + Send + Sync + 'static>;\n"
    "pub(crate) type Completion =\n"
    "    Arc<dyn Fn(&ExecutionTask, u64, Result<Vec<u8>, String>) + Send + Sync + 'static>;",
)
replace_once(
    "container-runtime/crates/container-runtime-core/src/swamp.rs",
    "                Ok(())\n            })",
    "                Ok(Vec::new())\n            })",
)

runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
text = runtime.read_text(encoding="utf-8")
text = text.replace("use std::io::Write;", "use std::io::{BufReader, Write};", 1)
text = text.replace(
    "use environments::EnvironmentId;\nuse execution_engine::WasmExecutor;",
    "use environments::EnvironmentId;\nuse execution_engine::WasmExecutor;\n"
    "use ipc_protocol::{read_worker_result, write_worker_input, WorkerResultFrame};",
    1,
)
old = """            Arc::new(move |task| {
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled before start".into());
                }
                if cache.contains_artifact(&task.artifact_hash) {
                    run_isolated_worker(task, &cancelled)?;
                } else if task.work_ms > 0 {
                    run_simulated_work(task, &cancelled)?;
                }
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled".into());
                }
                Ok(())
            })
"""
new = """            Arc::new(move |task| {
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled before start".into());
                }
                let output = if cache.contains_artifact(&task.artifact_hash) {
                    run_isolated_worker(task, &cancelled)?
                } else if task.work_ms > 0 {
                    run_simulated_work(task, &cancelled)?
                } else {
                    Vec::new()
                };
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled".into());
                }
                Ok(output)
            })
"""
if old not in text:
    raise SystemExit("runtime runner output anchor missing")
text = text.replace(old, new, 1)
text = text.replace(
    "                    ExecutionOutcome {\n                        output: Vec::new(),\n                        error: result.as_ref().err().cloned(),",
    "                    ExecutionOutcome {\n                        output: result.as_ref().ok().cloned().unwrap_or_default(),\n                        error: result.as_ref().err().cloned(),",
    1,
)
text = text.replace(
    ") -> Result<(), String> {\n    let started = Instant::now();\n    let work = Duration::from_millis(task.work_ms);",
    ") -> Result<Vec<u8>, String> {\n    let started = Instant::now();\n    let work = Duration::from_millis(task.work_ms);",
    1,
)
text = text.replace("            return Ok(());", "            return Ok(Vec::new());", 1)
old_start = """fn run_isolated_worker(
    task: &ExecutionTask,
    cancelled: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), String> {
"""
start = text.find(old_start)
if start < 0:
    raise SystemExit("run_isolated_worker start anchor missing")
new_worker = r'''fn run_isolated_worker(
    task: &ExecutionTask,
    cancelled: &Arc<Mutex<HashSet<String>>>,
) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let artifact = &task.artifact_hash;
    let fuel = task.limits.cpu_millis.saturating_mul(10_000).max(1_000_000);
    let memory = task.limits.memory_bytes.max(64 * 1024);
    let mut command = std::process::Command::new(exe);
    command.args([
        "--worker",
        "--artifact",
        artifact,
        "--fuel",
        &fuel.to_string(),
        "--memory",
        &memory.to_string(),
    ]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;

    let write_result = (|| -> Result<(), String> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "isolated worker stdin pipe is unavailable".to_string())?;
        write_worker_input(&mut stdin, &task.payload).map_err(|e| e.to_string())?;
        drop(stdin);
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("failed to send invocation data to isolated worker: {error}"));
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "isolated worker stdout pipe is unavailable".to_string())?;
    let result_reader = thread::Builder::new()
        .name(format!("rbe-worker-result-{}", task.id))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            read_worker_result(&mut reader)
        })
        .map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to start isolated worker result reader: {e}")
        })?;

    let started = Instant::now();
    let timeout = Duration::from_millis(task.limits.wall_time_ms.max(1));

    loop {
        if is_cancelled(cancelled, task) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = result_reader.join();
            return Err("execution cancelled".into());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = result_reader.join();
            return Err(format!(
                "execution timed out after {} ms",
                task.limits.wall_time_ms
            ));
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let frame = result_reader
                    .join()
                    .map_err(|_| "isolated worker result reader panicked".to_string())?
                    .map_err(|e| format!("isolated worker returned an invalid result frame: {e}"))?;
                if !status.success() {
                    return Err(format!("isolated worker exited with status {status}"));
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
'''
# run_isolated_worker is the final function in this file today; replace its full tail.
text = text[:start] + new_worker
runtime.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Worker mode consumes its invocation frame after sandbox setup and emits exactly
# one structured result frame on stdout. All execution failures remain data on
# this private pipe rather than unbounded process output.
# ---------------------------------------------------------------------------
container = Path("container-runtime/crates/container-bin/src/main.rs")
text = container.read_text(encoding="utf-8")
text = text.replace(
    "use ipc_protocol::{\n    decode_request, read_frame, write_frame, Request, Response, MAX_ARTIFACT_BYTES,\n"
    "    MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,\n};",
    "use ipc_protocol::{\n"
    "    decode_request, read_frame, read_worker_input, write_frame, write_worker_result, Request,\n"
    "    Response, WorkerResultFrame, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS,\n"
    "    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,\n"
    "};",
    1,
)
old_start = "fn run_worker(args: &[String]) -> anyhow::Result<()> {\n"
start = text.find(old_start)
if start < 0:
    raise SystemExit("container run_worker start anchor missing")
end_marker = "\nfn run_control_server(\n"
end = text.find(end_marker, start)
if end < 0:
    raise SystemExit("container run_worker end anchor missing")
new_run_worker = r'''fn run_worker(args: &[String]) -> anyhow::Result<()> {
    let frame = match run_worker_inner(args) {
        Ok(output) => WorkerResultFrame::Success(output),
        Err(error) => WorkerResultFrame::Error(bounded_worker_error(&error.to_string())),
    };
    let mut stdout = std::io::stdout().lock();
    write_worker_result(&mut stdout, &frame)
        .map_err(|error| anyhow::anyhow!("worker: failed to write result frame: {error}"))?;
    Ok(())
}

fn run_worker_inner(args: &[String]) -> anyhow::Result<Vec<u8>> {
    set_no_new_privileges()
        .map_err(|e| anyhow::anyhow!("worker: failed to set no_new_privs: {e}"))?;
    install_restricted_seccomp()
        .map_err(|e| anyhow::anyhow!("worker: failed to install seccomp: {e}"))?;

    let mut stdin = std::io::stdin().lock();
    let input = read_worker_input(&mut stdin)
        .map_err(|e| anyhow::anyhow!("worker: invalid invocation frame: {e}"))?;

    let artifact = value_after(args, "--artifact")
        .ok_or_else(|| anyhow::anyhow!("worker: --artifact is required"))?;
    if artifact.len() != 64
        || !artifact
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        anyhow::bail!("worker: invalid artifact hash");
    }
    let path = runtime_paths::binary_dir()
        .join("data")
        .join("container-runtime")
        .join("artifacts")
        .join(format!("{artifact}.wasm"));
    let wasm =
        fs::read(path).map_err(|e| anyhow::anyhow!("worker: failed to read artifact: {e}"))?;
    let fuel = value_after(args, "--fuel")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10_000_000);
    let max_memory_bytes = value_after(args, "--memory")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(64 * 1024 * 1024);
    let executor = WasmExecutor::new()?;
    let result = executor.execute_with_input(
        &wasm,
        &input,
        ExecutionLimits {
            fuel,
            max_memory_bytes,
            max_output_bytes: MAX_EXECUTION_OUTPUT_BYTES as u64,
        },
    )?;
    if result.exit_code != 0 {
        anyhow::bail!("worker: WASM exited with status {}", result.exit_code);
    }
    Ok(result.output)
}

fn bounded_worker_error(message: &str) -> String {
    const LIMIT: usize = 32 * 1024;
    if message.len() <= LIMIT {
        return message.to_string();
    }
    let mut end = LIMIT;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}
'''
text = text[:start] + new_run_worker + text[end:]
container.write_text(text, encoding="utf-8")

print("WASM worker invocation IO applied")
