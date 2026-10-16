//! Script execution planning for REL.
//!
//! This module deliberately does not spawn processes. It maps a symbolic
//! workspace path to one of RBE's managed runtime identities. The trusted
//! Backend/Container executor owns runtime lookup, sandboxing and execution.

use std::fmt;

use crate::workspace::{WorkspacePath, WorkspacePlanError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLanguage {
    JavaScript,
    TypeScript,
    Python,
    PyPy,
    Rust,
}

impl ScriptLanguage {
    pub const fn runtime_identity(self) -> &'static str {
        match self {
            Self::JavaScript => "rbe.sys.nodejs",
            Self::TypeScript => "rbe.sys.bunjs",
            Self::Python => "rbe.sys.python",
            Self::PyPy => "rbe.sys.pypy",
            Self::Rust => "rbe.sys.rust",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptPath {
    workspace: WorkspacePath,
    language: ScriptLanguage,
}

impl ScriptPath {
    pub fn infer(path: &str) -> Result<Self, ScriptPlanError> {
        let workspace = parse_script_workspace(path)?;
        let lower = workspace.relative().to_ascii_lowercase();
        let language =
            if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
                ScriptLanguage::JavaScript
            } else if lower.ends_with(".ts") || lower.ends_with(".mts") || lower.ends_with(".cts") {
                ScriptLanguage::TypeScript
            } else if lower.ends_with(".py") {
                ScriptLanguage::Python
            } else {
                return Err(ScriptPlanError::UnsupportedExtension(path.to_string()));
            };
        Ok(Self {
            workspace,
            language,
        })
    }

    pub fn explicit(path: &str, language: ScriptLanguage) -> Result<Self, ScriptPlanError> {
        let workspace = parse_script_workspace(path)?;
        Ok(Self {
            workspace,
            language,
        })
    }

    pub fn workspace(&self) -> &WorkspacePath {
        &self.workspace
    }

    pub fn language(&self) -> ScriptLanguage {
        self.language
    }

    pub fn runtime_identity(&self) -> &'static str {
        self.language.runtime_identity()
    }
}

/// Script helpers accept `/path/to/file` as a project-relative DX shorthand.
/// It is deliberately normalized into the symbolic `$$/` namespace before the
/// host sees it; a leading slash here is never an OS-root path. Canonical
/// `$$/` and `??/` paths remain accepted unchanged.
fn parse_script_workspace(path: &str) -> Result<WorkspacePath, ScriptPlanError> {
    let workspace = if let Some(relative) = path.strip_prefix('/') {
        WorkspacePath::project(relative)
    } else {
        WorkspacePath::parse(path)
    };
    workspace.map_err(ScriptPlanError::Workspace)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptPlan {
    pub path: ScriptPath,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

impl ScriptPlan {
    pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    pub const MAX_TIMEOUT_MS: u64 = 15 * 60_000;
    pub const MAX_ARGS: usize = 128;
    pub const MAX_ARG_BYTES: usize = 16 * 1024;

    pub fn run(path: &str) -> Result<Self, ScriptPlanError> {
        Ok(Self {
            path: ScriptPath::infer(path)?,
            args: Vec::new(),
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
        })
    }

    pub fn run_pypy(path: &str) -> Result<Self, ScriptPlanError> {
        Ok(Self {
            path: ScriptPath::explicit(path, ScriptLanguage::PyPy)?,
            args: Vec::new(),
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
        })
    }

    pub fn run_rust(path: &str) -> Result<Self, ScriptPlanError> {
        Ok(Self {
            path: ScriptPath::explicit(path, ScriptLanguage::Rust)?,
            args: Vec::new(),
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
        })
    }

    pub fn with_args(mut self, args: Vec<String>) -> Result<Self, ScriptPlanError> {
        if args.len() > Self::MAX_ARGS {
            return Err(ScriptPlanError::TooManyArguments(args.len()));
        }
        if let Some((index, _)) = args
            .iter()
            .enumerate()
            .find(|(_, value)| value.len() > Self::MAX_ARG_BYTES || value.contains('\0'))
        {
            return Err(ScriptPlanError::InvalidArgument(index));
        }
        self.args = args;
        Ok(self)
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Result<Self, ScriptPlanError> {
        if timeout_ms == 0 || timeout_ms > Self::MAX_TIMEOUT_MS {
            return Err(ScriptPlanError::InvalidTimeout(timeout_ms));
        }
        self.timeout_ms = timeout_ms;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptPlanError {
    Workspace(WorkspacePlanError),
    UnsupportedExtension(String),
    TooManyArguments(usize),
    InvalidArgument(usize),
    InvalidTimeout(u64),
}

impl fmt::Display for ScriptPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => write!(f, "{error}"),
            Self::UnsupportedExtension(path) => write!(
                f,
                "cannot infer script runtime from {path:?}; use .js/.ts/.py or an explicit runtime helper"
            ),
            Self::TooManyArguments(count) => write!(f, "script has {count} arguments; maximum is {}", ScriptPlan::MAX_ARGS),
            Self::InvalidArgument(index) => write!(f, "script argument {index} is too large or contains NUL"),
            Self::InvalidTimeout(value) => write!(f, "script timeout {value}ms is outside the allowed range"),
        }
    }
}

impl std::error::Error for ScriptPlanError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceRoot;

    #[test]
    fn infers_managed_runtime_from_extension() {
        assert_eq!(
            ScriptPath::infer("$$/scripts/build.js")
                .unwrap()
                .runtime_identity(),
            "rbe.sys.nodejs"
        );
        assert_eq!(
            ScriptPath::infer("$$/scripts/build.ts")
                .unwrap()
                .runtime_identity(),
            "rbe.sys.bunjs"
        );
        assert_eq!(
            ScriptPath::infer("??/job.py").unwrap().runtime_identity(),
            "rbe.sys.python"
        );
    }

    #[test]
    fn leading_slash_is_project_relative_not_host_absolute() {
        let script = ScriptPath::infer("/mycool/script.js").unwrap();
        assert_eq!(script.workspace().root(), WorkspaceRoot::Project);
        assert_eq!(script.workspace().relative(), "mycool/script.js");
        assert_eq!(script.workspace().symbolic(), "$$/mycool/script.js");
        assert!(ScriptPath::infer("//host/escape.js").is_err());
        assert!(ScriptPath::infer(r"C:\\host\\escape.js").is_err());
    }

    #[test]
    fn pypy_is_explicit_and_first_class() {
        assert_eq!(
            ScriptPlan::run_pypy("??/job.py")
                .unwrap()
                .path
                .runtime_identity(),
            "rbe.sys.pypy"
        );
    }
}
