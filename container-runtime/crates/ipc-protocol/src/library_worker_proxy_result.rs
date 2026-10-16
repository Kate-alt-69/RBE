use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES: usize = 256 * 1024;
pub const MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES: usize = 256 * 1024;
pub const MAX_LIBRARY_WORKER_PROXY_ERROR_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LibraryWorkerProxyResult {
    Completed {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        timed_out: bool,
        output_limit_exceeded: bool,
        cgroup_enforced: bool,
        wall_time_ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

impl LibraryWorkerProxyResult {
    pub fn validate(&self) -> Result<(), LibraryWorkerProxyResultError> {
        match self {
            Self::Completed { stdout, stderr, .. } => {
                if stdout.len() > MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES {
                    return Err(LibraryWorkerProxyResultError::StdoutTooLarge(stdout.len()));
                }
                if stderr.len() > MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES {
                    return Err(LibraryWorkerProxyResultError::StderrTooLarge(stderr.len()));
                }
            }
            Self::Error { code, message } => {
                if code.is_empty()
                    || code.len() > 64
                    || !code.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-')
                    })
                {
                    return Err(LibraryWorkerProxyResultError::InvalidErrorCode);
                }
                if message.is_empty()
                    || message.len() > MAX_LIBRARY_WORKER_PROXY_ERROR_BYTES
                    || message.as_bytes().contains(&0)
                {
                    return Err(LibraryWorkerProxyResultError::InvalidErrorMessage);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryWorkerProxyResultError {
    StdoutTooLarge(usize),
    StderrTooLarge(usize),
    InvalidErrorCode,
    InvalidErrorMessage,
}

impl fmt::Display for LibraryWorkerProxyResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StdoutTooLarge(size) => write!(
                formatter,
                "Library Worker Proxy stdout exceeds {MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES} bytes: {size}"
            ),
            Self::StderrTooLarge(size) => write!(
                formatter,
                "Library Worker Proxy stderr exceeds {MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES} bytes: {size}"
            ),
            Self::InvalidErrorCode => {
                formatter.write_str("Library Worker Proxy result error code is invalid")
            }
            Self::InvalidErrorMessage => {
                formatter.write_str("Library Worker Proxy result error message is invalid")
            }
        }
    }
}

impl Error for LibraryWorkerProxyResultError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_completed_result_is_valid() {
        LibraryWorkerProxyResult::Completed {
            exit_code: 0,
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
            output_limit_exceeded: false,
            cgroup_enforced: true,
            wall_time_ms: 12,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn oversized_output_and_malformed_errors_fail_closed() {
        let oversized = LibraryWorkerProxyResult::Completed {
            exit_code: 1,
            stdout: vec![0; MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES + 1],
            stderr: Vec::new(),
            timed_out: false,
            output_limit_exceeded: true,
            cgroup_enforced: true,
            wall_time_ms: 1,
        };
        assert!(matches!(
            oversized.validate(),
            Err(LibraryWorkerProxyResultError::StdoutTooLarge(_))
        ));

        let malformed = LibraryWorkerProxyResult::Error {
            code: "NOPE!".into(),
            message: "bad".into(),
        };
        assert_eq!(
            malformed.validate(),
            Err(LibraryWorkerProxyResultError::InvalidErrorCode)
        );
    }
}
