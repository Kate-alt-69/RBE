//! Authenticated control-plane IPC types for the standalone `container` binary.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 6;
pub const CAPABILITY_ABI_VERSION: u16 = 1;
pub const HOST_CAPABILITY_PROTOCOL_VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_EXECUTION_INPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EXECUTION_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CAPABILITY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_HOST_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 64 * 1024;
pub const MAX_AWAIT_RESULT_MS: u64 = 30_000;
pub const MAX_WORKER_ERROR_BYTES: usize = 64 * 1024;
const WORKER_PIPE_MAGIC: [u8; 4] = *b"RBW1";
const WORKER_CAPABILITY_CALL_MAGIC: [u8; 4] = *b"RBCQ";
const WORKER_CAPABILITY_RESULT_MAGIC: [u8; 4] = *b"RBCR";
const WORKER_CAPABILITY_FRAME_BYTES: usize = MAX_CAPABILITY_PAYLOAD_BYTES + 16 * 1024;
const WORKER_STATUS_SUCCESS: u8 = 0;
const WORKER_STATUS_ERROR: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Service,
    Network,
    Storage,
    Vault,
    HostFile,
    Video,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityGrant {
    pub kind: CapabilityKind,
    /// Logical target, never a host socket/path. Examples: `uac`,
    /// `api.example.com:443`, `service.uac`, or `video`.
    pub target: String,
    /// Exact allowed operation names. Wildcards are intentionally unsupported.
    pub operations: Vec<String>,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterCapabilityManifestRequest {
    pub request_id: String,
    pub auth_token: String,
    pub capability_abi: u16,
    pub runtime_image: String,
    pub source_id: String,
    pub environment: String,
    /// The caller deliberately does not provide an Environment generation.
    /// Container Controller binds the manifest to its current live generation.
    pub grants: Vec<CapabilityGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    /// Identity of the immutable Runtime Image that selected this execution.
    pub runtime_image: String,
    /// Exact REL source identity inside the Runtime Image.
    pub source_id: String,
    /// Capability ABI expected by this caller. Environment generation is
    /// intentionally absent: only Container Controller may stamp generation.
    pub capability_abi: u16,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    /// Invocation data for an already-registered artifact. This field must
    /// never be interpreted as executable bytes.
    pub input: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaitResultRequest {
    pub request_id: String,
    pub auth_token: String,
    pub execution_id: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    pub request_id: String,
    pub auth_token: String,
    pub execution_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectRequest {
    pub request_id: String,
    pub auth_token: String,
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartEnvironmentRequest {
    pub request_id: String,
    pub auth_token: String,
    pub environment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthRequest {
    pub request_id: String,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRefreshRequest {
    pub request_id: String,
    pub auth_token: String,
    pub drain_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeRequest {
    pub request_id: String,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello(Hello),
    RegisterArtifact(RegisterArtifactRequest),
    RegisterCapabilityManifest(RegisterCapabilityManifestRequest),
    Execute(ExecuteRequest),
    AwaitResult(AwaitResultRequest),
    Cancel(CancelRequest),
    Inspect(InspectRequest),
    RestartEnvironment(RestartEnvironmentRequest),
    Health(HealthRequest),
    PrepareRefresh(PrepareRefreshRequest),
    Resume(ResumeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkCost {
    pub cpu: u64,
    pub memory: u64,
    pub io: u64,
    pub network: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Success {
        call_id: u64,
        payload: Vec<u8>,
    },
    Error {
        call_id: u64,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapabilityRequest {
    pub version: u16,
    pub auth_token: String,
    pub execution_id: String,
    pub call_id: u64,
    pub kind: CapabilityKind,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostCapabilityResponse {
    Success {
        execution_id: String,
        call_id: u64,
        payload: Vec<u8>,
    },
    Error {
        execution_id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    HelloAccepted {
        version: u16,
    },
    ArtifactRegistered {
        request_id: String,
        artifact_hash: String,
        already_present: bool,
    },
    CapabilityManifestRegistered {
        request_id: String,
        runtime_image: String,
        source_id: String,
        environment: String,
        generation: u64,
        grants: usize,
    },
    Accepted {
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
        request_id: String,
    },
    Inspection {
        request_id: String,
        body: serde_json::Value,
    },
    Restarted {
        request_id: String,
        environment: String,
    },
    Health {
        request_id: String,
        body: serde_json::Value,
    },
    ReadyForRefresh {
        request_id: String,
    },
    Resumed {
        request_id: String,
    },
    Error {
        request_id: Option<String>,
        code: String,
        message: String,
    },
}

pub fn write_frame<W: Write>(writer: &mut W, value: &impl Serialize) -> io::Result<()> {
    let body =
        serde_json::to_vec(value).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC frame exceeds maximum size",
        ));
    }
    let length = body.len() as u32;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let length = u32::from_be_bytes(len) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

pub fn write_worker_input<W: Write>(writer: &mut W, input: &[u8]) -> io::Result<()> {
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

pub fn write_worker_result<W: Write>(writer: &mut W, result: &WorkerResultFrame) -> io::Result<()> {
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
    let length = read_worker_length(
        reader,
        WORKER_CAPABILITY_FRAME_BYTES,
        "worker capability frame",
    )?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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

pub fn decode_request(bytes: &[u8]) -> io::Result<Request> {
    serde_json::from_slice(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn decode_response(bytes: &[u8]) -> io::Result<Response> {
    serde_json::from_slice(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let request = Request::Health(HealthRequest {
            request_id: "req-1".into(),
            auth_token: "secret".into(),
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        assert!(matches!(decoded, Request::Health(_)));
    }

    #[test]
    fn refresh_round_trip() {
        let request = Request::PrepareRefresh(PrepareRefreshRequest {
            request_id: "refresh-1".into(),
            auth_token: "secret".into(),
            drain_timeout_ms: 30_000,
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        assert!(matches!(decoded, Request::PrepareRefresh(_)));
    }

    #[test]
    fn artifact_registration_round_trip_is_distinct_from_execution_input() {
        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: "artifact-1".into(),
            auth_token: "secret".into(),
            artifact_hash: "ab".repeat(32),
            wasm: vec![0, 97, 115, 109],
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        assert!(matches!(decoded, Request::RegisterArtifact(_)));

        let execute = Request::Execute(ExecuteRequest {
            request_id: "exec-1".into(),
            auth_token: "secret".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            environment: "general-1".into(),
            artifact_hash: "ab".repeat(32),
            declared_cost: WorkCost {
                cpu: 1,
                memory: 1,
                io: 0,
                network: 0,
            },
            input: b"request-body".to_vec(),
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &execute).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::Execute(decoded) = decoded else {
            panic!("expected execute request");
        };
        assert_eq!(decoded.input, b"request-body");
    }

    #[test]
    fn capability_manifest_round_trip_preserves_exact_grants() {
        let request = Request::RegisterCapabilityManifest(RegisterCapabilityManifestRequest {
            request_id: "cap-1".into(),
            auth_token: "secret".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            environment: "general-1".into(),
            grants: vec![CapabilityGrant {
                kind: CapabilityKind::Service,
                target: "uac".into(),
                operations: vec!["get_user".into()],
                max_request_bytes: 4096,
                max_response_bytes: 65536,
            }],
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::RegisterCapabilityManifest(decoded) = decoded else {
            panic!("expected capability-manifest request");
        };
        assert_eq!(decoded.capability_abi, CAPABILITY_ABI_VERSION);
        assert_eq!(decoded.grants[0].target, "uac");
    }

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
    fn worker_pipe_rejects_oversized_lengths_before_allocating() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&WORKER_PIPE_MAGIC);
        encoded.extend_from_slice(&((MAX_EXECUTION_INPUT_BYTES as u32) + 1).to_be_bytes());
        assert!(read_worker_input(&mut encoded.as_slice()).is_err());
    }

    #[test]
    fn rejects_zero_length_frame() {
        let bytes = [0, 0, 0, 0];
        assert!(read_frame(&mut bytes.as_slice()).is_err());
    }
}
