//! Exact process contracts for RPX compiler checks.
//!
//! This module does not spawn anything. It lowers a resolved compiler plan into
//! explicit process specifications so the RPX binary can execute only the tools
//! already selected by the managed toolchain boundary.

use crate::compile_plan::{ResolvedCompilerPlan, ResolvedCompilerTool};
use crate::toolchain::ResolvedCompiler;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerInvocation {
    pub tool: &'static str,
    pub program: CompilerProgram,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub clear_environment: bool,
    pub environment: BTreeMap<String, String>,
    pub network_allowed: bool,
    pub use_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompilerProgram {
    Managed(PathBuf),
    HostAuthoring(String),
}

impl From<&ResolvedCompiler> for CompilerProgram {
    fn from(value: &ResolvedCompiler) -> Self {
        match value {
            ResolvedCompiler::Managed(path) => Self::Managed(path.clone()),
            ResolvedCompiler::HostAuthoring(name) => Self::HostAuthoring(name.clone()),
        }
    }
}

impl CompilerInvocation {
    fn from_tool(
        tool: &ResolvedCompilerTool,
        working_directory: impl Into<PathBuf>,
        args: Vec<String>,
    ) -> Self {
        Self {
            tool: tool.name,
            program: CompilerProgram::from(&tool.compiler),
            args,
            working_directory: working_directory.into(),
            clear_environment: matches!(tool.compiler, ResolvedCompiler::Managed(_)),
            environment: BTreeMap::new(),
            network_allowed: false,
            use_shell: false,
        }
    }
}

pub fn rust_check(
    plan: &ResolvedCompilerPlan,
    manifest_path: impl AsRef<Path>,
) -> Result<CompilerInvocation, CompilerExecutionError> {
    require_safe_plan(plan)?;
    let cargo = required_tool(plan, "cargo")?;
    let rustc = required_tool(plan, "rustc")?;
    let manifest_path = manifest_path.as_ref();
    let working_directory = manifest_path
        .parent()
        .ok_or(CompilerExecutionError::MissingWorkingDirectory)?;
    let mut invocation = CompilerInvocation::from_tool(
        cargo,
        working_directory,
        vec![
            "check".into(),
            "--quiet".into(),
            "--offline".into(),
            "--manifest-path".into(),
            manifest_path.to_string_lossy().into_owned(),
        ],
    );
    invocation
        .environment
        .insert("RUSTC".into(), compiler_program_value(&rustc.compiler)?);
    invocation
        .environment
        .insert("CARGO_NET_OFFLINE".into(), "true".into());
    Ok(invocation)
}

pub fn node_check(
    plan: &ResolvedCompilerPlan,
    staged_module: impl AsRef<Path>,
) -> Result<CompilerInvocation, CompilerExecutionError> {
    require_safe_plan(plan)?;
    let node = required_tool(plan, "node")?;
    let staged_module = staged_module.as_ref();
    let working_directory = staged_module
        .parent()
        .ok_or(CompilerExecutionError::MissingWorkingDirectory)?;
    Ok(CompilerInvocation::from_tool(
        node,
        working_directory,
        vec![
            "--check".into(),
            staged_module.to_string_lossy().into_owned(),
        ],
    ))
}

pub fn bun_build(
    plan: &ResolvedCompilerPlan,
    source: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
) -> Result<CompilerInvocation, CompilerExecutionError> {
    require_safe_plan(plan)?;
    let bun = required_tool(plan, "bun")?;
    let source = source.as_ref();
    let working_directory = source
        .parent()
        .ok_or(CompilerExecutionError::MissingWorkingDirectory)?;
    Ok(CompilerInvocation::from_tool(
        bun,
        working_directory,
        vec![
            "build".into(),
            source.to_string_lossy().into_owned(),
            "--target=bun".into(),
            "--external=@rbe/sdk".into(),
            "--outdir".into(),
            out_dir.as_ref().to_string_lossy().into_owned(),
        ],
    ))
}

pub fn typescript_check(
    plan: &ResolvedCompilerPlan,
    config_path: impl AsRef<Path>,
) -> Result<CompilerInvocation, CompilerExecutionError> {
    require_safe_plan(plan)?;
    let tsc = required_tool(plan, "tsc")?;
    let config_path = config_path.as_ref();
    let working_directory = config_path
        .parent()
        .ok_or(CompilerExecutionError::MissingWorkingDirectory)?;
    let arguments = vec![
        "--pretty".into(),
        "false".into(),
        "-p".into(),
        config_path.to_string_lossy().into_owned(),
    ];

    match &tsc.compiler {
        ResolvedCompiler::Managed(tsc_entry) => {
            let runtime = plan
                .tool("node")
                .or_else(|| plan.tool("bun"))
                .ok_or(CompilerExecutionError::MissingTypescriptRuntime)?;
            let mut runtime_arguments = Vec::with_capacity(arguments.len() + 1);
            runtime_arguments.push(
                tsc_entry
                    .to_str()
                    .map(ToOwned::to_owned)
                    .ok_or(CompilerExecutionError::NonUtf8ManagedProgram)?,
            );
            runtime_arguments.extend(arguments);
            Ok(CompilerInvocation::from_tool(
                runtime,
                working_directory,
                runtime_arguments,
            ))
        }
        ResolvedCompiler::HostAuthoring(_) => Ok(CompilerInvocation::from_tool(
            tsc,
            working_directory,
            arguments,
        )),
    }
}

pub fn python_compile(
    plan: &ResolvedCompilerPlan,
    source: impl AsRef<Path>,
    pycache: impl AsRef<Path>,
) -> Result<CompilerInvocation, CompilerExecutionError> {
    require_safe_plan(plan)?;
    let python = required_tool(plan, "python")?;
    let source = source.as_ref();
    let working_directory = source
        .parent()
        .ok_or(CompilerExecutionError::MissingWorkingDirectory)?;
    let mut invocation = CompilerInvocation::from_tool(
        python,
        working_directory,
        vec![
            "-m".into(),
            "py_compile".into(),
            source.to_string_lossy().into_owned(),
        ],
    );
    invocation.environment.insert(
        "PYTHONPYCACHEPREFIX".into(),
        pycache.as_ref().to_string_lossy().into_owned(),
    );
    Ok(invocation)
}

fn required_tool<'a>(
    plan: &'a ResolvedCompilerPlan,
    name: &'static str,
) -> Result<&'a ResolvedCompilerTool, CompilerExecutionError> {
    plan.tool(name)
        .ok_or(CompilerExecutionError::RequiredToolMissing(name))
}

fn require_safe_plan(plan: &ResolvedCompilerPlan) -> Result<(), CompilerExecutionError> {
    if plan.network_allowed {
        return Err(CompilerExecutionError::NetworkEnabled);
    }
    if plan.use_shell {
        return Err(CompilerExecutionError::ShellEnabled);
    }
    Ok(())
}

fn compiler_program_value(program: &ResolvedCompiler) -> Result<String, CompilerExecutionError> {
    match program {
        ResolvedCompiler::Managed(path) => path
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or(CompilerExecutionError::NonUtf8ManagedProgram),
        ResolvedCompiler::HostAuthoring(name) => Ok(name.clone()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompilerExecutionError {
    RequiredToolMissing(&'static str),
    MissingWorkingDirectory,
    MissingTypescriptRuntime,
    NetworkEnabled,
    ShellEnabled,
    NonUtf8ManagedProgram,
}

impl fmt::Display for CompilerExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredToolMissing(tool) => {
                write!(
                    formatter,
                    "resolved RPX compiler plan is missing required tool {tool:?}"
                )
            }
            Self::MissingWorkingDirectory => {
                write!(formatter, "RPX compiler input has no working directory")
            }
            Self::MissingTypescriptRuntime => write!(
                formatter,
                "managed TypeScript compiler plan is missing its declared Node/Bun runtime"
            ),
            Self::NetworkEnabled => write!(
                formatter,
                "RPX compiler execution refuses a plan that permits network access"
            ),
            Self::ShellEnabled => write!(
                formatter,
                "RPX compiler execution refuses a plan that permits shell execution"
            ),
            Self::NonUtf8ManagedProgram => write!(
                formatter,
                "managed compiler path cannot be represented in the compiler environment"
            ),
        }
    }
}

impl std::error::Error for CompilerExecutionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_plan::CompilerPlan;
    use crate::toolchain::{CompilerResolver, ManagedCompilerToolchain};
    use sdk_package::{JsRuntime, PackageLanguage};

    fn managed(json: &str) -> CompilerResolver {
        CompilerResolver::managed(ManagedCompilerToolchain::parse_json(json).unwrap()).unwrap()
    }

    #[test]
    fn rust_invocation_is_offline_shellless_and_pins_rustc() {
        let resolver = managed(
            r#"{"format":1,"tools":{"cargo":"/opt/rbe/rust/bin/cargo","rustc":"/opt/rbe/rust/bin/rustc"}}"#,
        );
        let plan = CompilerPlan::for_component(PackageLanguage::Rust, None)
            .unwrap()
            .resolve(&resolver)
            .unwrap();
        let invocation = rust_check(&plan, "/tmp/rbe-check/Cargo.toml").unwrap();
        assert_eq!(
            invocation.program,
            CompilerProgram::Managed(PathBuf::from("/opt/rbe/rust/bin/cargo"))
        );
        assert!(invocation.args.contains(&"--offline".to_string()));
        assert_eq!(
            invocation.environment.get("RUSTC").map(String::as_str),
            Some("/opt/rbe/rust/bin/rustc")
        );
        assert_eq!(
            invocation
                .environment
                .get("CARGO_NET_OFFLINE")
                .map(String::as_str),
            Some("true")
        );
        assert!(invocation.clear_environment);
        assert!(!invocation.network_allowed);
        assert!(!invocation.use_shell);
    }

    #[test]
    fn node_invocation_uses_exact_managed_program() {
        let resolver = managed(r#"{"format":1,"tools":{"node":"/opt/rbe/node/bin/node"}}"#);
        let plan = CompilerPlan::for_component(PackageLanguage::Javascript, Some(JsRuntime::Node))
            .unwrap()
            .resolve(&resolver)
            .unwrap();
        let invocation = node_check(&plan, "/tmp/check/hello.mjs").unwrap();
        assert_eq!(
            invocation.program,
            CompilerProgram::Managed(PathBuf::from("/opt/rbe/node/bin/node"))
        );
        assert_eq!(invocation.args[0], "--check");
        assert!(invocation.clear_environment);
    }

    #[test]
    fn host_authoring_invocation_keeps_the_developer_environment() {
        let plan = ResolvedCompilerPlan {
            language: PackageLanguage::Javascript,
            tools: vec![ResolvedCompilerTool {
                name: "node",
                purpose: "authoring test",
                compiler: ResolvedCompiler::HostAuthoring("node".into()),
            }],
            network_allowed: false,
            use_shell: false,
        };
        let invocation = node_check(&plan, "/tmp/check/hello.mjs").unwrap();
        assert!(!invocation.clear_environment);
        assert_eq!(invocation.program, CompilerProgram::HostAuthoring("node".into()));
    }

    #[test]
    fn managed_typescript_runs_tsc_through_declared_runtime() {
        let resolver = managed(
            r#"{"format":1,"tools":{"node":"/opt/rbe/node/bin/node","tsc":"/opt/rbe/typescript/lib/tsc.js"}}"#,
        );
        let plan = CompilerPlan::for_component(PackageLanguage::Typescript, Some(JsRuntime::Node))
            .unwrap()
            .resolve(&resolver)
            .unwrap();
        let invocation = typescript_check(&plan, "/tmp/check/tsconfig.json").unwrap();
        assert_eq!(
            invocation.program,
            CompilerProgram::Managed(PathBuf::from("/opt/rbe/node/bin/node"))
        );
        assert_eq!(invocation.args[0], "/opt/rbe/typescript/lib/tsc.js");
        assert!(invocation.clear_environment);
    }

    #[test]
    fn python_invocation_sets_private_pycache_location() {
        let resolver = managed(r#"{"format":1,"tools":{"python":"/opt/rbe/python/bin/python"}}"#);
        let plan = CompilerPlan::for_component(PackageLanguage::Python, None)
            .unwrap()
            .resolve(&resolver)
            .unwrap();
        let invocation = python_compile(&plan, "/tmp/pkg/hello.py", "/tmp/cache/pycache").unwrap();
        assert_eq!(
            invocation
                .environment
                .get("PYTHONPYCACHEPREFIX")
                .map(String::as_str),
            Some("/tmp/cache/pycache")
        );
        assert_eq!(invocation.args[..2], ["-m", "py_compile"]);
    }

    #[test]
    fn hostile_execution_policy_is_rejected_before_process_creation() {
        let resolver = managed(r#"{"format":1,"tools":{"node":"/opt/rbe/node/bin/node"}}"#);
        let mut plan =
            CompilerPlan::for_component(PackageLanguage::Javascript, Some(JsRuntime::Node))
                .unwrap()
                .resolve(&resolver)
                .unwrap();
        plan.network_allowed = true;
        assert_eq!(
            node_check(&plan, "/tmp/check/hello.mjs").unwrap_err(),
            CompilerExecutionError::NetworkEnabled
        );
    }
}
