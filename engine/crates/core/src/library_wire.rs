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
use crate::library_session::AcceptedLibrarySessionInfo;

pub const LIBRARY_HELLO_TYPE: &str = "library.hello";
pub const LIBRARY_ACCEPT_TYPE: &str = "library.accept";
pub const LIBRARY_REJECT_TYPE: &str = "library.reject";
pub const LIBRARY_INVOKE_TYPE: &str = "library.invoke";
pub const LIBRARY_REPLY_TYPE: &str = "library.reply";
pub const HOST_CALL_TYPE: &str = "host.call";
pub const HOST_REPLY_TYPE: &str = "host.reply";

const MAX_WIRE_ERROR_BYTES: usize = 64 * 1024;

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
        PackageIdentity, RuntimeIdentity, SdkIdentity, LIBRARY_ABI_VERSION,
        LIBRARY_PROTOCOL_VERSION,
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
}
