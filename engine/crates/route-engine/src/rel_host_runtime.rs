//! Runtime request/response envelopes for host-backed REL capabilities.
//!
//! The evaluator builds typed requests; Backend/Container performs the
//! privileged operation and returns a bounded REL value. Keeping the envelope
//! typed avoids letting REL smuggle arbitrary executable paths or host calls.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use crate::ast::Value;
use crate::module_eval::ModuleEvalError;
use crate::{ArchivePlan, ScriptPlan, WorkspacePlan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelHostRequest {
    Workspace(WorkspacePlan),
    Script(ScriptPlan),
    Archive(ArchivePlan),
    /// Execute one already-validated host operation with an execution-scoped
    /// `??/` root. The host allocates and cleans the workspace; Route Engine
    /// never resolves the OS temporary directory itself.
    Temp(Box<RelHostRequest>),
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

/// RBE serves one frozen Runtime Image per Backend process. The privileged REL
/// executor is therefore installed once after Backend has admitted the
/// Container/runtime authority, then cloned into each request-scoped host
/// capability adapter. Replacing it at runtime is intentionally forbidden.
static REL_HOST_EXECUTOR: OnceLock<Arc<dyn RelHostExecutor>> = OnceLock::new();

pub fn install_rel_host_executor(
    executor: Arc<dyn RelHostExecutor>,
) -> Result<(), RelHostExecutorInstallError> {
    REL_HOST_EXECUTOR
        .set(executor)
        .map_err(|_| RelHostExecutorInstallError::AlreadyInstalled)
}

pub(crate) fn installed_rel_host_executor() -> Option<Arc<dyn RelHostExecutor>> {
    REL_HOST_EXECUTOR.get().cloned()
}

/// Public installation hook reachable through the already-exported
/// `RelHostExecutor` trait object, so Backend does not need Route Engine's
/// internal module path. The string error keeps the private installation error
/// type out of the cross-crate API.
impl dyn RelHostExecutor {
    pub fn install_process_executor(
        executor: Arc<dyn RelHostExecutor>,
    ) -> Result<(), String> {
        install_rel_host_executor(executor).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelHostExecutorInstallError {
    AlreadyInstalled,
}

impl std::fmt::Display for RelHostExecutorInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInstalled => formatter.write_str(
                "trusted REL host executor is already installed for this Backend process",
            ),
        }
    }
}

impl std::error::Error for RelHostExecutorInstallError {}
