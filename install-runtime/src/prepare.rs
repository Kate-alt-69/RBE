//! Trusted materialization of verified package artifacts into fresh install-session source trees.
//!
//! This module deliberately stops before package-controlled process execution. It re-verifies the
//! promoted content-addressed artifact, re-inspects the bounded archive, extracts only regular files
//! and directories into a fresh session root, and computes a deterministic source-tree identity.
//! Build steps are returned as data; executing them requires a sandbox that can enforce the
//! network-dead `BuildInvocation` contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rbe_install_executor::{
    ExtractionPlan, SourceFileDigest, SourceFileHasher, SourceSelection, SourceTreeDigest,
};
use rbe_install_request::validate_registry_package_name;
use rbe_library_package::{inspect_zip, ArchivePolicy, BuildStep, HostOs, LIBRARY_MANIFEST};
use rbe_project_package::ProjectCacheLayout;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::{InstallRuntimeError, VerifiedRegistryPackage, VerifiedRootGraph};

const PREPARATION_ID_DOMAIN: &[u8] = b"RBE-PACKAGE-PREPARATION-V1\0";
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPackageSource {
    pub root: String,
    pub package: String,
    pub version: String,
    pub private: bool,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    pub build_root: PathBuf,
    /// Deterministic identity of every regular file materialized from the verified archive.
    ///
    /// This is a local prepared-tree identity. It is not automatically treated as the registry
    /// `source_sha256` until the registry/source-selection contract explicitly defines the same
    /// file set.
    pub extracted_tree: SourceTreeDigest,
    pub build_steps: Vec<BuildStep>,
    /// Stable identity of the prepared input plus host-selected build plan. This does not mean a
    /// build has executed successfully.
    pub preparation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRootGraph {
    pub session_id: String,
    pub root: String,
    pub install_order: Vec<String>,
    pub session_build_root: PathBuf,
    pub packages: BTreeMap<String, PreparedPackageSource>,
}

/// Materialize one already-verified/promoted root graph into a fresh per-session build tree.
///
/// No package command is executed here. The caller may use `build_steps` to construct managed,
/// shell-free, network-dead build invocations after toolchain/hydration admission.
pub fn prepare_verified_graph_sources(
    project_root: &Path,
    session_id: &str,
    graph: &VerifiedRootGraph,
    host: HostOs,
) -> Result<PreparedRootGraph, InstallRuntimeError> {
    if !project_root.is_absolute() {
        return Err(InstallRuntimeError::PreparationProjectRootMustBeAbsolute(
            project_root.display().to_string(),
        ));
    }
    validate_session_id(session_id)?;
    validate_registry_package_name(&graph.root)?;

    let layout = ProjectCacheLayout::new(project_root);
    let build_parent = layout.rbe_system_cache_root().join("install").join("build");
    ensure_no_symlink_components(&build_parent)?;
    std::fs::create_dir_all(&build_parent)?;
    ensure_no_symlink_components(&build_parent)?;

    let session_build_root = build_parent.join(session_id);
    match std::fs::symlink_metadata(&session_build_root) {
        Ok(_) => {
            return Err(InstallRuntimeError::PreparationRootAlreadyExists(
                session_build_root.display().to_string(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::create_dir(&session_build_root)?;
    ensure_no_symlink_components(&session_build_root)?;

    match prepare_graph_inner(&layout, session_id, graph, host, session_build_root.clone()) {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            if let Err(source) = std::fs::remove_dir_all(&session_build_root) {
                return Err(InstallRuntimeError::PreparationCleanupFailed {
                    path: session_build_root.display().to_string(),
                    source,
                });
            }
            Err(error)
        }
    }
}

fn prepare_graph_inner(
    layout: &ProjectCacheLayout,
    session_id: &str,
    graph: &VerifiedRootGraph,
    host: HostOs,
    session_build_root: PathBuf,
) -> Result<PreparedRootGraph, InstallRuntimeError> {
    let root_build_dir = session_build_root.join(&graph.root);
    std::fs::create_dir(&root_build_dir)?;

    let mut seen = BTreeSet::new();
    let mut packages = BTreeMap::new();
    for package_name in &graph.install_order {
        validate_registry_package_name(package_name)?;
        if !seen.insert(package_name.clone()) {
            return Err(InstallRuntimeError::ResolutionOrderDuplicate {
                root: graph.root.clone(),
                package: package_name.clone(),
            });
        }
        let verified = graph.packages.get(package_name).ok_or_else(|| {
            InstallRuntimeError::VerifiedGraphPackageMissing {
                root: graph.root.clone(),
                package: package_name.clone(),
            }
        })?;
        let prepared = prepare_package(
            layout,
            &graph.root,
            package_name,
            verified,
            host,
            &root_build_dir,
        )?;
        packages.insert(package_name.clone(), prepared);
    }

    for package_name in graph.packages.keys() {
        if !seen.contains(package_name) {
            return Err(InstallRuntimeError::ResolutionOrderIncomplete {
                root: graph.root.clone(),
                package: package_name.clone(),
            });
        }
    }

    Ok(PreparedRootGraph {
        session_id: session_id.to_string(),
        root: graph.root.clone(),
        install_order: graph.install_order.clone(),
        session_build_root,
        packages,
    })
}

fn prepare_package(
    layout: &ProjectCacheLayout,
    root: &str,
    package_name: &str,
    verified: &VerifiedRegistryPackage,
    host: HostOs,
    root_build_dir: &Path,
) -> Result<PreparedPackageSource, InstallRuntimeError> {
    if verified.manifest.name != package_name
        || verified.locked.version != verified.manifest.version
    {
        return Err(InstallRuntimeError::PreparedManifestDrift {
            package: package_name.to_string(),
        });
    }

    let artifact_sha256 = canonical_sha256(&verified.locked.artifact_sha256)?;
    let artifact_path = layout
        .library_artifact_dir(&artifact_sha256)?
        .join("artifact.rbe");
    verify_regular_file_sha256(&artifact_path, &artifact_sha256)?;

    let policy = ArchivePolicy::default();
    let inspected = inspect_zip(File::open(&artifact_path)?, policy)?;
    if inspected.manifest != verified.manifest {
        return Err(InstallRuntimeError::PreparedManifestDrift {
            package: package_name.to_string(),
        });
    }
    let manifest_sha256 = hash_manifest(&artifact_path, policy.max_manifest_bytes)?;
    let expected_manifest_sha256 = canonical_sha256(&verified.manifest_sha256)?;
    let locked_manifest_sha256 = canonical_sha256(&verified.locked.manifest_sha256)?;
    if manifest_sha256 != expected_manifest_sha256 || manifest_sha256 != locked_manifest_sha256 {
        return Err(InstallRuntimeError::PreparedManifestHashMismatch {
            package: package_name.to_string(),
            expected: expected_manifest_sha256,
            actual: manifest_sha256,
        });
    }

    let build_root = root_build_dir.join(package_name);
    let plan = ExtractionPlan::from_inspected(&inspected, &build_root)?;
    materialize_archive(&artifact_path, &plan)?;
    let extracted_tree = source_tree_identity(&plan)?;
    let build_steps = inspected.manifest.build.for_host(host).to_vec();
    let preparation_id = preparation_id(
        root,
        package_name,
        &verified.locked.version,
        &artifact_sha256,
        &locked_manifest_sha256,
        &extracted_tree.sha256,
        host,
        &build_steps,
    );

    Ok(PreparedPackageSource {
        root: root.to_string(),
        package: package_name.to_string(),
        version: verified.locked.version.clone(),
        private: package_name != root,
        artifact_sha256,
        manifest_sha256: locked_manifest_sha256,
        build_root,
        extracted_tree,
        build_steps,
        preparation_id,
    })
}

fn materialize_archive(
    artifact_path: &Path,
    plan: &ExtractionPlan,
) -> Result<(), InstallRuntimeError> {
    if plan.root.try_exists()? {
        return Err(InstallRuntimeError::PreparationRootAlreadyExists(
            plan.root.display().to_string(),
        ));
    }
    let parent = plan
        .root
        .parent()
        .ok_or_else(|| InstallRuntimeError::UnsafeStagingEntry(plan.root.display().to_string()))?;
    ensure_no_symlink_components(parent)?;
    std::fs::create_dir(&plan.root)?;
    ensure_no_symlink_components(&plan.root)?;

    let mut archive = ZipArchive::new(File::open(artifact_path)?)?;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];

    for entry in &plan.entries {
        let mut source = archive.by_name(&entry.archive_path)?;
        if source.is_dir() != entry.directory || source.size() != entry.size {
            return Err(InstallRuntimeError::ExtractedEntryMismatch(
                entry.archive_path.clone(),
            ));
        }

        if entry.directory {
            if entry.size != 0 {
                return Err(InstallRuntimeError::ExtractedEntryMismatch(
                    entry.archive_path.clone(),
                ));
            }
            std::fs::create_dir_all(&entry.destination)?;
            ensure_no_symlink_components(&entry.destination)?;
            continue;
        }

        let destination_parent = entry.destination.parent().ok_or_else(|| {
            InstallRuntimeError::UnsafeStagingEntry(entry.destination.display().to_string())
        })?;
        std::fs::create_dir_all(destination_parent)?;
        ensure_no_symlink_components(destination_parent)?;

        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&entry.destination)?;
        let mut remaining = entry.size;
        while remaining > 0 {
            let limit = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                .unwrap_or(COPY_BUFFER_BYTES);
            let read = source.read(&mut buffer[..limit])?;
            if read == 0 {
                return Err(InstallRuntimeError::ExtractedEntryMismatch(
                    entry.archive_path.clone(),
                ));
            }
            destination.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        let mut extra = [0_u8; 1];
        if source.read(&mut extra)? != 0 {
            return Err(InstallRuntimeError::ExtractedEntryMismatch(
                entry.archive_path.clone(),
            ));
        }
        destination.sync_all()?;
    }
    Ok(())
}

fn source_tree_identity(plan: &ExtractionPlan) -> Result<SourceTreeDigest, InstallRuntimeError> {
    let mut digests = Vec::new();
    for entry in &plan.entries {
        if entry.directory {
            continue;
        }
        let mut source = File::open(&entry.destination)?;
        let mut hasher = SourceFileHasher::new(entry.archive_path.clone(), entry.size)?;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read])?;
        }
        digests.push(hasher.finish()?);
    }

    let selection = SourceSelection::new(
        digests
            .iter()
            .map(|digest: &SourceFileDigest| digest.path.clone()),
    )?;
    Ok(SourceTreeDigest::from_files(&selection, digests)?)
}

fn verify_regular_file_sha256(
    path: &Path,
    expected_sha256: &str,
) -> Result<(), InstallRuntimeError> {
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InstallRuntimeError::UnsafeCacheEntry(
            path.display().to_string(),
        ));
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(InstallRuntimeError::ExistingArtifactMismatch {
            path: path.display().to_string(),
            expected_sha256: expected_sha256.to_string(),
        });
    }
    Ok(())
}

fn hash_manifest(path: &Path, maximum_bytes: u64) -> Result<String, InstallRuntimeError> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut manifest = archive.by_name(LIBRARY_MANIFEST)?;
    if manifest.is_dir() || manifest.size() > maximum_bytes {
        return Err(InstallRuntimeError::InvalidManifestForHashing);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(manifest.size()).unwrap_or(0));
    manifest
        .by_ref()
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(InstallRuntimeError::InvalidManifestForHashing);
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), InstallRuntimeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(InstallRuntimeError::SymlinkedPath(
                    current.display().to_string(),
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_session_id(value: &str) -> Result<(), InstallRuntimeError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(InstallRuntimeError::InvalidPreparationSessionId(
            value.to_string(),
        ));
    }
    Ok(())
}

fn canonical_sha256(value: &str) -> Result<String, InstallRuntimeError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(rbe_install_executor::ExecutorError::InvalidSha256(value.to_string()).into());
    }
    Ok(value.to_ascii_lowercase())
}

#[allow(clippy::too_many_arguments)]
fn preparation_id(
    root: &str,
    package: &str,
    version: &str,
    artifact_sha256: &str,
    manifest_sha256: &str,
    tree_sha256: &str,
    host: HostOs,
    steps: &[BuildStep],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PREPARATION_ID_DOMAIN);
    hash_field(&mut hasher, root);
    hash_field(&mut hasher, package);
    hash_field(&mut hasher, version);
    hash_field(&mut hasher, artifact_sha256);
    hash_field(&mut hasher, manifest_sha256);
    hash_field(&mut hasher, tree_sha256);
    hash_field(&mut hasher, host_key(host));
    hasher.update((steps.len() as u64).to_be_bytes());
    for step in steps {
        hash_field(&mut hasher, &step.program);
        hasher.update((step.args.len() as u64).to_be_bytes());
        for arg in &step.args {
            hash_field(&mut hasher, arg);
        }
    }
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn host_key(host: HostOs) -> &'static str {
    match host {
        HostOs::Windows => "windows",
        HostOs::Linux => "linux",
        HostOs::Macos => "macos",
        HostOs::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write as _};

    use rbe_install_executor::{PromotionPlan, VerifiedDownload};
    use rbe_library_package::{LibraryManifest, LIBRARY_MANIFEST};
    use rbe_project_package::{LockedProjectPackage, ProjectPackageLock};
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::ArtifactStage;

    fn manifest() -> String {
        r#"name = "demo"
version = "1.0.0"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "javascript"
package = "@rbe/sdk"
version = "0.1"

[runtime]
kind = "bun"
version = "1.3"
entry = "src/index.js"

[[build.windows]]
program = "bun"
args = ["test"]

[[build.other]]
program = "bun"
args = ["build"]
"#
        .to_string()
    }

    fn package_bytes(manifest: &str, reverse: bool) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            let options = SimpleFileOptions::default();
            let files = if reverse {
                vec![
                    ("src/index.js", b"export default 1;".as_slice()),
                    (LIBRARY_MANIFEST, manifest.as_bytes()),
                ]
            } else {
                vec![
                    (LIBRARY_MANIFEST, manifest.as_bytes()),
                    ("src/index.js", b"export default 1;".as_slice()),
                ]
            };
            for (path, contents) in files {
                writer.start_file(path, options).unwrap();
                writer.write_all(contents).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    fn graph(project_root: &Path, bytes: &[u8], manifest_text: &str) -> VerifiedRootGraph {
        let layout = ProjectCacheLayout::new(project_root);
        let artifact_sha256 = format!("{:x}", Sha256::digest(bytes));
        let manifest_sha256 = format!("{:x}", Sha256::digest(manifest_text.as_bytes()));
        let artifact_dir = layout.library_artifact_dir(&artifact_sha256).unwrap();
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let artifact_path = artifact_dir.join("artifact.rbe");
        std::fs::write(&artifact_path, bytes).unwrap();

        let locked = LockedProjectPackage {
            version: "1.0.0".into(),
            resolved_from: "registry:demo".into(),
            artifact_url: "https://example.com/demo.rbe".into(),
            artifact_sha256: artifact_sha256.clone(),
            manifest_sha256: manifest_sha256.clone(),
            source_sha256: None,
            dependencies: BTreeMap::new(),
            runtime: None,
            sdk: None,
        };
        let stage = ArtifactStage {
            verified: VerifiedDownload {
                sha256: artifact_sha256.clone(),
                size_bytes: bytes.len() as u64,
            },
            promotion: PromotionPlan {
                verified_partial: layout
                    .library_cache_root()
                    .join(".staging")
                    .join(&artifact_sha256)
                    .join("artifact.rbe.part"),
                final_dir: artifact_dir,
                final_artifact: artifact_path,
                create_final_dir: true,
                replace_existing: false,
                fsync_before_publish: true,
                fsync_parent_after_publish: true,
            },
            resumed_from_bytes: 0,
        };
        let verified = VerifiedRegistryPackage {
            stage,
            manifest: LibraryManifest::parse(manifest_text).unwrap(),
            manifest_sha256,
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
    fn prepares_fresh_tree_and_host_build_plan() {
        let temp = tempdir().unwrap();
        let source = manifest();
        let bytes = package_bytes(&source, false);
        let graph = graph(temp.path(), &bytes, &source);

        let prepared =
            prepare_verified_graph_sources(temp.path(), "session-1", &graph, HostOs::Linux)
                .unwrap();
        let package = prepared.packages.get("demo").unwrap();
        assert_eq!(package.build_steps[0].args, ["build"]);
        assert!(package.build_root.join("src/index.js").is_file());
        assert_eq!(package.extracted_tree.file_count, 2);
        assert_eq!(package.preparation_id.len(), 64);
        assert!(!package.private);
    }

    #[test]
    fn extracted_tree_identity_ignores_zip_entry_order() {
        let source = manifest();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let first_graph = graph(first.path(), &package_bytes(&source, false), &source);
        let second_graph = graph(second.path(), &package_bytes(&source, true), &source);

        let first_prepared =
            prepare_verified_graph_sources(first.path(), "s1", &first_graph, HostOs::Linux)
                .unwrap();
        let second_prepared =
            prepare_verified_graph_sources(second.path(), "s2", &second_graph, HostOs::Linux)
                .unwrap();

        assert_eq!(
            first_prepared.packages["demo"].extracted_tree.sha256,
            second_prepared.packages["demo"].extracted_tree.sha256
        );
    }

    #[test]
    fn rejects_existing_session_build_root_without_deleting_it() {
        let temp = tempdir().unwrap();
        let source = manifest();
        let bytes = package_bytes(&source, false);
        let graph = graph(temp.path(), &bytes, &source);
        let existing = temp.path().join(".cache/rbe/install/build/session-1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("keep.txt"), b"keep").unwrap();

        let error = prepare_verified_graph_sources(temp.path(), "session-1", &graph, HostOs::Linux)
            .unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::PreparationRootAlreadyExists(_)
        ));
        assert_eq!(std::fs::read(existing.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn rejects_tampered_promoted_artifact_before_extraction() {
        let temp = tempdir().unwrap();
        let source = manifest();
        let bytes = package_bytes(&source, false);
        let graph = graph(temp.path(), &bytes, &source);
        let locked = &graph.packages["demo"].locked;
        let artifact = ProjectCacheLayout::new(temp.path())
            .library_artifact_dir(&locked.artifact_sha256)
            .unwrap()
            .join("artifact.rbe");
        std::fs::write(artifact, b"tampered").unwrap();

        let error = prepare_verified_graph_sources(temp.path(), "session-1", &graph, HostOs::Linux)
            .unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::ExistingArtifactMismatch { .. }
        ));
        assert!(!temp
            .path()
            .join(".cache/rbe/install/build/session-1")
            .exists());
    }

    #[test]
    fn rejects_manifest_drift_from_verified_graph() {
        let temp = tempdir().unwrap();
        let source = manifest();
        let bytes = package_bytes(&source, false);
        let mut graph = graph(temp.path(), &bytes, &source);
        graph.packages.get_mut("demo").unwrap().manifest.version = "9.9.9".into();

        let error = prepare_verified_graph_sources(temp.path(), "session-1", &graph, HostOs::Linux)
            .unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::PreparedManifestDrift { .. }
        ));
    }

    #[test]
    fn preparation_id_binds_host_selected_build_plan() {
        let temp = tempdir().unwrap();
        let source = manifest();
        let bytes = package_bytes(&source, false);
        let graph = graph(temp.path(), &bytes, &source);

        let windows =
            prepare_verified_graph_sources(temp.path(), "windows", &graph, HostOs::Windows)
                .unwrap();
        let linux =
            prepare_verified_graph_sources(temp.path(), "linux", &graph, HostOs::Linux).unwrap();
        assert_eq!(windows.packages["demo"].build_steps[0].args, ["test"]);
        assert_eq!(linux.packages["demo"].build_steps[0].args, ["build"]);
        assert_ne!(
            windows.packages["demo"].preparation_id,
            linux.packages["demo"].preparation_id
        );
    }
}
