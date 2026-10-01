//! Runtime request/response envelopes for host-backed REL capabilities.
//!
//! The evaluator serializes these requests; Backend/Container performs the
//! privileged operation and returns a bounded response. Keeping the envelope
//! typed avoids letting REL smuggle arbitrary executable paths or host calls.

use crate::{ArchivePlan, ScriptPlan, WorkspacePlan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelHostRequest {
    Workspace(WorkspacePlan),
    Script(ScriptPlan),
    Archive(ArchivePlan),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelHostOutput {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl RelHostOutput {
    pub const MAX_STREAM_BYTES: usize = 1024 * 1024;

    pub fn bounded(
        ok: bool,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        fn truncate(mut value: String) -> String {
            if value.len() > RelHostOutput::MAX_STREAM_BYTES {
                value.truncate(RelHostOutput::MAX_STREAM_BYTES);
            }
            value
        }
        Self {
            ok,
            stdout: truncate(stdout.into()),
            stderr: truncate(stderr.into()),
            exit_code,
        }
    }
}
