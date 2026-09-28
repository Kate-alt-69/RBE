use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use rbe_install_executor::{build_invocations, BuildInvocation, ExtractionPlan, ManagedToolchain};
use rbe_install_orchestrator::InstallSession;
use rbe_library_package::{inspect_zip, ArchivePolicy, HostOs};

use crate::VerifiedRootGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPackageBuildPlan {
    pub instance: String,
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub expected_source_sha256: Option<String>,
    pub source_root: PathBuf,
    pub extraction: ExtractionPlan,
    pub invocations: Vec<BuildInvocation>,
}

pub fn prepare_managed_build_plans(
    graph: &VerifiedRootGraph,
    toolchain: &ManagedToolchain,
    build_root: impl AsRef<Path>,
) -> Result<BTreeMap<String, ManagedPackageBuildPlan>, ManagedBuildPlanError> {
    let build_root = build_root.as_ref();
    if build_root.as_os_str().is_empty() || !build_root.is_absolute() {
        return Err(ManagedBuildPlanError::BuildRootMustBeAbsolute);
    }

    let host_os = current_host_os();
    let mut plans = BTreeMap::new();

    for (position, package) in graph.install_order.iter().enumerate() {
        let verified = graph.packages.get(package).ok_or_else(|| {
            ManagedBuildPlanError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package.clone(),
            }
        })?;
        let build_steps = verified.manifest.build.for_host(host_os);
        if build_steps.is_empty() {
            continue;
        }

        let locked = graph
            .lock
            .locked_for_root(&graph.root, package)
            .ok_or_else(|| ManagedBuildPlanError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package.clone(),
            })?;
        let artifact_path = &verified.stage.promotion.final_artifact;
        if !artifact_path.is_absolute() {
            return Err(ManagedBuildPlanError::ArtifactPathMustBeAbsolute {
                package: package.clone(),
                path: artifact_path.clone(),
            });
        }

        let policy = ArchivePolicy::default();
        let inspected = inspect_zip(File::open(artifact_path)?, policy)?;
        if inspected.manifest != verified.manifest {
            return Err(ManagedBuildPlanError::ManifestDrift {
                package: package.clone(),
            });
        }

        let short_sha = locked.artifact_sha256.get(..16).ok_or_else(|| {
            ManagedBuildPlanError::InvalidArtifactSha256 {
                package: package.clone(),
            }
        })?;
        let source_root = build_root.join(format!("{position:04}-{short_sha}"));
        let extraction = ExtractionPlan::from_inspected(&inspected, &source_root)?;
        let invocations = build_invocations(build_steps, toolchain, &source_root)?;
        if invocations.iter().any(|invocation| {
            invocation.network_allowed || invocation.use_shell || !invocation.clear_environment
        }) {
            return Err(ManagedBuildPlanError::UnsafeInvocation {
                package: package.clone(),
            });
        }

        let instance = if package == &graph.root {
            package.clone()
        } else {
            InstallSession::private_instance_id(&graph.root, package)?
        };
        let plan = ManagedPackageBuildPlan {
            instance: instance.clone(),
            package: package.clone(),
            version: locked.version.clone(),
            artifact_sha256: locked.artifact_sha256.clone(),
            expected_source_sha256: locked.source_sha256.clone(),
            source_root,
            extraction,
            invocations,
        };
        if plans.insert(instance.clone(), plan).is_some() {
            return Err(ManagedBuildPlanError::DuplicateBuildInstance(instance));
        }
    }

    Ok(plans)
}

const fn current_host_os() -> HostOs {
    if cfg!(target_os = "windows") {
        HostOs::Windows
    } else if cfg!(target_os = "linux") {
        HostOs::Linux
    } else if cfg!(target_os = "macos") {
        HostOs::Macos
    } else {
        HostOs::Other
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedBuildPlanError {
    #[error(transparent)]
    Archive(#[from] rbe_library_package::ArchiveError),
    #[error(transparent)]
    Executor(#[from] rbe_install_executor::ExecutorError),
    #[error(transparent)]
    Source(#[from] rbe_install_executor::SourceStageError),
    #[error(transparent)]
    Session(#[from] rbe_install_orchestrator::SessionError),
    #[error("managed build root must be an absolute path")]
    BuildRootMustBeAbsolute,
    #[error("verified root graph {root:?} is missing staged package {package:?}")]
    VerifiedGraphPackageMissing { root: String, package: String },
    #[error("promoted artifact for package {package:?} must use an absolute path: {path}")]
    ArtifactPathMustBeAbsolute { package: String, path: PathBuf },
    #[error("verified package manifest drifted before managed build planning for {package:?}")]
    ManifestDrift { package: String },
    #[error("locked artifact SHA-256 is malformed for package {package:?}")]
    InvalidArtifactSha256 { package: String },
    #[error("managed build invocation policy became unsafe for package {package:?}")]
    UnsafeInvocation { package: String },
    #[error("duplicate managed build instance {0:?}")]
    DuplicateBuildInstance(String),
    #[error("managed build artifact I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use rbe_library_package::LibraryManifest;
    use rbe_project_package::{LockedProjectPackage, LockedToolchain, ProjectPackageLock};
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::{ArtifactStage, VerifiedRegistryPackage};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SOURCE_SHA: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const PREBUILT_MANIFEST: &str = r#"
name = "demo"
version = "1.0.0"
language = "rust"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "rust"
package = "rbe-sdk"
version = "0.1"

[runtime]
kind = "rust"
version = "1.98"
managed = true
entry = "src/main.rs"
"#;
    const BUILD_MANIFEST: &str = r#"
name = "demo"
version = "1.0.0"
language = "rust"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "rust"
package = "rbe-sdk"
version = "0.1"

[runtime]
kind = "rust"
version = "1.98"
managed = true
entry = "src/main.rs"

[[build.other]]
program = "cargo"
args = ["build", "--release"]
"#;

    fn write_package(path: &Path, manifest_source: &str) -> LibraryManifest {
        let manifest = LibraryManifest::parse(manifest_source).unwrap();
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("library.toml", options).unwrap();
        zip.write_all(manifest_source.as_bytes()).unwrap();
        zip.start_file("src/main.rs", options).unwrap();
        zip.write_all(b"fn main() {}\n").unwrap();
        zip.finish().unwrap();
        manifest
    }

    fn graph(artifact: PathBuf, manifest: LibraryManifest) -> VerifiedRootGraph {
        let locked = LockedProjectPackage {
            version: "1.0.0".into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: SHA.into(),
            manifest_sha256: "b".repeat(64),
            source_sha256: Some(SOURCE_SHA.into()),
            dependencies: Default::default(),
            runtime: Some(LockedToolchain {
                kind: "rust".into(),
                version: "1.98".into(),
            }),
            sdk: Some(LockedToolchain {
                kind: "rust".into(),
                version: "0.1".into(),
            }),
        };
        let stage = ArtifactStage {
            verified: VerifiedDownload {
                sha256: SHA.into(),
                size_bytes: 10,
            },
            promotion: PromotionPlan {
                verified_partial: artifact.with_extension("part"),
                final_dir: artifact.parent().unwrap().to_path_buf(),
                final_artifact: artifact,
                create_final_dir: false,
                replace_existing: false,
                fsync_before_publish: true,
                fsync_parent_after_publish: true,
            },
            resumed_from_bytes: 0,
        };
        let verified = VerifiedRegistryPackage {
            stage,
            manifest,
            manifest_sha256: "b".repeat(64),
            locked: locked.clone(),
        };
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert("demo".into(), locked);
        VerifiedRootGraph {
            root: "demo".into(),
            install_order: vec!["demo".into()],
            lock,
            packages: BTreeMap::from([("demo".into(), verified)]),
        }
    }

    #[test]
    fn host_build_plan_uses_absolute_managed_tool_without_shell_or_network() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("demo.rbe");
        let manifest = write_package(&artifact, BUILD_MANIFEST);
        let graph = graph(artifact, manifest);
        let toolchain = ManagedToolchain::new(BTreeMap::from([(
            "cargo".into(),
            temp.path().join("managed/cargo"),
        )]))
        .unwrap();
        let plans =
            prepare_managed_build_plans(&graph, &toolchain, temp.path().join("build-session"))
                .unwrap();
        let plan = &plans["demo"];
        assert_eq!(plan.expected_source_sha256.as_deref(), Some(SOURCE_SHA));
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].args, ["build", "--release"]);
        assert!(plan.invocations[0].program.is_absolute());
        assert!(!plan.invocations[0].network_allowed);
        assert!(!plan.invocations[0].use_shell);
        assert!(plan.invocations[0].clear_environment);
        assert!(plan.extraction.hardening.require_fresh_root);
        assert!(!plan.extraction.hardening.follow_symlinks);
    }

    #[test]
    fn prebuilt_package_needs_no_managed_build_plan() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("demo.rbe");
        let manifest = write_package(&artifact, PREBUILT_MANIFEST);
        let graph = graph(artifact, manifest);
        let toolchain = ManagedToolchain::new(BTreeMap::from([(
            "cargo".into(),
            temp.path().join("managed/cargo"),
        )]))
        .unwrap();
        let plans =
            prepare_managed_build_plans(&graph, &toolchain, temp.path().join("build-session"))
                .unwrap();
        assert!(plans.is_empty());
    }

    #[test]
    fn unknown_build_tool_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("demo.rbe");
        let manifest = write_package(&artifact, BUILD_MANIFEST);
        let graph = graph(artifact, manifest);
        let toolchain = ManagedToolchain::new(BTreeMap::from([(
            "rustc".into(),
            temp.path().join("managed/rustc"),
        )]))
        .unwrap();
        assert!(matches!(
            prepare_managed_build_plans(
                &graph,
                &toolchain,
                temp.path().join("build-session")
            ),
            Err(ManagedBuildPlanError::Executor(
                rbe_install_executor::ExecutorError::UnknownManagedTool(tool)
            )) if tool == "cargo"
        ));
    }
}
