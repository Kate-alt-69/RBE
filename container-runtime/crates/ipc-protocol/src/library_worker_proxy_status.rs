use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::{read_frame, write_frame, LIBRARY_WORKER_PROXY_PROTOCOL_VERSION};

pub const MAX_LIBRARY_WORKER_PROXY_CODE_BYTES: usize = 64;
pub const MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES: usize = 4 * 1024;

/// Startup status emitted by the trusted Container Library Worker Proxy before
/// raw Library Protocol traffic is allowed to begin.
///
/// Backend must consume exactly one of these frames after sending the proxy
/// bootstrap. Only `Ready` permits it to continue into `library.hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum LibraryWorkerProxyStatus {
    #[serde(rename = "library.proxy.ready")]
    Ready { protocol: u16, pid: u32 },
    #[serde(rename = "library.proxy.reject")]
    Reject {
        protocol: u16,
        code: String,
        message: String,
    },
}

impl LibraryWorkerProxyStatus {
    pub fn ready(pid: u32) -> Self {
        Self::Ready {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            pid,
        }
    }

    pub fn reject(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Reject {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn validate(&self) -> Result<(), LibraryWorkerProxyStatusError> {
        let protocol = match self {
            Self::Ready { protocol, .. } | Self::Reject { protocol, .. } => *protocol,
        };
        if protocol != LIBRARY_WORKER_PROXY_PROTOCOL_VERSION {
            return Err(LibraryWorkerProxyStatusError::UnsupportedProtocol(protocol));
        }

        match self {
            Self::Ready { pid, .. } => {
                if *pid == 0 {
                    return Err(LibraryWorkerProxyStatusError::InvalidPid);
                }
            }
            Self::Reject { code, message, .. } => {
                if code.is_empty()
                    || code.len() > MAX_LIBRARY_WORKER_PROXY_CODE_BYTES
                    || !code.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                {
                    return Err(LibraryWorkerProxyStatusError::InvalidCode);
                }
                if message.is_empty()
                    || message.len() > MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES
                    || message.chars().any(char::is_control)
                {
                    return Err(LibraryWorkerProxyStatusError::InvalidMessage);
                }
            }
        }
        Ok(())
    }
}

pub fn write_library_worker_proxy_status<W: Write>(
    writer: &mut W,
    status: &LibraryWorkerProxyStatus,
) -> io::Result<()> {
    status
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    write_frame(writer, status)
}

pub fn read_library_worker_proxy_status<R: Read>(
    reader: &mut R,
) -> io::Result<LibraryWorkerProxyStatus> {
    let body = read_frame(reader)?;
    let status: LibraryWorkerProxyStatus = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    status
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(status)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryWorkerProxyStatusError {
    UnsupportedProtocol(u16),
    InvalidPid,
    InvalidCode,
    InvalidMessage,
}

impl fmt::Display for LibraryWorkerProxyStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocol(protocol) => {
                write!(
                    formatter,
                    "unsupported Library Worker Proxy status protocol {protocol}"
                )
            }
            Self::InvalidPid => {
                formatter.write_str("Library Worker Proxy ready status requires a non-zero pid")
            }
            Self::InvalidCode => formatter
                .write_str("Library Worker Proxy reject code must be bounded ASCII A-Z/0-9/_"),
            Self::InvalidMessage => formatter
                .write_str("Library Worker Proxy reject message must be bounded printable text"),
        }
    }
}

impl Error for LibraryWorkerProxyStatusError {}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn ready_status_round_trips() {
        let expected = LibraryWorkerProxyStatus::ready(42);
        let mut bytes = Vec::new();
        write_library_worker_proxy_status(&mut bytes, &expected).unwrap();
        let decoded = read_library_worker_proxy_status(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn reject_status_round_trips() {
        let expected = LibraryWorkerProxyStatus::reject(
            "SANDBOX_START_FAILED",
            "Container could not start the isolated package worker",
        );
        let mut bytes = Vec::new();
        write_library_worker_proxy_status(&mut bytes, &expected).unwrap();
        let decoded = read_library_worker_proxy_status(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn invalid_status_is_rejected_before_any_bytes_are_written() {
        let invalid = LibraryWorkerProxyStatus::Reject {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            code: "bad-code".into(),
            message: "nope".into(),
        };
        let mut bytes = Vec::new();
        let error = write_library_worker_proxy_status(&mut bytes, &invalid).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bytes.is_empty());
    }

    #[test]
    fn receiver_rejects_unknown_fields() {
        let mut value = serde_json::to_value(LibraryWorkerProxyStatus::ready(7)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();
        let error = read_library_worker_proxy_status(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn status_validation_is_fail_closed() {
        assert_eq!(
            LibraryWorkerProxyStatus::Ready {
                protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
                pid: 0,
            }
            .validate(),
            Err(LibraryWorkerProxyStatusError::InvalidPid)
        );
        assert_eq!(
            LibraryWorkerProxyStatus::Reject {
                protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION + 1,
                code: "FAILED".into(),
                message: "failed".into(),
            }
            .validate(),
            Err(LibraryWorkerProxyStatusError::UnsupportedProtocol(
                LIBRARY_WORKER_PROXY_PROTOCOL_VERSION + 1
            ))
        );
    }
}
