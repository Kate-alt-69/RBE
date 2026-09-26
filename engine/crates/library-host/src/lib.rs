//! Trusted host-side contract for RBE external package workers.
//!
//! This crate owns protocol/session validation only. It does not spawn a worker,
//! choose a package runtime, or grant capabilities. Callers must derive the
//! expected identity and admitted capability set from already-verified package
//! state before creating a [`LibrarySession`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const LIBRARY_PROTOCOL_VERSION: u32 = 1;
pub const LIBRARY_ABI_VERSION: u32 = 1;
pub const MAX_LIBRARY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LIBRARY_NAME_BYTES: usize = 192;
pub const MAX_LIBRARY_OPERATION_BYTES: usize = 128;
pub const MAX_LIBRARY_TARGET_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHello {
    pub protocol: u32,
    pub package: PackageIdentity,
    pub sdk: SdkIdentity,
    pub runtime: RuntimeIdentity,
    pub abi_min: u32,
    pub abi_max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub artifact_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkIdentity {
    pub language: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub kind: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedWorkerIdentity {
    pub package: PackageIdentity,
    pub sdk: SdkIdentity,
    pub runtime: RuntimeIdentity,
    pub abi: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    pub capability: String,
    pub target: String,
    pub operations: BTreeSet<String>,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

impl CapabilityGrant {
    pub fn new(
        capability: impl Into<String>,
        target: impl Into<String>,
        operations: impl IntoIterator<Item = String>,
        max_request_bytes: usize,
        max_response_bytes: usize,
    ) -> Result<Self, LibraryHostError> {
        let grant = Self {
            capability: capability.into(),
            target: target.into(),
            operations: operations.into_iter().collect(),
            max_request_bytes,
            max_response_bytes,
        };
        grant.validate()?;
        Ok(grant)
    }

    fn validate(&self) -> Result<(), LibraryHostError> {
        validate_text("capability", &self.capability, MAX_LIBRARY_NAME_BYTES)?;
        validate_text("target", &self.target, MAX_LIBRARY_TARGET_BYTES)?;
        if self.operations.is_empty() {
            return Err(LibraryHostError::InvalidGrant(
                "capability grant must allow at least one operation".into(),
            ));
        }
        for operation in &self.operations {
            validate_text("operation", operation, MAX_LIBRARY_OPERATION_BYTES)?;
        }
        if self.max_request_bytes == 0
            || self.max_response_bytes == 0
            || self.max_request_bytes > MAX_LIBRARY_PAYLOAD_BYTES
            || self.max_response_bytes > MAX_LIBRARY_PAYLOAD_BYTES
        {
            return Err(LibraryHostError::InvalidGrant(
                "grant payload limits must be within the Library Protocol payload ceiling".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCall {
    pub call_id: u64,
    pub capability: String,
    pub target: String,
    pub operation: String,
    #[serde(default)]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageInvocation {
    pub call_id: u64,
    pub export: String,
    #[serde(default)]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageReply {
    pub call_id: u64,
    pub ok: bool,
    #[serde(default)]
    pub payload: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    AwaitHello,
    Accepted,
    Closed,
}

pub struct LibrarySession {
    expected: ExpectedWorkerIdentity,
    grants: BTreeMap<(String, String), CapabilityGrant>,
    state: SessionState,
}

impl LibrarySession {
    pub fn new(
        expected: ExpectedWorkerIdentity,
        grants: impl IntoIterator<Item = CapabilityGrant>,
    ) -> Result<Self, LibraryHostError> {
        validate_expected(&expected)?;
        let mut admitted = BTreeMap::new();
        for grant in grants {
            grant.validate()?;
            let key = (grant.capability.clone(), grant.target.clone());
            if admitted.insert(key.clone(), grant).is_some() {
                return Err(LibraryHostError::InvalidGrant(format!(
                    "duplicate capability/target grant {:?} -> {:?}",
                    key.0, key.1
                )));
            }
        }
        Ok(Self {
            expected,
            grants: admitted,
            state: SessionState::AwaitHello,
        })
    }

    pub const fn state(&self) -> SessionState {
        self.state
    }

    pub fn accept_hello(&mut self, hello: &WorkerHello) -> Result<(), LibraryHostError> {
        if self.state != SessionState::AwaitHello {
            return Err(LibraryHostError::InvalidState {
                expected: SessionState::AwaitHello,
                actual: self.state,
            });
        }
        validate_hello(hello)?;
        if hello.protocol != LIBRARY_PROTOCOL_VERSION {
            return Err(LibraryHostError::ProtocolMismatch {
                expected: LIBRARY_PROTOCOL_VERSION,
                actual: hello.protocol,
            });
        }
        if self.expected.abi < hello.abi_min || self.expected.abi > hello.abi_max {
            return Err(LibraryHostError::AbiMismatch {
                selected: self.expected.abi,
                worker_min: hello.abi_min,
                worker_max: hello.abi_max,
            });
        }
        if hello.package != self.expected.package
            || hello.sdk != self.expected.sdk
            || hello.runtime != self.expected.runtime
        {
            return Err(LibraryHostError::IdentityMismatch);
        }
        self.state = SessionState::Accepted;
        Ok(())
    }

    pub fn authorize_host_call(&self, call: &HostCall) -> Result<&CapabilityGrant, LibraryHostError> {
        if self.state != SessionState::Accepted {
            return Err(LibraryHostError::InvalidState {
                expected: SessionState::Accepted,
                actual: self.state,
            });
        }
        validate_text("capability", &call.capability, MAX_LIBRARY_NAME_BYTES)?;
        validate_text("target", &call.target, MAX_LIBRARY_TARGET_BYTES)?;
        validate_text("operation", &call.operation, MAX_LIBRARY_OPERATION_BYTES)?;
        if call.payload.len() > MAX_LIBRARY_PAYLOAD_BYTES {
            return Err(LibraryHostError::PayloadTooLarge {
                limit: MAX_LIBRARY_PAYLOAD_BYTES,
                observed: call.payload.len(),
            });
        }
        let key = (call.capability.clone(), call.target.clone());
        let grant = self
            .grants
            .get(&key)
            .ok_or_else(|| LibraryHostError::CapabilityDenied {
                capability: call.capability.clone(),
                target: call.target.clone(),
                operation: call.operation.clone(),
            })?;
        if !grant.operations.contains(&call.operation) || call.payload.len() > grant.max_request_bytes {
            return Err(LibraryHostError::CapabilityDenied {
                capability: call.capability.clone(),
                target: call.target.clone(),
                operation: call.operation.clone(),
            });
        }
        Ok(grant)
    }

    pub fn validate_reply(
        &self,
        reply: &PackageReply,
        maximum_response_bytes: usize,
    ) -> Result<(), LibraryHostError> {
        if self.state != SessionState::Accepted {
            return Err(LibraryHostError::InvalidState {
                expected: SessionState::Accepted,
                actual: self.state,
            });
        }
        let limit = maximum_response_bytes.min(MAX_LIBRARY_PAYLOAD_BYTES);
        if reply.payload.len() > limit {
            return Err(LibraryHostError::PayloadTooLarge {
                limit,
                observed: reply.payload.len(),
            });
        }
        if let Some(error) = &reply.error {
            validate_text("worker error", error, 64 * 1024)?;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        self.state = SessionState::Closed;
    }
}

pub fn write_message<W: Write>(writer: &mut W, message: &impl Serialize) -> io::Result<()> {
    ipc_protocol::write_frame(writer, message)
}

pub fn read_message<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, LibraryHostError> {
    let frame = ipc_protocol::read_frame(reader)?;
    serde_json::from_slice(&frame).map_err(LibraryHostError::Json)
}

fn validate_expected(expected: &ExpectedWorkerIdentity) -> Result<(), LibraryHostError> {
    validate_package(&expected.package)?;
    validate_sdk(&expected.sdk)?;
    validate_runtime(&expected.runtime)?;
    if expected.abi == 0 {
        return Err(LibraryHostError::InvalidIdentity(
            "selected ABI must be non-zero".into(),
        ));
    }
    Ok(())
}

fn validate_hello(hello: &WorkerHello) -> Result<(), LibraryHostError> {
    validate_package(&hello.package)?;
    validate_sdk(&hello.sdk)?;
    validate_runtime(&hello.runtime)?;
    if hello.abi_min == 0 || hello.abi_min > hello.abi_max {
        return Err(LibraryHostError::InvalidIdentity(
            "worker ABI range is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_package(package: &PackageIdentity) -> Result<(), LibraryHostError> {
    validate_text("package name", &package.name, MAX_LIBRARY_NAME_BYTES)?;
    validate_text("package version", &package.version, 128)?;
    validate_sha256(&package.artifact_sha256)
}

fn validate_sdk(sdk: &SdkIdentity) -> Result<(), LibraryHostError> {
    validate_text("SDK language", &sdk.language, 64)?;
    validate_text("SDK name", &sdk.name, MAX_LIBRARY_NAME_BYTES)?;
    validate_text("SDK version", &sdk.version, 128)
}

fn validate_runtime(runtime: &RuntimeIdentity) -> Result<(), LibraryHostError> {
    validate_text("runtime kind", &runtime.kind, 64)?;
    validate_text("runtime version", &runtime.version, 128)
}

fn validate_text(field: &'static str, value: &str, maximum: usize) -> Result<(), LibraryHostError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(LibraryHostError::InvalidText(field));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), LibraryHostError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LibraryHostError::InvalidIdentity(
            "package artifact SHA-256 must be 64 hexadecimal characters".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryHostError {
    #[error("invalid Library Protocol state: expected {expected:?}, currently {actual:?}")]
    InvalidState {
        expected: SessionState,
        actual: SessionState,
    },
    #[error("Library Protocol mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: u32, actual: u32 },
    #[error("Library ABI mismatch: selected {selected}, worker supports {worker_min}..={worker_max}")]
    AbiMismatch {
        selected: u32,
        worker_min: u32,
        worker_max: u32,
    },
    #[error("package worker identity does not match the verified package/SDK/runtime state")]
    IdentityMismatch,
    #[error("invalid worker identity: {0}")]
    InvalidIdentity(String),
    #[error("invalid capability grant: {0}")]
    InvalidGrant(String),
    #[error("{0} must be non-empty, bounded and free of control characters")]
    InvalidText(&'static str),
    #[error("package host capability denied: {capability} -> {target} / {operation}")]
    CapabilityDenied {
        capability: String,
        target: String,
        operation: String,
    },
    #[error("Library Protocol payload exceeds {limit} bytes (observed {observed})")]
    PayloadTooLarge { limit: usize, observed: usize },
    #[error("Library Protocol IPC failed: {0}")]
    Io(#[from] io::Error),
    #[error("Library Protocol frame is invalid JSON: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> ExpectedWorkerIdentity {
        ExpectedWorkerIdentity {
            package: PackageIdentity {
                name: "advancenet".into(),
                version: "2.0.0".into(),
                artifact_sha256: "a".repeat(64),
            },
            sdk: SdkIdentity {
                language: "typescript".into(),
                name: "@rbe/sdk".into(),
                version: "1.0.0".into(),
            },
            runtime: RuntimeIdentity {
                kind: "bun".into(),
                version: "1.3.0".into(),
            },
            abi: LIBRARY_ABI_VERSION,
        }
    }

    fn hello() -> WorkerHello {
        let expected = expected();
        WorkerHello {
            protocol: LIBRARY_PROTOCOL_VERSION,
            package: expected.package,
            sdk: expected.sdk,
            runtime: expected.runtime,
            abi_min: 1,
            abi_max: 1,
        }
    }

    fn session() -> LibrarySession {
        LibrarySession::new(
            expected(),
            [CapabilityGrant::new(
                "net:http",
                "net:http",
                ["request".to_string()],
                1024,
                4096,
            )
            .unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn worker_cannot_call_host_before_verified_handshake() {
        let session = session();
        let error = session
            .authorize_host_call(&HostCall {
                call_id: 1,
                capability: "net:http".into(),
                target: "net:http".into(),
                operation: "request".into(),
                payload: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(error, LibraryHostError::InvalidState { .. }));
    }

    #[test]
    fn exact_verified_handshake_activates_session() {
        let mut session = session();
        session.accept_hello(&hello()).unwrap();
        assert_eq!(session.state(), SessionState::Accepted);
    }

    #[test]
    fn artifact_identity_drift_rejects_worker() {
        let mut session = session();
        let mut hello = hello();
        hello.package.artifact_sha256 = "b".repeat(64);
        assert!(matches!(
            session.accept_hello(&hello()),
            Err(LibraryHostError::IdentityMismatch)
        ));
    }

    #[test]
    fn accepted_worker_is_still_limited_to_admitted_capability_operation() {
        let mut session = session();
        session.accept_hello(&hello()).unwrap();
        session
            .authorize_host_call(&HostCall {
                call_id: 7,
                capability: "net:http".into(),
                target: "net:http".into(),
                operation: "request".into(),
                payload: b"hello".to_vec(),
            })
            .unwrap();
        assert!(matches!(
            session.authorize_host_call(&HostCall {
                call_id: 8,
                capability: "net:http".into(),
                target: "net:http".into(),
                operation: "raw_socket".into(),
                payload: Vec::new(),
            }),
            Err(LibraryHostError::CapabilityDenied { .. })
        ));
    }

    #[test]
    fn library_frames_use_existing_bounded_ipc_framing() {
        let message = PackageInvocation {
            call_id: 9,
            export: "request".into(),
            payload: b"payload".to_vec(),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        let decoded: PackageInvocation = read_message(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, message);
    }
}
