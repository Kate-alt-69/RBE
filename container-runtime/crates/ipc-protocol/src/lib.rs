//! Authenticated control-plane IPC types for the standalone `container` binary.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 3;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_EXECUTION_INPUT_BYTES: usize = 2 * 1024 * 1024;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub request_id: String,
    pub auth_token: String,
    pub environment: String,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    /// Invocation data for an already-registered artifact. This field must
    /// never be interpreted as executable bytes.
    pub input: Vec<u8>,
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
    Execute(ExecuteRequest),
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
    Accepted {
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
    fn rejects_zero_length_frame() {
        let bytes = [0, 0, 0, 0];
        assert!(read_frame(&mut bytes.as_slice()).is_err());
    }
}
