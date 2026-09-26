//! Trusted host-side contract for RBE external package workers.
//!
//! This module owns protocol/session validation only. It does not spawn a
//! worker, choose a package runtime, or grant capabilities. Callers must derive
//! the expected identity and admitted capability set from already-verified
//! package state before creating a [`LibrarySession`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Write};

use serde_json::{json, Map, Value};

pub const LIBRARY_PROTOCOL_VERSION: u32 = 1;
pub const LIBRARY_ABI_VERSION: u32 = 1;
pub const MAX_LIBRARY_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LIBRARY_NAME_BYTES: usize = 192;
pub const MAX_LIBRARY_OPERATION_BYTES: usize = 128;
pub const MAX_LIBRARY_TARGET_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHello {
    pub protocol: u32,
    pub package: PackageIdentity,
    pub sdk: SdkIdentity,
    pub runtime: RuntimeIdentity,
    pub abi_min: u32,
    pub abi_max: u32,
}

impl WorkerHello {
    pub fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "worker hello")?;
        reject_unknown(
            object,
            &[
                "protocol", "package", "sdk", "runtime", "abi_min", "abi_max",
            ],
            "worker hello",
        )?;
        Ok(Self {
            protocol: u32_value(required(object, "protocol")?, "protocol")?,
            package: PackageIdentity::from_value(required(object, "package")?)?,
            sdk: SdkIdentity::from_value(required(object, "sdk")?)?,
            runtime: RuntimeIdentity::from_value(required(object, "runtime")?)?,
            abi_min: u32_value(required(object, "abi_min")?, "abi_min")?,
            abi_max: u32_value(required(object, "abi_max")?, "abi_max")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub artifact_sha256: String,
}

impl PackageIdentity {
    fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "package identity")?;
        reject_unknown(
            object,
            &["name", "version", "artifact_sha256"],
            "package identity",
        )?;
        Ok(Self {
            name: string_value(required(object, "name")?, "package name")?.to_string(),
            version: string_value(required(object, "version")?, "package version")?.to_string(),
            artifact_sha256: string_value(
                required(object, "artifact_sha256")?,
                "package artifact SHA-256",
            )?
            .to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkIdentity {
    pub language: String,
    pub name: String,
    pub version: String,
}

impl SdkIdentity {
    fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "SDK identity")?;
        reject_unknown(object, &["language", "name", "version"], "SDK identity")?;
        Ok(Self {
            language: string_value(required(object, "language")?, "SDK language")?.to_string(),
            name: string_value(required(object, "name")?, "SDK name")?.to_string(),
            version: string_value(required(object, "version")?, "SDK version")?.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub kind: String,
    pub version: String,
}

impl RuntimeIdentity {
    fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "runtime identity")?;
        reject_unknown(object, &["kind", "version"], "runtime identity")?;
        Ok(Self {
            kind: string_value(required(object, "kind")?, "runtime kind")?.to_string(),
            version: string_value(required(object, "version")?, "runtime version")?.to_string(),
        })
    }
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
                "grant payload limits must fit the Library Protocol ceiling".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCall {
    pub call_id: u64,
    pub capability: String,
    pub target: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl HostCall {
    pub fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "host call")?;
        reject_unknown(
            object,
            &["call_id", "capability", "target", "operation", "payload"],
            "host call",
        )?;
        Ok(Self {
            call_id: u64_value(required(object, "call_id")?, "call_id")?,
            capability: string_value(required(object, "capability")?, "capability")?.to_string(),
            target: string_value(required(object, "target")?, "target")?.to_string(),
            operation: string_value(required(object, "operation")?, "operation")?.to_string(),
            payload: bytes_value(object.get("payload"), "payload")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInvocation {
    pub call_id: u64,
    pub export: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl PackageInvocation {
    pub fn validate(&self) -> Result<(), LibraryHostError> {
        validate_text("package export", &self.export, MAX_LIBRARY_NAME_BYTES)?;
        validate_text(
            "package operation",
            &self.operation,
            MAX_LIBRARY_OPERATION_BYTES,
        )?;
        if self.payload.len() > MAX_LIBRARY_PAYLOAD_BYTES {
            return Err(LibraryHostError::PayloadTooLarge {
                limit: MAX_LIBRARY_PAYLOAD_BYTES,
                observed: self.payload.len(),
            });
        }
        Ok(())
    }

    pub fn to_value(&self) -> Result<Value, LibraryHostError> {
        self.validate()?;
        Ok(json!({
            "call_id": self.call_id,
            "export": self.export,
            "operation": self.operation,
            "payload": self.payload,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageReply {
    pub call_id: u64,
    pub ok: bool,
    pub payload: Vec<u8>,
    pub error: Option<String>,
}

impl PackageReply {
    pub fn from_value(value: &Value) -> Result<Self, LibraryHostError> {
        let object = object(value, "package reply")?;
        reject_unknown(
            object,
            &["call_id", "ok", "payload", "error"],
            "package reply",
        )?;
        let error = match object.get("error") {
            None | Some(Value::Null) => None,
            Some(value) => Some(string_value(value, "worker error")?.to_string()),
        };
        Ok(Self {
            call_id: u64_value(required(object, "call_id")?, "call_id")?,
            ok: bool_value(required(object, "ok")?, "ok")?,
            payload: bytes_value(object.get("payload"), "payload")?,
            error,
        })
    }
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

    pub fn authorize_host_call(
        &self,
        call: &HostCall,
    ) -> Result<&CapabilityGrant, LibraryHostError> {
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
        if !grant.operations.contains(&call.operation)
            || call.payload.len() > grant.max_request_bytes
        {
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

pub fn write_json_message<W: Write>(writer: &mut W, message: &Value) -> io::Result<()> {
    ipc_protocol::write_frame(writer, message)
}

pub fn read_json_message<R: Read>(reader: &mut R) -> Result<Value, LibraryHostError> {
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

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, LibraryHostError> {
    value
        .as_object()
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("{label} must be a JSON object")))
}

fn required<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, LibraryHostError> {
    object
        .get(field)
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("missing field {field:?}")))
}

fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
    label: &str,
) -> Result<(), LibraryHostError> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(LibraryHostError::InvalidMessage(format!(
            "{label} contains unknown field {field:?}"
        )));
    }
    Ok(())
}

fn string_value<'a>(value: &'a Value, field: &str) -> Result<&'a str, LibraryHostError> {
    value
        .as_str()
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("{field} must be a string")))
}

fn u64_value(value: &Value, field: &str) -> Result<u64, LibraryHostError> {
    value
        .as_u64()
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("{field} must be a u64")))
}

fn u32_value(value: &Value, field: &str) -> Result<u32, LibraryHostError> {
    u64_value(value, field)?
        .try_into()
        .map_err(|_| LibraryHostError::InvalidMessage(format!("{field} must be a u32")))
}

fn bool_value(value: &Value, field: &str) -> Result<bool, LibraryHostError> {
    value
        .as_bool()
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("{field} must be boolean")))
}

fn bytes_value(value: Option<&Value>, field: &str) -> Result<Vec<u8>, LibraryHostError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| LibraryHostError::InvalidMessage(format!("{field} must be a byte array")))?;
    if values.len() > MAX_LIBRARY_PAYLOAD_BYTES {
        return Err(LibraryHostError::PayloadTooLarge {
            limit: MAX_LIBRARY_PAYLOAD_BYTES,
            observed: values.len(),
        });
    }
    values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or_else(|| {
                    LibraryHostError::InvalidMessage(format!(
                        "{field} entries must be bytes (0..=255)"
                    ))
                })
        })
        .collect()
}

#[derive(Debug)]
pub enum LibraryHostError {
    InvalidState {
        expected: SessionState,
        actual: SessionState,
    },
    ProtocolMismatch {
        expected: u32,
        actual: u32,
    },
    AbiMismatch {
        selected: u32,
        worker_min: u32,
        worker_max: u32,
    },
    IdentityMismatch,
    InvalidIdentity(String),
    InvalidGrant(String),
    InvalidText(&'static str),
    InvalidMessage(String),
    CapabilityDenied {
        capability: String,
        target: String,
        operation: String,
    },
    PayloadTooLarge {
        limit: usize,
        observed: usize,
    },
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for LibraryHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState { expected, actual } => write!(
                formatter,
                "invalid Library Protocol state: expected {expected:?}, currently {actual:?}"
            ),
            Self::ProtocolMismatch { expected, actual } => write!(
                formatter,
                "Library Protocol mismatch: expected {expected}, got {actual}"
            ),
            Self::AbiMismatch {
                selected,
                worker_min,
                worker_max,
            } => write!(
                formatter,
                "Library ABI mismatch: selected {selected}, worker supports {worker_min}..={worker_max}"
            ),
            Self::IdentityMismatch => write!(
                formatter,
                "package worker identity does not match verified package/SDK/runtime state"
            ),
            Self::InvalidIdentity(message) => write!(formatter, "invalid worker identity: {message}"),
            Self::InvalidGrant(message) => write!(formatter, "invalid capability grant: {message}"),
            Self::InvalidText(field) => write!(
                formatter,
                "{field} must be non-empty, bounded and free of control characters"
            ),
            Self::InvalidMessage(message) => {
                write!(formatter, "invalid Library Protocol message: {message}")
            }
            Self::CapabilityDenied {
                capability,
                target,
                operation,
            } => write!(
                formatter,
                "package host capability denied: {capability} -> {target} / {operation}"
            ),
            Self::PayloadTooLarge { limit, observed } => write!(
                formatter,
                "Library Protocol payload exceeds {limit} bytes (observed {observed})"
            ),
            Self::Io(error) => write!(formatter, "Library Protocol IPC failed: {error}"),
            Self::Json(error) => write!(formatter, "Library Protocol frame is invalid JSON: {error}"),
        }
    }
}

impl std::error::Error for LibraryHostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LibraryHostError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
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
            [
                CapabilityGrant::new("net:http", "net:http", ["request".to_string()], 1024, 4096)
                    .unwrap(),
            ],
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
        let mut probe = hello();
        probe.package.artifact_sha256 = "b".repeat(64);
        assert!(matches!(
            session.accept_hello(&probe),
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
    fn package_invocation_preserves_export_and_operation_identity() {
        let message = PackageInvocation {
            call_id: 9,
            export: "request".into(),
            operation: "get".into(),
            payload: b"payload".to_vec(),
        };
        let value = message.to_value().unwrap();
        let mut bytes = Vec::new();
        write_json_message(&mut bytes, &value).unwrap();
        let decoded = read_json_message(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn worker_hello_rejects_unknown_fields() {
        let value = json!({
            "protocol": 1,
            "package": {
                "name": "advancenet",
                "version": "2.0.0",
                "artifact_sha256": "a".repeat(64),
            },
            "sdk": {"language":"typescript", "name":"@rbe/sdk", "version":"1.0.0"},
            "runtime": {"kind":"bun", "version":"1.3.0"},
            "abi_min": 1,
            "abi_max": 1,
            "surprise": true,
        });
        assert!(matches!(
            WorkerHello::from_value(&value),
            Err(LibraryHostError::InvalidMessage(_))
        ));
    }

    #[test]
    fn package_invocation_rejects_invalid_identity_and_oversized_payload() {
        let mut invocation = PackageInvocation {
            call_id: 1,
            export: String::new(),
            operation: "get".into(),
            payload: Vec::new(),
        };
        assert!(matches!(
            invocation.validate(),
            Err(LibraryHostError::InvalidText("package export"))
        ));

        invocation.export = "request".into();
        invocation.operation.clear();
        assert!(matches!(
            invocation.validate(),
            Err(LibraryHostError::InvalidText("package operation"))
        ));

        invocation.operation = "get".into();
        invocation.payload = vec![0; MAX_LIBRARY_PAYLOAD_BYTES + 1];
        assert!(matches!(
            invocation.validate(),
            Err(LibraryHostError::PayloadTooLarge { .. })
        ));
    }
}
