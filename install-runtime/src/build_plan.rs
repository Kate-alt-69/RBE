use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use rbe_install_executor::{
    build_invocations, BuildInvocation, ExtractionPlan, ManagedToolchain, SourceFileDigest,
    SourceFileHasher,
};
use rbe_install_orchestrator::InstallSession;
use rbe_library_package::{inspect_zip, ArchivePolicy, HostOs};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

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

impl ManagedPackageBuildPlan {
    /// Materialize the already-verified package archive into the fresh build
    /// source root without executing any package-controlled code.
    ///
    /// The promoted artifact is hashed and re-inspected immediately before
    /// extraction. A caller cannot weaken the extraction policy by mutating a
    /// public plan: the fresh plan derived from the current artifact must equal
    /// the plan produced during trusted build planning.
    pub fn materialize_source(
        &self,
        artifact_path: impl AsRef<Path>,
    ) -> Result<Vec<SourceFileDigest>, ManagedBuildPlanError> {
        let artifact_path = artifact_path.as_ref();
        if !artifact_path.is_absolute() {
            return Err(ManagedBuildPlanError::ArtifactPathMustBeAbsolute {
                package: self.package.clone(),
                path: artifact_path.to_path_buf(),
            });
        }
        let observed_artifact_sha256 = hash_file(artifact_path)?;
        if !observed_artifact_sha256.eq_ignore_ascii_case(&self.artifact_sha256) {
            return Err(ManagedBuildPlanError::ArtifactHashMismatch {
                package: self.package.clone(),
                expected: self.artifact_sha256.clone(),
                actual: observed_artifact_sha256,
            });
        }

        let policy = ArchivePolicy::default();
        let inspected = inspect_zip(File::open(artifact_path)?, policy)?;
        let fresh_extraction = ExtractionPlan::from_inspected(&inspected, &self.source_root)?;
        if fresh_extraction != self.extraction {
            return Err(ManagedBuildPlanError::ExtractionPlanDrift {
                package: self.package.clone(),
            });
        }
        require_safe_extraction_policy(self)?;
        require_fresh_root_parent(&self.source_root)?;
        if std::fs::symlink_metadata(&self.source_root).is_ok() {
            return Err(ManagedBuildPlanError::BuildRootAlreadyExists(
                self.source_root.clone(),
            ));
        }
        std::fs::create_dir(&self.source_root)?;

        let result = materialize_entries(self, artifact_path);
        if result.is_err() {
            cleanup_partial_root(&self.source_root);
        }
        result
    }
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

fn materialize_entries(
    plan: &ManagedPackageBuildPlan,
    artifact_path: &Path,
) -> Result<Vec<SourceFileDigest>, ManagedBuildPlanError> {
    let mut archive = ZipArchive::new(File::open(artifact_path)?)?;
    let mut digests = Vec::new();
    let mut observed_total = 0_u64;

    for entry in &plan.extraction.entries {
        let mut source = archive.by_name(&entry.archive_path)?;
        if source.is_dir() != entry.directory || source.size() != entry.size {
            return Err(ManagedBuildPlanError::ArchiveEntryDrift {
                package: plan.package.clone(),
                path: entry.archive_path.clone(),
            });
        }
        if !entry.destination.starts_with(&plan.source_root) {
            return Err(ManagedBuildPlanError::UnsafeExtractionDestination(
                entry.destination.clone(),
            ));
        }

        if entry.directory {
            ensure_parent_directories(&plan.source_root, &entry.destination)?;
            ensure_directory(&entry.destination)?;
            continue;
        }

        ensure_parent_directories(&plan.source_root, &entry.destination)?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&entry.destination)?;
        let mut hasher = SourceFileHasher::new(&entry.archive_path, entry.size)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read])?;
            destination.write_all(&buffer[..read])?;
        }
        destination.flush()?;
        let digest = hasher.finish()?;
        observed_total = observed_total
            .checked_add(digest.size)
            .ok_or(rbe_install_executor::SourceStageError::SourceSizeOverflow)?;
        digests.push(digest);
    }

    let planned_file_bytes = plan
        .extraction
        .entries
        .iter()
        .filter(|entry| !entry.directory)
        .try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.size)
                .ok_or(rbe_install_executor::SourceStageError::SourceSizeOverflow)
        })?;
    if observed_total != planned_file_bytes {
        return Err(ManagedBuildPlanError::MaterializedSizeMismatch {
            expected: planned_file_bytes,
            actual: observed_total,
        });
    }

    Ok(digests)
}

fn require_safe_extraction_policy(
    plan: &ManagedPackageBuildPlan,
) -> Result<(), ManagedBuildPlanError> {
    let hardening = plan.extraction.hardening;
    if !hardening.require_fresh_root
        || !hardening.reject_existing_destinations
        || hardening.follow_symlinks
        || hardening.preserve_archive_permissions
        || hardening.preserve_archive_timestamps
        || plan.extraction.root != plan.source_root
    {
        return Err(ManagedBuildPlanError::UnsafeExtractionPolicy {
            package: plan.package.clone(),
        });
    }
    Ok(())
}

fn require_fresh_root_parent(root: &Path) -> Result<(), ManagedBuildPlanError> {
    let parent = root
        .parent()
        .ok_or_else(|| ManagedBuildPlanError::BuildRootParentUnavailable(root.to_path_buf()))?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|_| ManagedBuildPlanError::BuildRootParentUnavailable(parent.to_path_buf()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ManagedBuildPlanError::BuildRootParentUnavailable(
            parent.to_path_buf(),
        ));
    }
    Ok(())
}

fn ensure_parent_directories(root: &Path, destination: &Path) -> Result<(), ManagedBuildPlanError> {
    let parent = destination
        .parent()
        .ok_or_else(|| ManagedBuildPlanError::UnsafeExtractionDestination(destination.to_path_buf()))?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| ManagedBuildPlanError::UnsafeExtractionDestination(destination.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ManagedBuildPlanError::UnsafeExtractionDestination(
                destination.to_path_buf(),
            ));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(ManagedBuildPlanError::UnsafeExtractionDestination(current));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), ManagedBuildPlanError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ManagedBuildPlanError::UnsafeExtractionDestination(
                    path.to_path_buf(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, ManagedBuildPlanError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ManagedBuildPlanError::UnsafeArtifactFile(path.to_path_buf()));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn cleanup_partial_root(root: &Path) {
    if let Ok(metadata) = std::fs::symlink_metadata(root) {
        if !metadata.file_type().is_symlink() && metadata.is_dir() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
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
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
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
    #[error("promoted artifact hash changed for package {package:?}: expected {expected}, got {actual}")]
    ArtifactHashMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    #[error("verified extraction plan changed before materialization for package {package:?}")]
    ExtractionPlanDrift { package: String },
    #[error("managed build extraction policy is unsafe for package {package:?}")]
    UnsafeExtractionPolicy { package: String },
    #[error("managed build source root already exists: {0}")]
    BuildRootAlreadyExists(PathBuf),
    #[error("managed build source root parent is unavailable or unsafe: {0}")]
    BuildRootParentUnavailable(PathBuf),
    #[error("unsafe managed build extraction destination: {0}")]
    UnsafeExtractionDestination(PathBuf),
    #[error("promoted package archive entry changed before extraction: package={package:?}, path={path:?}")]
    ArchiveEntryDrift { package: String, path: String },
    #[error("managed build materialized file bytes mismatch: expected {expected}, got {actual}")]
    MaterializedSizeMismatch { expected: u64, actual: u64 },
    #[error("managed build artifact is not a regular non-symlink file: {0}")]
    UnsafeArtifactFile(PathBuf),
    #[error("managed build artifact I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use rbe_library_package::LibraryManifest;
    use rbe_project_package::{LockedProjectPackage, LockedToolchain, ProjectPackageLock};
    use sha2::{Digest, Sha256};
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::{ArtifactStage, VerifiedRegistryPackage};

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
        zip.add_directory("src/", options).unwrap();
        zip.start_file("src/main.rs", options).unwrap();
        zip.write_all(b"fn main() {}\n").unwrap();
        zip.finish().unwrap();
        manifest
    }

    fn graph(artifact: PathBuf, manifest: LibraryManifest) -> VerifiedRootGraph {
        let bytes = std::fs::read(&artifact).unwrap();
        let artifact_sha256 = format!("{:x}", Sha256::digest(&bytes));
        let locked = LockedProjectPackage {
            version: "1.0.0".into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: artifact_sha256.clone(),
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
                sha256: artifact_sha256,
                size_bytes: bytes.len() as u64,
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

    fn build_plan(temp: &tempfile::TempDir, artifact: &Path) -> ManagedPackageBuildPlan {
        let manifest = write_package(artifact, BUILD_MANIFEST);
        let graph = graph(artifact.to_path_buf(), manifest);
        let toolchain = ManagedToolchain::new(BTreeMap::from([(
            "cargo".into(),
            temp.path().join("managed/cargo"),
        )]))
        .unwrap();
        let build_root = temp.path().join("build-session");
        std::fs::create_dir(&build_root).unwrap();
        prepare_managed_build_plans(&graph, &toolchain, &build_root)
            .unwrap()
            .remove("demo")
            .unwrap()
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

    #[test]
    fn materializes_fresh_verified_source_without_executing_build_steps() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("demo.rbe");
        let plan = build_plan(&temp, &artifact);
        let digests = plan.materialize_source(&artifact).unwrap();

        assert_eq!(digests.len(), 2);
        assert_eq!(std::fs::read_to_string(plan.source_root.join("src/main.rs")).unwrap(), "fn main() {}\n");
        assert!(plan.source_root.join("library.toml").is_file());
        assert!(matches!(
            plan.materialize_source(&artifact),
            Err(ManagedBuildPlanError::BuildRootAlreadyExists(_))
        ));
    }

    #[test]
    fn artifact_drift_is_rejected_before_build_root_creation() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("demo.rbe");
        let plan = build_plan(&temp, &artifact);
        let mut file = OpenOptions::new().append(true).open(&artifact).unwrap();
        file.write_all(b"tampered").unwrap();
        file.flush().unwrap();

        assert!(matches!(
            plan.materialize_source(&artifact),
            Err(ManagedBuildPlanError::ArtifactHashMismatch { .. })
        ));
        assert!(!plan.source_root.exists());
    }
}
