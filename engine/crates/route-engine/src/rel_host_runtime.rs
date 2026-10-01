//! Runtime request/response envelopes for host-backed REL capabilities.
//!
//! The evaluator builds typed requests; Backend/Container performs the
//! privileged operation and returns a bounded REL value. Keeping the envelope
//! typed avoids letting REL smuggle arbitrary executable paths or host calls.

use std::future::Future;
use std::pin::Pin;

use crate::ast::Value;
use crate::module_eval::ModuleEvalError;
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

    pub fn into_rel_value(self) -> Value {
        use std::collections::HashMap;

        let mut fields = HashMap::new();
        fields.insert("ok".into(), Value::Bool(self.ok));
        fields.insert("stdout".into(), Value::String(self.stdout));
        fields.insert("stderr".into(), Value::String(self.stderr));
        fields.insert(
            "exitCode".into(),
            self.exit_code
                .map(|value| Value::Number(value as f64))
                .unwrap_or(Value::Null),
        );
        Value::Object(fields)
    }
}

pub type RelHostExecutionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, ModuleEvalError>> + Send + 'a>>;

/// Trusted executor for filesystem/process/archive operations described by REL.
///
/// Implementations belong in Backend/Container. Route Engine never launches a
/// process or resolves a symbolic path to the host filesystem by itself.
pub trait RelHostExecutor: Send + Sync {
    fn execute<'a>(&'a self, request: RelHostRequest) -> RelHostExecutionFuture<'a>;
}
