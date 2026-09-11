from pathlib import Path
import re


def rep(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    found = text.count(old)
    if found != count:
        raise SystemExit(f"{path}: expected {count} anchors, found {found}: {old[:180]!r}")
    p.write_text(text.replace(old, new, count), encoding="utf-8")


def regex_rep(path: str, pattern: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    text, replaced = re.subn(pattern, new, text, count=count, flags=re.S)
    if replaced != count:
        raise SystemExit(f"{path}: expected {count} regex replacements, got {replaced}: {pattern[:180]!r}")
    p.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------
# Worker pipe protocol: capability requests are a distinct framed message from
# the terminal worker result. Responses travel back over the still-open stdin.
# ---------------------------------------------------------------------------
ipc = "container-runtime/crates/ipc-protocol/src/lib.rs"
rep(
    ipc,
    '''const WORKER_PIPE_MAGIC: [u8; 4] = *b"RBW1";
const WORKER_STATUS_SUCCESS: u8 = 0;''',
    '''const WORKER_PIPE_MAGIC: [u8; 4] = *b"RBW1";
const WORKER_CAPABILITY_CALL_MAGIC: [u8; 4] = *b"RBCQ";
const WORKER_CAPABILITY_RESULT_MAGIC: [u8; 4] = *b"RBCR";
const WORKER_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 16 * 1024;
const WORKER_STATUS_SUCCESS: u8 = 0;''',
)
rep(
    ipc,
    '''#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerResultFrame {
    Success(Vec<u8>),
    Error(String),
}
''',
    '''#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerResultFrame {
    Success(Vec<u8>),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapabilityCall {
    pub call_id: u64,
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerCapabilityResult {
    Success { call_id: u64, payload: Vec<u8> },
    Error {
        call_id: u64,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutputFrame {
    CapabilityCall(WorkerCapabilityCall),
    Result(WorkerResultFrame),
}
''',
)

# Replace worker result reader with a reusable body reader + multiplexed output.
regex_rep(
    ipc,
    r'''pub fn read_worker_result<R: Read>\(reader: &mut R\) -> io::Result<WorkerResultFrame> \{.*?\n\}\n\nfn expect_worker_magic<R: Read>''',
    '''pub fn read_worker_result<R: Read>(reader: &mut R) -> io::Result<WorkerResultFrame> {
    expect_worker_magic(reader)?;
    read_worker_result_body(reader)
}

pub fn write_worker_capability_call<W: Write>(
    writer: &mut W,
    call: &WorkerCapabilityCall,
) -> io::Result<()> {
    if call.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker capability request exceeds maximum size",
        ));
    }
    writer.write_all(&WORKER_CAPABILITY_CALL_MAGIC)?;
    write_worker_json(writer, call)
}

pub fn write_worker_capability_result<W: Write>(
    writer: &mut W,
    result: &WorkerCapabilityResult,
) -> io::Result<()> {
    if matches!(
        result,
        WorkerCapabilityResult::Success { payload, .. }
            if payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker capability response exceeds maximum size",
        ));
    }
    writer.write_all(&WORKER_CAPABILITY_RESULT_MAGIC)?;
    write_worker_json(writer, result)
}

pub fn read_worker_capability_result<R: Read>(
    reader: &mut R,
) -> io::Result<WorkerCapabilityResult> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic != WORKER_CAPABILITY_RESULT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid worker capability response magic",
        ));
    }
    let result: WorkerCapabilityResult = read_worker_json(reader)?;
    if matches!(
        &result,
        WorkerCapabilityResult::Success { payload, .. }
            if payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker capability response exceeds maximum size",
        ));
    }
    Ok(result)
}

pub fn read_worker_output<R: Read>(reader: &mut R) -> io::Result<WorkerOutputFrame> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic == WORKER_PIPE_MAGIC {
        return read_worker_result_body(reader).map(WorkerOutputFrame::Result);
    }
    if magic == WORKER_CAPABILITY_CALL_MAGIC {
        let call: WorkerCapabilityCall = read_worker_json(reader)?;
        if call.payload.len() > MAX_CAPABILITY_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker capability request exceeds maximum size",
            ));
        }
        return Ok(WorkerOutputFrame::CapabilityCall(call));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid worker output protocol magic",
    ))
}

fn read_worker_result_body<R: Read>(reader: &mut R) -> io::Result<WorkerResultFrame> {
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
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("worker error is not UTF-8: {error}"),
                )
            })?;
            Ok(WorkerResultFrame::Error(message))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown worker result status {other}"),
        )),
    }
}

fn write_worker_json<W: Write>(writer: &mut W, value: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.len() > WORKER_CAPABILITY_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker capability frame exceeds maximum size",
        ));
    }
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn read_worker_json<T: for<'de> Deserialize<'de>, R: Read>(reader: &mut R) -> io::Result<T> {
    let length = read_worker_length(reader, WORKER_CAPABILITY_FRAME_BYTES, "worker capability frame")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn expect_worker_magic<R: Read>''',
)
rep(
    ipc,
    '''    #[test]
    fn worker_pipe_rejects_oversized_lengths_before_allocating() {''',
    '''    #[test]
    fn worker_capability_pipe_round_trips_requests_and_results() {
        let call = WorkerCapabilityCall {
            call_id: 7,
            kind: CapabilityKind::Service,
            target: "uac".into(),
            operation: "get_user".into(),
            payload: b"request".to_vec(),
        };
        let mut encoded = Vec::new();
        write_worker_capability_call(&mut encoded, &call).unwrap();
        assert_eq!(
            read_worker_output(&mut encoded.as_slice()).unwrap(),
            WorkerOutputFrame::CapabilityCall(call)
        );

        let result = WorkerCapabilityResult::Success {
            call_id: 7,
            payload: b"response".to_vec(),
        };
        let mut encoded = Vec::new();
        write_worker_capability_result(&mut encoded, &result).unwrap();
        assert_eq!(
            read_worker_capability_result(&mut encoded.as_slice()).unwrap(),
            result
        );
    }

    #[test]
    fn worker_pipe_rejects_oversized_lengths_before_allocating() {''',
)


# ---------------------------------------------------------------------------
# Execution engine: add a synchronous, bounded capability host ABI. The host is
# supplied by the worker process; Wasmtime itself still receives no sockets,
# paths, credentials, or host handles.
# ---------------------------------------------------------------------------
cargo = "container-runtime/crates/execution-engine/Cargo.toml"
rep(cargo, 'anyhow = "1"\n', 'anyhow = "1"\nipc-protocol = { path = "../ipc-protocol" }\n')
engine = "container-runtime/crates/execution-engine/src/lib.rs"
rep(
    engine,
    '''use anyhow::Result;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};''',
    '''use anyhow::Result;
use ipc_protocol::{CapabilityKind, MAX_CAPABILITY_PAYLOAD_BYTES};
use wasmtime::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};''',
)
rep(
    engine,
    '''#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub fuel_consumed: u64,
    pub output: Vec<u8>,
}

struct ExecutionState {''',
    '''#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub fuel_consumed: u64,
    pub output: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapabilityRequest {
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

pub type CapabilityHost = Box<dyn FnMut(HostCapabilityRequest) -> Result<Vec<u8>, String>>;

struct ExecutionState {''',
)
rep(
    engine,
    '''    max_output_bytes: usize,
    abi_error: Option<String>,
}''',
    '''    max_output_bytes: usize,
    abi_error: Option<String>,
    capability_host: Option<CapabilityHost>,
    capability_response: Vec<u8>,
}''',
)
rep(
    engine,
    '''    pub fn execute_with_input(
        &self,
        wasm: &[u8],
        input: &[u8],
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm)''',
    '''    pub fn execute_with_input(
        &self,
        wasm: &[u8],
        input: &[u8],
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult> {
        self.execute_with_input_and_capabilities(wasm, input, limits, None)
    }

    pub fn execute_with_input_and_capabilities(
        &self,
        wasm: &[u8],
        input: &[u8],
        limits: ExecutionLimits,
        capability_host: Option<CapabilityHost>,
    ) -> Result<ExecutionResult> {
        let module = Module::new(&self.engine, wasm)''',
)
rep(
    engine,
    '''                max_output_bytes,
                abi_error: None,
            },''',
    '''                max_output_bytes,
                abi_error: None,
                capability_host,
                capability_response: Vec::new(),
            },''',
)

# Add capability imports after output_write.
rep(
    engine,
    '''        linker.func_wrap(
            "rbe",
            "output_write",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, length: i32| -> i32 {
                if ptr < 0 || length < 0 {
                    return set_abi_error(
                        &mut caller,
                        "output_write received a negative pointer or length",
                    );
                }
                let length = length as usize;
                if length > caller.data().max_output_bytes {
                    return set_abi_error(
                        &mut caller,
                        "module output exceeds the configured execution limit",
                    );
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return set_abi_error(
                        &mut caller,
                        "module does not export memory for output_write",
                    );
                };
                let mut output = vec![0u8; length];
                if memory.read(&caller, ptr as usize, &mut output).is_err() {
                    return set_abi_error(&mut caller, "output_write points outside guest memory");
                }
                caller.data_mut().output = output;
                length as i32
            },
        )?;
''',
    '''        linker.func_wrap(
            "rbe",
            "output_write",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, length: i32| -> i32 {
                if ptr < 0 || length < 0 {
                    return set_abi_error(
                        &mut caller,
                        "output_write received a negative pointer or length",
                    );
                }
                let length = length as usize;
                if length > caller.data().max_output_bytes {
                    return set_abi_error(
                        &mut caller,
                        "module output exceeds the configured execution limit",
                    );
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return set_abi_error(
                        &mut caller,
                        "module does not export memory for output_write",
                    );
                };
                let mut output = vec![0u8; length];
                if memory.read(&caller, ptr as usize, &mut output).is_err() {
                    return set_abi_error(&mut caller, "output_write points outside guest memory");
                }
                caller.data_mut().output = output;
                length as i32
            },
        )?;
        linker.func_wrap(
            "rbe",
            "capability_call",
            |mut caller: Caller<'_, ExecutionState>,
             kind: i32,
             target_ptr: i32,
             target_len: i32,
             operation_ptr: i32,
             operation_len: i32,
             payload_ptr: i32,
             payload_len: i32|
             -> i32 {
                let Some(kind) = capability_kind_from_abi(kind) else {
                    return set_abi_error(&mut caller, "capability_call received an unknown kind");
                };
                let target = match read_guest_bytes(&mut caller, target_ptr, target_len, 512, "capability target") {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(value) => value,
                        Err(_) => return set_abi_error(&mut caller, "capability target is not UTF-8"),
                    },
                    Err(error) => return set_abi_error(&mut caller, &error),
                };
                let operation = match read_guest_bytes(
                    &mut caller,
                    operation_ptr,
                    operation_len,
                    256,
                    "capability operation",
                ) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(value) => value,
                        Err(_) => return set_abi_error(&mut caller, "capability operation is not UTF-8"),
                    },
                    Err(error) => return set_abi_error(&mut caller, &error),
                };
                let payload = match read_guest_bytes(
                    &mut caller,
                    payload_ptr,
                    payload_len,
                    MAX_CAPABILITY_PAYLOAD_BYTES,
                    "capability payload",
                ) {
                    Ok(value) => value,
                    Err(error) => return set_abi_error(&mut caller, &error),
                };
                caller.data_mut().capability_response.clear();
                let request = HostCapabilityRequest {
                    kind,
                    target,
                    operation,
                    payload,
                };
                let response = match caller.data_mut().capability_host.as_mut() {
                    Some(host) => host(request),
                    None => Err("capability host is unavailable".into()),
                };
                match response {
                    Ok(response) if response.len() <= MAX_CAPABILITY_PAYLOAD_BYTES => {
                        let length = i32::try_from(response.len()).unwrap_or(i32::MAX);
                        caller.data_mut().capability_response = response;
                        length
                    }
                    Ok(_) => set_abi_error(&mut caller, "capability response exceeds maximum size"),
                    Err(error) => set_abi_error(&mut caller, &format!("capability call failed: {error}")),
                }
            },
        )?;
        linker.func_wrap(
            "rbe",
            "capability_response_len",
            |caller: Caller<'_, ExecutionState>| -> i32 {
                i32::try_from(caller.data().capability_response.len()).unwrap_or(i32::MAX)
            },
        )?;
        linker.func_wrap(
            "rbe",
            "capability_response_read",
            |mut caller: Caller<'_, ExecutionState>, ptr: i32, capacity: i32| -> i32 {
                if ptr < 0 || capacity < 0 {
                    return set_abi_error(
                        &mut caller,
                        "capability_response_read received a negative pointer or capacity",
                    );
                }
                let response = caller.data().capability_response.clone();
                if capacity as usize < response.len() {
                    return set_abi_error(
                        &mut caller,
                        "capability_response_read capacity is too small",
                    );
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return set_abi_error(
                        &mut caller,
                        "module does not export memory for capability_response_read",
                    );
                };
                if memory.write(&mut caller, ptr as usize, &response).is_err() {
                    return set_abi_error(
                        &mut caller,
                        "capability_response_read points outside guest memory",
                    );
                }
                response.len() as i32
            },
        )?;
''',
)
rep(
    engine,
    '''fn set_abi_error(caller: &mut Caller<'_, ExecutionState>, message: &str) -> i32 {''',
    '''fn capability_kind_from_abi(value: i32) -> Option<CapabilityKind> {
    match value {
        0 => Some(CapabilityKind::Service),
        1 => Some(CapabilityKind::Network),
        2 => Some(CapabilityKind::Storage),
        3 => Some(CapabilityKind::Vault),
        4 => Some(CapabilityKind::HostFile),
        5 => Some(CapabilityKind::Video),
        6 => Some(CapabilityKind::Debug),
        _ => None,
    }
}

fn read_guest_bytes(
    caller: &mut Caller<'_, ExecutionState>,
    ptr: i32,
    length: i32,
    max: usize,
    label: &str,
) -> std::result::Result<Vec<u8>, String> {
    if ptr < 0 || length < 0 {
        return Err(format!("{label} received a negative pointer or length"));
    }
    let length = length as usize;
    if length > max {
        return Err(format!("{label} exceeds maximum size"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| format!("module does not export memory for {label}"))?;
    let mut bytes = vec![0u8; length];
    memory
        .read(&*caller, ptr as usize, &mut bytes)
        .map_err(|_| format!("{label} points outside guest memory"))?;
    Ok(bytes)
}

fn set_abi_error(caller: &mut Caller<'_, ExecutionState>, message: &str) -> i32 {''',
)
rep(
    engine,
    '''    #[test]
    fn host_abi_enforces_output_limit_even_if_guest_ignores_return_code() {''',
    '''    #[test]
    fn capability_host_abi_round_trips_through_trusted_callback() {
        let wasm = wat::parse_str(
            r#"(module
                (import "rbe" "capability_call" (func $call (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
                (import "rbe" "capability_response_len" (func $response_len (result i32)))
                (import "rbe" "capability_response_read" (func $response_read (param i32 i32) (result i32)))
                (import "rbe" "output_write" (func $output_write (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "uac")
                (data (i32.const 16) "get_user")
                (data (i32.const 32) "req")
                (func (export "run") (result i32)
                    (local $len i32)
                    i32.const 0
                    i32.const 0
                    i32.const 3
                    i32.const 16
                    i32.const 8
                    i32.const 32
                    i32.const 3
                    call $call
                    drop
                    call $response_len
                    local.set $len
                    i32.const 64
                    local.get $len
                    call $response_read
                    drop
                    i32.const 64
                    local.get $len
                    call $output_write
                    drop
                    i32.const 0))"#,
        )
        .unwrap();
        let executor = WasmExecutor::new().unwrap();
        let host: CapabilityHost = Box::new(|request| {
            assert_eq!(request.kind, CapabilityKind::Service);
            assert_eq!(request.target, "uac");
            assert_eq!(request.operation, "get_user");
            assert_eq!(request.payload, b"req");
            Ok(b"trusted-response".to_vec())
        });
        let result = executor
            .execute_with_input_and_capabilities(
                &wasm,
                &[],
                ExecutionLimits::default(),
                Some(host),
            )
            .unwrap();
        assert_eq!(result.output, b"trusted-response");
    }

    #[test]
    fn host_abi_enforces_output_limit_even_if_guest_ignores_return_code() {''',
)


# ---------------------------------------------------------------------------
# Worker executable: bridge the Wasmtime host callback to its parent over the
# dedicated worker capability frames. No network/file authority is added.
# ---------------------------------------------------------------------------
main = "container-runtime/crates/container-bin/src/main.rs"
rep(
    main,
    '''use execution_engine::{ExecutionLimits, WasmExecutor};
use ipc_protocol::{
    decode_request, read_frame, read_worker_input, write_frame, write_worker_result, Request,
    Response, WorkerResultFrame, CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES, MAX_AWAIT_RESULT_MS,
    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,
};''',
    '''use execution_engine::{CapabilityHost, ExecutionLimits, WasmExecutor};
use ipc_protocol::{
    decode_request, read_frame, read_worker_capability_result, read_worker_input, write_frame,
    write_worker_capability_call, write_worker_result, Request, Response, WorkerCapabilityCall,
    WorkerCapabilityResult, WorkerResultFrame, CAPABILITY_ABI_VERSION, MAX_ARTIFACT_BYTES,
    MAX_AWAIT_RESULT_MS, MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_OUTPUT_BYTES, PROTOCOL_VERSION,
};''',
)
rep(
    main,
    '''    let mut stdin = std::io::stdin().lock();
    let input = read_worker_input(&mut stdin)
        .map_err(|e| anyhow::anyhow!("worker: invalid invocation frame: {e}"))?;
''',
    '''    let input = {
        let mut stdin = std::io::stdin().lock();
        read_worker_input(&mut stdin)
            .map_err(|e| anyhow::anyhow!("worker: invalid invocation frame: {e}"))?
    };
''',
)
rep(
    main,
    '''    let executor = WasmExecutor::new()?;
    let result = executor.execute_with_input(
        &wasm,
        &input,
        ExecutionLimits {
            fuel,
            max_memory_bytes,
            max_output_bytes: MAX_EXECUTION_OUTPUT_BYTES as u64,
        },
    )?;''',
    '''    let executor = WasmExecutor::new()?;
    let mut capability_stdin = std::io::stdin();
    let mut capability_stdout = std::io::stdout();
    let mut next_capability_call = 1u64;
    let host: CapabilityHost = Box::new(move |request| {
        let call_id = next_capability_call;
        next_capability_call = next_capability_call.saturating_add(1);
        let call = WorkerCapabilityCall {
            call_id,
            kind: request.kind,
            target: request.target,
            operation: request.operation,
            payload: request.payload,
        };
        write_worker_capability_call(&mut capability_stdout, &call)
            .map_err(|error| format!("send capability request to Environment: {error}"))?;
        match read_worker_capability_result(&mut capability_stdin)
            .map_err(|error| format!("read capability response from Environment: {error}"))?
        {
            WorkerCapabilityResult::Success {
                call_id: returned,
                payload,
            } if returned == call_id => Ok(payload),
            WorkerCapabilityResult::Error {
                call_id: returned,
                code,
                message,
            } if returned == call_id => Err(format!("{code}: {message}")),
            _ => Err("capability response identity mismatch".into()),
        }
    });
    let result = executor.execute_with_input_and_capabilities(
        &wasm,
        &input,
        ExecutionLimits {
            fuel,
            max_memory_bytes,
            max_output_bytes: MAX_EXECUTION_OUTPUT_BYTES as u64,
        },
        Some(host),
    )?;''',
)


# ---------------------------------------------------------------------------
# Environment ↔ Controller: multiplex capability calls over the already-
# authenticated execution socket. Controller authorizes against the exact task
# provenance before invoking a trusted dispatcher.
# ---------------------------------------------------------------------------
envp = "container-runtime/crates/container-bin/src/environment_process.rs"
rep(
    envp,
    '''use container_runtime_core::{
    Canceller, EnvironmentId, EnvironmentStorageManager, ExecutionTask, Runner,
    DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
use ipc_protocol::{
    read_frame, read_worker_result, write_frame, write_worker_input, WorkerResultFrame,
    CAPABILITY_ABI_VERSION, MAX_EXECUTION_INPUT_BYTES,
};''',
    '''use container_runtime_core::{
    Canceller, CapabilityBroker, CapabilityCall, EnvironmentId, EnvironmentStorageManager,
    ExecutionTask, Runner, DEFAULT_ENVIRONMENT_STORAGE_BYTES,
};
use ipc_protocol::{
    read_frame, read_worker_output, write_frame, write_worker_capability_result,
    write_worker_input, CapabilityKind, WorkerCapabilityCall, WorkerCapabilityResult,
    WorkerOutputFrame, WorkerResultFrame, CAPABILITY_ABI_VERSION, MAX_CAPABILITY_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES,
};''',
)
rep(envp, "const CHILD_PROTOCOL_VERSION: u16 = 2;", "const CHILD_PROTOCOL_VERSION: u16 = 3;")
rep(
    envp,
    '''    Cancel {
        request_id: String,
        session: String,
        environment: String,
        generation: u64,
        execution_id: String,
    },
}''',
    '''    Cancel {
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
}''',
)
rep(
    envp,
    '''    CancelAccepted {
        request_id: String,
        execution_id: String,
        active: bool,
    },
    Error {''',
    '''    CancelAccepted {
        request_id: String,
        execution_id: String,
        active: bool,
    },
    CapabilityCall {
        request_id: String,
        call: WorkerCapabilityCall,
    },
    Error {''',
)
rep(
    envp,
    '''struct ManagedEnvironment {
    child: Child,''',
    '''#[derive(Debug, Clone)]
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
    Arc::new(|_| {
        Err(CapabilityDispatchError {
            code: "CAPABILITY_DISPATCH_UNAVAILABLE".into(),
            message: "no trusted host capability dispatcher is configured".into(),
        })
    })
}

struct ManagedEnvironment {
    child: Child,''',
)
rep(
    envp,
    '''    cancelled: Mutex<HashMap<String, Instant>>,
}''',
    '''    cancelled: Mutex<HashMap<String, Instant>>,
    capability_broker: Arc<CapabilityBroker>,
    capability_dispatcher: CapabilityDispatcher,
}''',
    count=1,
)
rep(
    envp,
    '''    pub fn start(
        general_environments: usize,
        debug: bool,
        controller_token: Option<&str>,
    ) -> Result<Arc<Self>> {''',
    '''    pub fn start(
        general_environments: usize,
        debug: bool,
        controller_token: Option<&str>,
        capability_broker: Arc<CapabilityBroker>,
        capability_dispatcher: CapabilityDispatcher,
    ) -> Result<Arc<Self>> {''',
)
rep(
    envp,
    '''            executions: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashMap::new()),
        });''',
    '''            executions: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashMap::new()),
            capability_broker,
            capability_dispatcher,
        });''',
)

# Controller-side response loop handles intermediate capability calls.
rep(
    envp,
    '''        let result = (|| -> Result<Vec<u8>, String> {
            write_frame(&mut stream, &request)
                .map_err(|error| format!("send Environment execution: {error}"))?;
            read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
                .map_err(|error| format!("read Environment execution result: {error}"))
                .and_then(|response| match response {
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
                })
        })();''',
    '''        let result = (|| -> Result<Vec<u8>, String> {
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
        })();''',
)

# Add dispatcher method before spawn_one.
rep(
    envp,
    '''    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {''',
    '''    fn dispatch_capability(
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

    fn spawn_one(&self, id: EnvironmentId, generation: u64) -> Result<ManagedEnvironment> {''',
)

# Environment child Execute must pass the controller stream into worker relay.
rep(
    envp,
    '''                match execute_isolated_worker(worker, state) {''',
    '''                match execute_isolated_worker(worker, state, &mut stream, &bootstrap.session) {''',
)
# CapabilityResult is only legal as an in-flight response, never as first request.
rep(
    envp,
    '''        ChildRequest::Bootstrap(_) => child_error(
            None,
            "BOOTSTRAP_REPLAY",
            "Environment bootstrap is only accepted on inherited stdin",
        ),''',
    '''        ChildRequest::CapabilityResult { .. } => child_error(
            None,
            "CAPABILITY_RESULT_UNEXPECTED",
            "capability results are only accepted during an active execution",
        ),
        ChildRequest::Bootstrap(_) => child_error(
            None,
            "BOOTSTRAP_REPLAY",
            "Environment bootstrap is only accepted on inherited stdin",
        ),''',
)
rep(
    envp,
    '''fn execute_isolated_worker(
    worker: WorkerExecution<'_>,
    state: &EnvironmentChildState,
) -> Result<Vec<u8>, String> {''',
    '''fn execute_isolated_worker(
    worker: WorkerExecution<'_>,
    state: &EnvironmentChildState,
    controller: &mut TcpStream,
    session: &str,
) -> Result<Vec<u8>, String> {''',
)

# Keep worker stdin alive and replace terminal-only reader with multiplexed frames.
rep(
    envp,
    '''        if let Err(error) = write_worker_input(&mut worker_stdin, input) {
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
        }''',
    '''        if let Err(error) = write_worker_input(&mut worker_stdin, input) {
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
                    let response: ChildRequest = read_typed(
                        &mut BufReader::new(
                            controller
                                .try_clone()
                                .map_err(|error| format!("clone capability relay socket: {error}"))?,
                        ),
                    )
                    .map_err(|error| format!("read Controller capability response: {error}"))?;
                    let result = match response {
                        ChildRequest::CapabilityResult {
                            session: returned_session,
                            execution_id: returned_execution,
                            result,
                        } if returned_session == session
                            && returned_execution == execution_id
                            && capability_result_id(&result) == call.call_id => result,
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
        }''',
)
rep(
    envp,
    '''fn mark_cancelled(''',
    '''fn capability_result_id(result: &WorkerCapabilityResult) -> u64 {
    match result {
        WorkerCapabilityResult::Success { call_id, .. }
        | WorkerCapabilityResult::Error { call_id, .. } => *call_id,
    }
}

fn mark_cancelled(''',
)


# ---------------------------------------------------------------------------
# Container boot: the broker must exist before Environment processes so every
# relay shares the exact same authority table. Host dispatch remains explicit
# fail-closed until CAP-005 wires Backend's trusted ServiceManager bridge.
# ---------------------------------------------------------------------------
rep(
    main,
    '''    let token = env::var("RBE_CONTAINER_TOKEN").ok();
    let environment_processes = environment_process::EnvironmentProcessSupervisor::start(
        general_environments,
        debug,
        token.as_deref(),
    )?;
    let runtime = Runtime::new_with_runner(''',
    '''    let token = env::var("RBE_CONTAINER_TOKEN").ok();
    let capability_broker = Arc::new(CapabilityBroker::new(debug));
    let capability_dispatcher = environment_process::unavailable_capability_dispatcher();
    let environment_processes = environment_process::EnvironmentProcessSupervisor::start(
        general_environments,
        debug,
        token.as_deref(),
        Arc::clone(&capability_broker),
        capability_dispatcher,
    )?;
    let runtime = Runtime::new_with_runner(''',
)
rep(
    main,
    '''    );
    let capability_broker = Arc::new(CapabilityBroker::new(debug));
    let accepting = Arc::new(AtomicBool::new(true));''',
    '''    );
    let accepting = Arc::new(AtomicBool::new(true));''',
)

# Static checks before expensive CI.
checks = {
    ipc: ["WorkerCapabilityCall", "read_worker_output", "WORKER_CAPABILITY_CALL_MAGIC"],
    engine: ["execute_with_input_and_capabilities", "capability_response_read", "HostCapabilityRequest"],
    main: ["write_worker_capability_call", "unavailable_capability_dispatcher"],
    envp: ["CapabilityDispatchRequest", "dispatch_capability", "ChildResponse::CapabilityCall", "CHILD_PROTOCOL_VERSION: u16 = 3"],
}
for path, needles in checks.items():
    text = Path(path).read_text(encoding="utf-8")
    for needle in needles:
        if needle not in text:
            raise SystemExit(f"{path}: CAP-004 invariant missing: {needle}")
