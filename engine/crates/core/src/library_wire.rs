//! Typed wire envelopes for the RBE Library Protocol.
//!
//! The core Library Host owns payload validation while this module owns the
//! directional `type` envelope used on framed IPC. Workers cannot smuggle a
//! second message kind into a payload because the envelope tag is removed
//! before the strict payload parsers run.

use std::io::{Read, Write};

use serde_json::{json, Value};

use crate::library_host::{
    read_json_message, write_json_message, HostCall, LibraryHostError, PackageInvocation,
    PackageReply, WorkerHello, MAX_LIBRARY_PAYLOAD_BYTES,
};
use crate::library_session::{AcceptedLibrarySessionInfo, LibrarySessionBinding};

pub const LIBRARY_HELLO_TYPE: &str = "library.hello";
pub const LIBRARY_ACCEPT_TYPE: &str = "library.accept";
pub const LIBRARY_REJECT_TYPE: &str = "library.reject";
pub const LIBRARY_INVOKE_TYPE: &str = "library.invoke";
pub const LIBRARY_REPLY_TYPE: &str = "library.reply";
pub const HOST_CALL_TYPE: &str = "host.call";
pub const HOST_REPLY_TYPE: &str = "host.reply";

const MAX_WIRE_ERROR_BYTES: usize = 64 * 1024;
const HANDSHAKE_MALFORMED_CODE: &str = "MALFORMED_HELLO";
const HANDSHAKE_REQUIRED_CODE: &str = "HELLO_REQUIRED";
const HANDSHAKE_REJECTED_CODE: &str = "HELLO_REJECTED";
const HANDSHAKE_MALFORMED_MESSAGE: &str = "worker handshake could not be decoded";
const HANDSHAKE_REQUIRED_MESSAGE: &str = "first worker message must be library.hello";
const HANDSHAKE_REJECTED_MESSAGE: &str = "worker identity or ABI was rejected";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryWorkerMessage {
    Hello(WorkerHello),
    HostCall(HostCall),
    Reply(PackageReply),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryHostCallReply {
    pub call_id: u64,
    pub ok: bool,
    pub payload: Vec<u8>,
    pub error: Option<String>,
}

impl LibraryHostCallReply {
    pub fn success(call_id: u64, payload: Vec<u8>) -> Result<Self, LibraryHostError> {
        let reply = Self {
            call_id,
            ok: true,
            payload,
            error: None,
        };
        reply.validate()?;
        Ok(reply)
    }

    pub fn failure(call_id: u64, error: impl Into<String>) -> Result<Self, LibraryHostError> {
        let reply = Self {
            call_id,
            ok: false,
            payload: Vec::new(),
            error: Some(error.into()),
        };
        reply.validate()?;
        Ok(reply)
    }

    pub fn to_value(&self) -> Result<Value, LibraryHostError> {
        self.validate()?;
        Ok(json!({
            "type": HOST_REPLY_TYPE,
            "call_id": self.call_id,
            "ok": self.ok,
            "payload": self.payload,
            "error": self.error,
        }))
    }

    fn validate(&self) -> Result<(), LibraryHostError> {
        if self.payload.len() > MAX_LIBRARY_PAYLOAD_BYTES {
            return Err(LibraryHostError::PayloadTooLarge {
                limit: MAX_LIBRARY_PAYLOAD_BYTES,
                observed: self.payload.len(),
            });
        }
        match (self.ok, self.error.as_deref()) {
            (true, None) => Ok(()),
            (false, Some(error)) => validate_wire_text("host reply error", error),
            (true, Some(_)) => Err(LibraryHostError::InvalidMessage(
                "successful host reply must not include an error".into(),
            )),
            (false, None) => Err(LibraryHostError::InvalidMessage(
                "failed host reply must include an error".into(),
            )),
        }
    }
}

pub fn decode_worker_message(value: Value) -> Result<LibraryWorkerMessage, LibraryHostError> {
    let Value::Object(mut object) = value else {
        return Err(LibraryHostError::InvalidMessage(
            "Library Protocol envelope must be a JSON object".into(),
        ));
    };
    let kind = object
        .remove("type")
        .ok_or_else(|| LibraryHostError::InvalidMessage("missing field \"type\"".into()))?;
    let kind = kind.as_str().ok_or_else(|| {
        LibraryHostError::InvalidMessage("Library Protocol type must be a string".into())
    })?;
    let payload = Value::Object(object);

    match kind {
        LIBRARY_HELLO_TYPE => Ok(LibraryWorkerMessage::Hello(WorkerHello::from_value(
            &payload,
        )?)),
        HOST_CALL_TYPE => Ok(LibraryWorkerMessage::HostCall(HostCall::from_value(
            &payload,
        )?)),
        LIBRARY_REPLY_TYPE => Ok(LibraryWorkerMessage::Reply(PackageReply::from_value(
            &payload,
        )?)),
        other => Err(LibraryHostError::InvalidMessage(format!(
            "unsupported worker message type {other:?}"
        ))),
    }
}

pub fn read_worker_message<R: Read>(
    reader: &mut R,
) -> Result<LibraryWorkerMessage, LibraryHostError> {
    decode_worker_message(read_json_message(reader)?)
}

pub fn package_invocation_value(invocation: &PackageInvocation) -> Result<Value, LibraryHostError> {
    with_type(LIBRARY_INVOKE_TYPE, invocation.to_value()?)
}

pub fn reject_value(code: &str, message: &str) -> Result<Value, LibraryHostError> {
    validate_wire_text("reject code", code)?;
    validate_wire_text("reject message", message)?;
    Ok(json!({
        "type": LIBRARY_REJECT_TYPE,
        "code": code,
        "message": message,
    }))
}

pub fn write_accept<W: Write>(
    writer: &mut W,
    accepted: &AcceptedLibrarySessionInfo,
) -> Result<(), LibraryHostError> {
    write_json_message(writer, &accepted.to_value()).map_err(LibraryHostError::from)
}

pub fn write_reject<W: Write>(
    writer: &mut W,
    code: &str,
    message: &str,
) -> Result<(), LibraryHostError> {
    let value = reject_value(code, message)?;
    write_json_message(writer, &value).map_err(LibraryHostError::from)
}

pub fn write_package_invocation<W: Write>(
    writer: &mut W,
    invocation: &PackageInvocation,
) -> Result<(), LibraryHostError> {
    let value = package_invocation_value(invocation)?;
    write_json_message(writer, &value).map_err(LibraryHostError::from)
}

pub fn write_host_reply<W: Write>(
    writer: &mut W,
    reply: &LibraryHostCallReply,
) -> Result<(), LibraryHostError> {
    let value = reply.to_value()?;
    write_json_message(writer, &value).map_err(LibraryHostError::from)
}

impl LibrarySessionBinding {
    /// Consume the first frame from a private worker channel and complete the
    /// fail-closed Library Protocol handshake.
    ///
    /// The first worker frame must be `library.hello`. Identity/ABI details are
    /// checked by the same `LibrarySession` that later authorizes host calls.
    /// Rejected workers receive only a bounded generic reason so trusted lock,
    /// SDK, runtime, and artifact expectations are not reflected back to an
    /// untrusted process.
    pub fn accept_wire_handshake<R: Read, W: Write>(
        &mut self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<AcceptedLibrarySessionInfo, LibraryHostError> {
        let message = match read_worker_message(reader) {
            Ok(message) => message,
            Err(error) => {
                self.close();
                let _ = write_reject(
                    writer,
                    HANDSHAKE_MALFORMED_CODE,
                    HANDSHAKE_MALFORMED_MESSAGE,
                );
                return Err(error);
            }
        };

        let LibraryWorkerMessage::Hello(hello) = message else {
            self.close();
            let error = LibraryHostError::InvalidMessage(HANDSHAKE_REQUIRED_MESSAGE.into());
            let _ = write_reject(writer, HANDSHAKE_REQUIRED_CODE, HANDSHAKE_REQUIRED_MESSAGE);
            return Err(error);
        };

        let accepted = match self.accept_hello(&hello) {
            Ok(accepted) => accepted.clone(),
            Err(error) => {
                self.close();
                let _ = write_reject(writer, HANDSHAKE_REJECTED_CODE, HANDSHAKE_REJECTED_MESSAGE);
                return Err(error);
            }
        };

        if let Err(error) = write_accept(writer, &accepted) {
            self.close();
            return Err(error);
        }
        Ok(accepted)
    }
}

fn with_type(kind: &str, value: Value) -> Result<Value, LibraryHostError> {
    let Value::Object(mut object) = value else {
        return Err(LibraryHostError::InvalidMessage(
            "Library Protocol payload must serialize to a JSON object".into(),
        ));
    };
    if object
        .insert("type".into(), Value::String(kind.into()))
        .is_some()
    {
        return Err(LibraryHostError::InvalidMessage(
            "Library Protocol payload must not contain a type field".into(),
        ));
    }
    Ok(Value::Object(object))
}

fn validate_wire_text(field: &'static str, value: &str) -> Result<(), LibraryHostError> {
    if value.trim().is_empty()
        || value.len() > MAX_WIRE_ERROR_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(LibraryHostError::InvalidText(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::library_host::{
        CapabilityGrant, ExpectedWorkerIdentity, PackageIdentity, RuntimeIdentity, SdkIdentity,
        SessionState, LIBRARY_ABI_VERSION, LIBRARY_PROTOCOL_VERSION,
    };

    fn hello_value() -> Value {
        json!({
            "type": LIBRARY_HELLO_TYPE,
            "protocol": LIBRARY_PROTOCOL_VERSION,
            "package": {
                "name": "advancenet",
                "version": "1.0.0",
                "artifact_sha256": "a".repeat(64),
            },
            "sdk": {
                "language": "typescript",
                "name": "@rbe/sdk",
                "version": "0.1.0",
            },
            "runtime": {
                "kind": "bun",
                "version": "1.3.0",
            },
            "abi_min": LIBRARY_ABI_VERSION,
            "abi_max": LIBRARY_ABI_VERSION,
        })
    }

    fn binding() -> LibrarySessionBinding {
        LibrarySessionBinding::new(
            ExpectedWorkerIdentity {
                package: PackageIdentity {
                    name: "advancenet".into(),
                    version: "1.0.0".into(),
                    artifact_sha256: "a".repeat(64),
                },
                sdk: SdkIdentity {
                    language: "typescript".into(),
                    name: "@rbe/sdk".into(),
                    version: "0.1.0".into(),
                },
                runtime: RuntimeIdentity {
                    kind: "bun".into(),
                    version: "1.3.0".into(),
                },
                abi: LIBRARY_ABI_VERSION,
            },
            [
                CapabilityGrant::new("net:http", "net:http", ["request".to_string()], 1024, 4096)
                    .unwrap(),
            ],
            "session:test-wire",
        )
        .unwrap()
    }

    fn framed(value: &Value) -> Cursor<Vec<u8>> {
        let mut bytes = Vec::new();
        write_json_message(&mut bytes, value).unwrap();
        Cursor::new(bytes)
    }

    fn written_value(bytes: Vec<u8>) -> Value {
        read_json_message(&mut Cursor::new(bytes)).unwrap()
    }

    #[test]
    fn hello_envelope_is_removed_before_strict_payload_decode() {
        let message = decode_worker_message(hello_value()).unwrap();
        let LibraryWorkerMessage::Hello(hello) = message else {
            panic!("expected hello message");
        };
        assert_eq!(hello.package.name, "advancenet");
        assert_eq!(hello.runtime.kind, "bun");
    }

    #[test]
    fn unknown_worker_message_fails_closed() {
        let error = decode_worker_message(json!({"type": "library.magic"})).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported worker message type"));
    }

    #[test]
    fn invocation_envelope_preserves_operation_identity() {
        let value = package_invocation_value(&PackageInvocation {
            call_id: 7,
            export: "request".into(),
            operation: "get".into(),
            payload: b"[]".to_vec(),
        })
        .unwrap();
        assert_eq!(value["type"], LIBRARY_INVOKE_TYPE);
        assert_eq!(value["export"], "request");
        assert_eq!(value["operation"], "get");
    }

    #[test]
    fn host_reply_requires_consistent_success_state() {
        assert!(LibraryHostCallReply::success(1, b"ok".to_vec()).is_ok());
        assert!(LibraryHostCallReply::failure(2, "denied").is_ok());
        let invalid = LibraryHostCallReply {
            call_id: 3,
            ok: true,
            payload: Vec::new(),
            error: Some("nope".into()),
        };
        assert!(invalid.to_value().is_err());
    }

    #[test]
    fn framed_worker_message_round_trips_through_core_framing() {
        let mut bytes = Vec::new();
        write_json_message(&mut bytes, &hello_value()).unwrap();
        let mut cursor = Cursor::new(bytes);
        let message = read_worker_message(&mut cursor).unwrap();
        assert!(matches!(message, LibraryWorkerMessage::Hello(_)));
    }

    #[test]
    fn worker_message_identity_types_remain_core_types() {
        let message = decode_worker_message(hello_value()).unwrap();
        let LibraryWorkerMessage::Hello(hello) = message else {
            panic!("expected hello message");
        };
        assert_eq!(
            hello.package,
            PackageIdentity {
                name: "advancenet".into(),
                version: "1.0.0".into(),
                artifact_sha256: "a".repeat(64),
            }
        );
        assert_eq!(
            hello.sdk,
            SdkIdentity {
                language: "typescript".into(),
                name: "@rbe/sdk".into(),
                version: "0.1.0".into(),
            }
        );
        assert_eq!(
            hello.runtime,
            RuntimeIdentity {
                kind: "bun".into(),
                version: "1.3.0".into(),
            }
        );
    }

    #[test]
    fn wire_handshake_accepts_only_verified_hello_and_exposes_actual_grants() {
        let mut binding = binding();
        let mut input = framed(&hello_value());
        let mut output = Vec::new();

        let accepted = binding
            .accept_wire_handshake(&mut input, &mut output)
            .unwrap();
        assert_eq!(binding.state(), SessionState::Accepted);
        assert_eq!(
            accepted.granted_capabilities,
            std::collections::BTreeSet::from(["net:http".to_string()])
        );

        let response = written_value(output);
        assert_eq!(response["type"], LIBRARY_ACCEPT_TYPE);
        assert_eq!(response["capabilityIdentity"], "session:test-wire");
    }

    #[test]
    fn wire_handshake_rejects_non_hello_first_frame_and_closes_session() {
        let mut binding = binding();
        let mut input = framed(&json!({
            "type": HOST_CALL_TYPE,
            "call_id": 1,
            "capability": "net:http",
            "target": "net:http",
            "operation": "request",
            "payload": [],
        }));
        let mut output = Vec::new();

        assert!(binding
            .accept_wire_handshake(&mut input, &mut output)
            .is_err());
        assert_eq!(binding.state(), SessionState::Closed);
        let response = written_value(output);
        assert_eq!(response["type"], LIBRARY_REJECT_TYPE);
        assert_eq!(response["code"], HANDSHAKE_REQUIRED_CODE);
    }

    #[test]
    fn wire_handshake_rejects_identity_drift_without_reflecting_expected_identity() {
        let mut binding = binding();
        let mut wrong = hello_value();
        wrong["package"]["version"] = Value::String("9.9.9".into());
        let mut input = framed(&wrong);
        let mut output = Vec::new();

        assert!(binding
            .accept_wire_handshake(&mut input, &mut output)
            .is_err());
        assert_eq!(binding.state(), SessionState::Closed);
        let response = written_value(output);
        assert_eq!(response["type"], LIBRARY_REJECT_TYPE);
        assert_eq!(response["code"], HANDSHAKE_REJECTED_CODE);
        assert_eq!(response["message"], HANDSHAKE_REJECTED_MESSAGE);
        assert!(!response.to_string().contains("1.0.0"));
    }

    #[test]
    fn malformed_wire_handshake_is_rejected_and_closes_session() {
        let mut binding = binding();
        let mut input = Cursor::new(vec![0, 0, 0, 4, b'n', b'o', b'p', b'e']);
        let mut output = Vec::new();

        assert!(binding
            .accept_wire_handshake(&mut input, &mut output)
            .is_err());
        assert_eq!(binding.state(), SessionState::Closed);
        let response = written_value(output);
        assert_eq!(response["type"], LIBRARY_REJECT_TYPE);
        assert_eq!(response["code"], HANDSHAKE_MALFORMED_CODE);
    }
}
