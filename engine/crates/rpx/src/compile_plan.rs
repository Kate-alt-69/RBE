//! Language compiler requirements for RPX package authoring.
//!
//! The binary should not decide compiler authority while it is constructing a
//! process. This module first derives the exact compiler/tool set a component
//! needs, then resolves that set through the managed toolchain boundary.

use crate::toolchain::{CompilerResolver, ResolvedCompiler, ToolchainError};
use sdk_package::{JsRuntime, PackageLanguage};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompilerToolRequirement {
    pub name: &'static str,
    pub purpose: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerPlan {
    pub language: PackageLanguage,
    pub tools: Vec<CompilerToolRequirement>,
    pub network_allowed: bool,
    pub use_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCompilerPlan {
    pub language: PackageLanguage,
    pub tools: Vec<ResolvedCompilerTool>,
    pub network_allowed: bool,
    pub use_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCompilerTool {
    pub name: &'static str,
    pub purpose: &'static str,
    pub compiler: ResolvedCompiler,
}

impl CompilerPlan {
    pub fn for_component(
        language: PackageLanguage,
        javascript_runtime: Option<JsRuntime>,
    ) -> Result<Self, CompilePlanError> {
        let tools = match language {
            PackageLanguage::Rust => vec![
                CompilerToolRequirement {
                    name: "cargo",
                    purpose: "Rust package checking and dependency graph execution",
                },
                CompilerToolRequirement {
                    name: "rustc",
                    purpose: "Rust compiler invoked by the managed Cargo toolchain",
                },
            ],
            PackageLanguage::Javascript => match javascript_runtime {
                Some(JsRuntime::Node) => vec![CompilerToolRequirement {
                    name: "node",
                    purpose: "JavaScript ES-module syntax checking",
                }],
                Some(JsRuntime::Bun) => vec![CompilerToolRequirement {
                    name: "bun",
                    purpose: "JavaScript Bun-target compilation",
                }],
                None => return Err(CompilePlanError::MissingJavascriptRuntime),
            },
            PackageLanguage::Typescript => {
                let runtime = match javascript_runtime {
                    Some(JsRuntime::Node) => CompilerToolRequirement {
                        name: "node",
                        purpose: "JavaScript runtime used by the TypeScript compiler toolchain",
                    },
                    Some(JsRuntime::Bun) => CompilerToolRequirement {
                        name: "bun",
                        purpose: "JavaScript runtime declared by the TypeScript package",
                    },
                    None => return Err(CompilePlanError::MissingJavascriptRuntime),
                };
                vec![
                    runtime,
                    CompilerToolRequirement {
                        name: "tsc",
                        purpose: "TypeScript type and syntax checking",
                    },
                ]
            }
            PackageLanguage::Python => vec![CompilerToolRequirement {
                name: "python",
                purpose: "Python syntax compilation",
            }],
            PackageLanguage::Global => return Err(CompilePlanError::UnresolvedGlobalLanguage),
        };

        Ok(Self {
            language,
            tools,
            network_allowed: false,
            use_shell: false,
        })
    }

    pub fn resolve(
        &self,
        resolver: &CompilerResolver,
    ) -> Result<ResolvedCompilerPlan, CompilePlanError> {
        let tools = self
            .tools
            .iter()
            .map(|requirement| {
                Ok(ResolvedCompilerTool {
                    name: requirement.name,
                    purpose: requirement.purpose,
                    compiler: resolver.resolve(requirement.name)?,
                })
            })
            .collect::<Result<Vec<_>, ToolchainError>>()?;

        Ok(ResolvedCompilerPlan {
            language: self.language,
            tools,
            network_allowed: self.network_allowed,
            use_shell: self.use_shell,
        })
    }

    pub fn requires(&self, tool: &str) -> bool {
        self.tools
            .iter()
            .any(|requirement| requirement.name == tool)
    }
}

impl ResolvedCompilerPlan {
    pub fn tool(&self, name: &str) -> Option<&ResolvedCompilerTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub fn all_managed(&self) -> bool {
        self.tools
            .iter()
            .all(|tool| matches!(tool.compiler, ResolvedCompiler::Managed(_)))
    }
}

#[derive(Debug)]
pub enum CompilePlanError {
    MissingJavascriptRuntime,
    UnresolvedGlobalLanguage,
    Toolchain(ToolchainError),
}

impl From<ToolchainError> for CompilePlanError {
    fn from(value: ToolchainError) -> Self {
        Self::Toolchain(value)
    }
}

impl fmt::Display for CompilePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingJavascriptRuntime => write!(
                formatter,
                "JavaScript/TypeScript compiler plan requires runtime = \"node\" or runtime = \"bun\""
            ),
            Self::UnresolvedGlobalLanguage => write!(
                formatter,
                "global package component must resolve to a concrete SDK language before compiler planning"
            ),
            Self::Toolchain(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CompilePlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Toolchain(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolchain::ManagedCompilerToolchain;

    #[test]
    fn rust_plan_requires_cargo_and_rustc_without_network_or_shell() {
        let plan = CompilerPlan::for_component(PackageLanguage::Rust, None).unwrap();
        assert!(plan.requires("cargo"));
        assert!(plan.requires("rustc"));
        assert!(!plan.network_allowed);
        assert!(!plan.use_shell);
    }

    #[test]
    fn javascript_plan_follows_declared_runtime() {
        let node = CompilerPlan::for_component(PackageLanguage::Javascript, Some(JsRuntime::Node))
            .unwrap();
        assert_eq!(
            node.tools.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            ["node"]
        );

        let bun =
            CompilerPlan::for_component(PackageLanguage::Javascript, Some(JsRuntime::Bun)).unwrap();
        assert_eq!(
            bun.tools.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            ["bun"]
        );
    }

    #[test]
    fn typescript_plan_requires_tsc_and_declared_runtime() {
        let plan = CompilerPlan::for_component(PackageLanguage::Typescript, Some(JsRuntime::Node))
            .unwrap();
        assert!(plan.requires("node"));
        assert!(plan.requires("tsc"));
    }

    #[test]
    fn managed_resolution_fails_if_any_required_tool_is_missing() {
        let toolchain = ManagedCompilerToolchain::parse_json(
            r#"{"format":1,"tools":{"cargo":"/opt/rbe/rust/bin/cargo"}}"#,
        )
        .unwrap();
        let resolver = CompilerResolver::managed(toolchain).unwrap();
        let plan = CompilerPlan::for_component(PackageLanguage::Rust, None).unwrap();
        let error = plan.resolve(&resolver).unwrap_err();
        assert!(matches!(
            error,
            CompilePlanError::Toolchain(ToolchainError::UnknownManagedTool(ref name)) if name == "rustc"
        ));
    }

    #[test]
    fn complete_managed_plan_keeps_every_tool_absolute() {
        let toolchain = ManagedCompilerToolchain::parse_json(
            r#"{"format":1,"tools":{"cargo":"/opt/rbe/rust/bin/cargo","rustc":"/opt/rbe/rust/bin/rustc"}}"#,
        )
        .unwrap();
        let resolver = CompilerResolver::managed(toolchain).unwrap();
        let plan = CompilerPlan::for_component(PackageLanguage::Rust, None)
            .unwrap()
            .resolve(&resolver)
            .unwrap();
        assert!(plan.all_managed());
        assert!(matches!(
            plan.tool("cargo").unwrap().compiler,
            ResolvedCompiler::Managed(_)
        ));
    }
}
