//! Application package installation for `package.rbe.json` projects.
//!
//! Fresh installs resolve against one frozen Kastrick index revision, download
//! through RBE's hardened install executor, verify the published package YAML
//! and RPX package index before cache promotion, and write a root-scoped JSON
//! lock. Matching locks are rehydrated directly from their pinned URLs/hashes
//! without widening private dependency visibility.

use crate::project::{
    find_project_root, LocalIndex, LockedPackage, ProjectLock, ProjectManifest, ProjectPaths,
};
use crate::registry_client::RegistryClient;
use crate::registry_contract::ResolvedRegistryRelease;
use anyhow::{bail, Context, Result};
use rbe_install_executor::{ArtifactDownloadPlan, DownloadLimits, ResumePolicy};
use rbe_install_runtime::{promote_artifact, stage_artifact};
use sdk_package::canonical_export_id;
use semver::{Version, VersionReq};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::runtime::Runtime;
use url::Url;
use zip::ZipArchive;

pub const ARCHIVE_PACKAGE_MANIFEST: &str = "package.rbe.yaml";
pub const ARCHIVE_PACKAGE_INDEX: &str = ".rbe/package-index.json";
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_PACKAGE_INDEX_BYTES: u64 = 512 * 1024;
const LEGACY_PACKAGE_INDEX_FORMAT: u64 = 1;
const PHASE3_PACKAGE_INDEX_FORMAT: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub project_root: PathBuf,
    pub lock_path: PathBuf,
    pub index_revision: Option<String>,
    pub roots: usize,
    pub private_packages: usize,
    pub reused_lock: bool,
}

#[derive(Debug, Deserialize)]
struct ArchivePackageManifest {
    package: ArchivePackageIdentity,
    #[serde(default)]
    dependencies: ArchiveDependencies,
}

#[derive(Debug, Deserialize)]
struct ArchivePackageIdentity {
    name: String,
    version: String,
}

#[derive(Debug, Default, Deserialize)]
struct ArchiveDependencies {
    #[serde(default)]
    rbe: BTreeMap<String, String>,
}

pub fn install_application(
    start: impl AsRef<Path>,
    registry_override: Option<&str>,
) -> Result<InstallReport> {
    let project_root = find_project_root(start.as_ref())?;
    let manifest = ProjectManifest::load(&project_root)?;
    let existing = ProjectLock::load(&project_root)?;
    let runtime = Runtime::new().context("failed to initialize RPX install executor")?;

    if let Some(lock) = existing.as_ref() {
        if lock.matches_manifest(&manifest)? && reusable_lock_matches_manifest(&manifest, lock)? {
            materialize_locked_graph(&runtime, &project_root, &manifest, lock)?;
            return report(&project_root, lock, true);
        }
    }

    for (package, requirement) in &manifest.packages {
        if let Some(source) = requirement.source() {
            bail!(
                "package {package:?} uses custom source {source:?}; RPX install requires registry-pinned artifacts until source hashes are part of package.rbe.json"
            );
        }
    }

    if manifest.packages.is_empty() {
        let lock = ProjectLock::new(&manifest)?;
        let lock_path = lock.write(&project_root)?;
        return Ok(InstallReport {
            project_root,
            lock_path,
            index_revision: None,
            roots: 0,
            private_packages: 0,
            reused_lock: false,
        });
    }

    let client = RegistryClient::from_override_or_env(registry_override)?;
    let index_response = client.fetch_index_list()?;
    let revision = index_response.revision.clone();
    index_response.persist_local_cache(&project_root)?;
    let index = LocalIndex::load(&project_root)?
        .context("RPX registry index refresh completed without a local index cache")?;
    if index.revision != revision {
        bail!("RPX local index revision changed during install");
    }

    let mut lock = ProjectLock::new(&manifest)?;
    lock.index_revision = Some(revision.clone());

    for (root, requirement) in &manifest.packages {
        let requested = requirement.version().unwrap_or("*");
        let mut private = BTreeMap::new();
        let mut visiting = BTreeSet::new();
        let root_locked = resolve_and_materialize(
            &runtime,
            &client,
            &index,
            &revision,
            &project_root,
            root,
            root,
            requested,
            &mut private,
            &mut visiting,
            true,
        )?;
        lock.packages.insert(root.clone(), root_locked);
        if !private.is_empty() {
            lock.private.insert(root.clone(), private);
        }
    }

    lock.validate()?;
    let lock_path = lock.write(&project_root)?;
    let mut report = report(&project_root, &lock, false)?;
    report.lock_path = lock_path;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn resolve_and_materialize(
    runtime: &Runtime,
    client: &RegistryClient,
    index: &LocalIndex,
    revision: &str,
    project_root: &Path,
    root: &str,
    package: &str,
    requirement: &str,
    private: &mut BTreeMap<String, LockedPackage>,
    visiting: &mut BTreeSet<String>,
    is_root: bool,
) -> Result<LockedPackage> {
    let requirement_parsed = VersionReq::parse(requirement)
        .with_context(|| format!("invalid package requirement {requirement:?} for {package:?}"))?;

    if !is_root && visiting.contains(package) {
        bail!("cyclic RBE package dependency under root {root:?} at {package:?}");
    }
    if !is_root {
        if let Some(existing) = private.get(package) {
            let version = Version::parse(&existing.version)?;
            if requirement_parsed.matches(&version) {
                return Ok(existing.clone());
            }
            bail!(
                "private dependency conflict under root {root:?}: {package:?} resolved to {} but also requires {requirement}",
                existing.version
            );
        }
    }

    if !visiting.insert(package.to_string()) {
        bail!("cyclic RBE package dependency under root {root:?} at {package:?}");
    }

    let exact = index.resolve(package, requirement)?.with_context(|| {
        format!("RPX index revision {revision:?} cannot satisfy {package:?} {requirement:?}")
    })?;
    let release = client.resolve_package(package, Some(&exact))?;
    if release.revision != revision {
        bail!(
            "RPX registry revision changed during install: index={revision:?}, package response={:?}; retry install",
            release.revision
        );
    }

    let dependencies = materialize_release(runtime, project_root, &release)?;
    let locked = LockedPackage {
        requested: requirement.to_string(),
        version: release.version.clone(),
        artifact_url: release.artifact_url.clone(),
        artifact_sha256: release.artifact_sha256.clone(),
        manifest_sha256: release.manifest_sha256.clone(),
        artifact_size: release.artifact_size,
        dependencies: dependencies.clone(),
    };

    if !is_root {
        private.insert(package.to_string(), locked.clone());
    }

    for (dependency, dependency_requirement) in dependencies {
        if dependency == root {
            bail!(
                "RBE package dependency cycle under root {root:?}: {package:?} depends on the root"
            );
        }
        resolve_and_materialize(
            runtime,
            client,
            index,
            revision,
            project_root,
            root,
            &dependency,
            &dependency_requirement,
            private,
            visiting,
            false,
        )?;
    }

    visiting.remove(package);
    Ok(locked)
}

fn materialize_locked_graph(
    runtime: &Runtime,
    project_root: &Path,
    manifest: &ProjectManifest,
    lock: &ProjectLock,
) -> Result<()> {
    for (root, requirement) in &manifest.packages {
        if requirement.source().is_some() {
            bail!("custom-source package {root:?} cannot reuse an RPX registry lock");
        }
        let locked = lock
            .packages
            .get(root)
            .with_context(|| format!("matching RPX lock is missing root {root:?}"))?;
        let requested = requirement.version().unwrap_or("*");
        require_version_match(root, requested, &locked.version)?;
        let dependencies = materialize_locked(runtime, project_root, root, locked)?;
        if dependencies != locked.dependencies {
            bail!("locked dependency metadata for root {root:?} no longer matches its package archive");
        }

        let graph = lock.private.get(root);
        validate_locked_dependencies(root, locked, graph)?;
        if let Some(graph) = graph {
            for (package, dependency) in graph {
                let dependencies = materialize_locked(runtime, project_root, package, dependency)?;
                if dependencies != dependency.dependencies {
                    bail!(
                        "locked dependency metadata for {package:?} under root {root:?} no longer matches its package archive"
                    );
                }
            }
        }
    }
    Ok(())
}

fn reusable_lock_matches_manifest(manifest: &ProjectManifest, lock: &ProjectLock) -> Result<bool> {
    if manifest.packages.len() != lock.packages.len() {
        return Ok(false);
    }
    for (name, requirement) in &manifest.packages {
        if requirement.source().is_some() {
            return Ok(false);
        }
        let Some(locked) = lock.packages.get(name) else {
            return Ok(false);
        };
        if !version_matches(requirement.version().unwrap_or("*"), &locked.version)? {
            return Ok(false);
        }
        validate_locked_dependencies(name, locked, lock.private.get(name))?;
    }
    Ok(true)
}

fn validate_locked_dependencies(
    root: &str,
    root_package: &LockedPackage,
    graph: Option<&BTreeMap<String, LockedPackage>>,
) -> Result<()> {
    let empty = BTreeMap::new();
    let graph = graph.unwrap_or(&empty);
    let mut visiting = BTreeSet::new();
    let mut complete = BTreeSet::new();
    validate_dependency_node(root, root_package, graph, &mut visiting, &mut complete)
}

fn validate_dependency_node(
    root: &str,
    package: &LockedPackage,
    graph: &BTreeMap<String, LockedPackage>,
    visiting: &mut BTreeSet<String>,
    complete: &mut BTreeSet<String>,
) -> Result<()> {
    for (dependency, requirement) in &package.dependencies {
        if complete.contains(dependency) {
            continue;
        }
        let locked = graph.get(dependency).with_context(|| {
            format!("locked root {root:?} is missing private dependency {dependency:?}")
        })?;
        require_version_match(dependency, requirement, &locked.version)?;
        if !visiting.insert(dependency.clone()) {
            bail!("locked dependency graph for root {root:?} contains a cycle at {dependency:?}");
        }
        validate_dependency_node(root, locked, graph, visiting, complete)?;
        visiting.remove(dependency);
        complete.insert(dependency.clone());
    }
    Ok(())
}

fn materialize_release(
    runtime: &Runtime,
    project_root: &Path,
    release: &ResolvedRegistryRelease,
) -> Result<BTreeMap<String, String>> {
    let plan = artifact_plan(
        project_root,
        &release.package,
        &release.version,
        &release.artifact_url,
        &release.artifact_sha256,
        release.artifact_size,
    )?;
    let stage = runtime
        .block_on(stage_artifact(&plan))
        .with_context(|| format!("failed to stage {}@{}", release.package, release.version))?;
    let dependencies = inspect_staged_archive(
        &stage.promotion.verified_partial,
        &release.package,
        &release.version,
        &release.manifest_sha256,
    )?;
    promote_artifact(&stage)
        .with_context(|| format!("failed to promote {}@{}", release.package, release.version))?;
    Ok(dependencies)
}

fn materialize_locked(
    runtime: &Runtime,
    project_root: &Path,
    package: &str,
    locked: &LockedPackage,
) -> Result<BTreeMap<String, String>> {
    let plan = artifact_plan(
        project_root,
        package,
        &locked.version,
        &locked.artifact_url,
        &locked.artifact_sha256,
        locked.artifact_size,
    )?;
    let stage = runtime
        .block_on(stage_artifact(&plan))
        .with_context(|| format!("failed to rehydrate locked {package}@{}", locked.version))?;
    let dependencies = inspect_staged_archive(
        &stage.promotion.verified_partial,
        package,
        &locked.version,
        &locked.manifest_sha256,
    )?;
    promote_artifact(&stage)
        .with_context(|| format!("failed to promote locked {package}@{}", locked.version))?;
    Ok(dependencies)
}

fn artifact_plan(
    project_root: &Path,
    package: &str,
    version: &str,
    artifact_url: &str,
    artifact_sha256: &str,
    artifact_size: u64,
) -> Result<ArtifactDownloadPlan> {
    let paths = ProjectPaths::new(project_root);
    let final_dir = paths.artifact_dir(artifact_sha256)?;
    let staging_dir = paths
        .library_root()
        .join(".staging")
        .join(artifact_sha256.to_ascii_lowercase());
    Ok(ArtifactDownloadPlan {
        package: package.to_string(),
        version: version.to_string(),
        source: Url::parse(artifact_url).context("invalid RPX artifact URL")?,
        expected_sha256: artifact_sha256.to_ascii_lowercase(),
        expected_size_bytes: Some(artifact_size),
        partial_path: staging_dir.join("artifact.rbe.part"),
        staging_dir,
        final_artifact_path: final_dir.join("artifact.rbe"),
        final_dir,
        limits: DownloadLimits::default(),
        resume: ResumePolicy::default(),
    })
}

fn inspect_staged_archive(
    path: &Path,
    expected_package: &str,
    expected_version: &str,
    expected_manifest_sha256: &str,
) -> Result<BTreeMap<String, String>> {
    let mut archive = ZipArchive::new(File::open(path)?)
        .with_context(|| format!("invalid RBE package archive {}", path.display()))?;

    let manifest_bytes = {
        let mut manifest = archive.by_name(ARCHIVE_PACKAGE_MANIFEST).with_context(|| {
            format!("RBE package archive is missing {ARCHIVE_PACKAGE_MANIFEST}")
        })?;
        if manifest.is_dir() || manifest.size() > MAX_PACKAGE_MANIFEST_BYTES {
            bail!("RBE package manifest is not a bounded regular file");
        }
        read_bounded(&mut manifest, MAX_PACKAGE_MANIFEST_BYTES)?
    };
    let observed_manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    if !observed_manifest_sha256.eq_ignore_ascii_case(expected_manifest_sha256) {
        bail!(
            "RBE package manifest hash mismatch for {expected_package}@{expected_version}: expected {expected_manifest_sha256}, observed {observed_manifest_sha256}"
        );
    }
    let manifest_text =
        std::str::from_utf8(&manifest_bytes).context("package.rbe.yaml is not valid UTF-8")?;
    let manifest: ArchivePackageManifest =
        serde_yaml::from_str(manifest_text).context("invalid package.rbe.yaml")?;
    if manifest.package.name != expected_package || manifest.package.version != expected_version {
        bail!(
            "RBE package archive identity mismatch: expected {expected_package}@{expected_version}, found {}@{}",
            manifest.package.name,
            manifest.package.version
        );
    }
    for (dependency, requirement) in &manifest.dependencies.rbe {
        validate_dependency_name(dependency)?;
        VersionReq::parse(requirement).with_context(|| {
            format!("invalid private dependency requirement {requirement:?} for {dependency:?}")
        })?;
    }

    let index_bytes = {
        let mut index = archive
            .by_name(ARCHIVE_PACKAGE_INDEX)
            .with_context(|| format!("RBE package archive is missing {ARCHIVE_PACKAGE_INDEX}"))?;
        if index.is_dir() || index.size() > MAX_PACKAGE_INDEX_BYTES {
            bail!("RBE package index is not a bounded regular file");
        }
        read_bounded(&mut index, MAX_PACKAGE_INDEX_BYTES)?
    };
    validate_package_index(&index_bytes, expected_package, expected_version)?;
    Ok(manifest.dependencies.rbe)
}

fn validate_package_index(
    bytes: &[u8],
    expected_package: &str,
    expected_version: &str,
) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("invalid .rbe/package-index.json")?;
    let format = value
        .get("format")
        .and_then(serde_json::Value::as_u64)
        .context("package index is missing a numeric format")?;
    if !matches!(format, LEGACY_PACKAGE_INDEX_FORMAT | PHASE3_PACKAGE_INDEX_FORMAT) {
        bail!("unsupported .rbe/package-index.json format {format}");
    }
    let package = value
        .get("package")
        .and_then(serde_json::Value::as_object)
        .context("package index is missing package identity")?;
    if package.get("name").and_then(serde_json::Value::as_str) != Some(expected_package)
        || package.get("version").and_then(serde_json::Value::as_str) != Some(expected_version)
    {
        bail!("package index identity does not match the registry release");
    }

    let exports = value
        .get("exports")
        .and_then(serde_json::Value::as_array)
        .context("package index is missing exports")?;
    let mut ids = BTreeSet::new();
    for export in exports {
        let export = export
            .as_object()
            .context("package index export must be an object")?;
        let name = export
            .get("name")
            .and_then(serde_json::Value::as_str)
            .context("package index export is missing name")?;
        let expected_id = canonical_export_id(expected_package, name)
            .with_context(|| format!("derive deterministic export ID for {name:?}"))?;
        match export.get("export_id").and_then(serde_json::Value::as_str) {
            Some(observed) => {
                if observed != expected_id {
                    bail!(
                        "package index export ID mismatch for {name:?}: expected {expected_id:?}, observed {observed:?}"
                    );
                }
                if !ids.insert(observed.to_string()) {
                    bail!("package index contains duplicate export ID {observed:?}");
                }
            }
            None if format == PHASE3_PACKAGE_INDEX_FORMAT => {
                bail!("format-2 package index export {name:?} is missing export_id")
            }
            None => {}
        }
    }
    Ok(())
}

fn read_bounded(reader: &mut impl Read, maximum: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!("bounded archive entry exceeded {maximum} bytes");
    }
    Ok(bytes)
}

fn version_matches(requirement: &str, version: &str) -> Result<bool> {
    let requirement = VersionReq::parse(requirement)?;
    let version = Version::parse(version)?;
    Ok(requirement.matches(&version))
}

fn require_version_match(package: &str, requirement: &str, version: &str) -> Result<()> {
    if !version_matches(requirement, version)? {
        bail!("locked package {package:?} version {version:?} does not satisfy {requirement:?}");
    }
    Ok(())
}

fn validate_dependency_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 192 {
        bail!("invalid RBE dependency name {value:?}");
    }
    for segment in value.split('.') {
        if segment.is_empty()
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("invalid RBE dependency name {value:?}");
        }
    }
    Ok(())
}

fn report(project_root: &Path, lock: &ProjectLock, reused_lock: bool) -> Result<InstallReport> {
    let private_packages = lock.private.values().map(BTreeMap::len).sum();
    Ok(InstallReport {
        project_root: project_root.to_path_buf(),
        lock_path: ProjectPaths::new(project_root).lock_path(),
        index_revision: lock.index_revision.clone(),
        roots: lock.packages.len(),
        private_packages,
        reused_lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_index_format_two_requires_verified_export_ids() {
        let good = br#"{
            "format": 2,
            "package": {"name":"mail","version":"1.0.0"},
            "exports": [{"name":"send","export_id":"lib_mail_send"}]
        }"#;
        validate_package_index(good, "mail", "1.0.0").unwrap();

        let missing = br#"{
            "format": 2,
            "package": {"name":"mail","version":"1.0.0"},
            "exports": [{"name":"send"}]
        }"#;
        assert!(validate_package_index(missing, "mail", "1.0.0").is_err());

        let forged = br#"{
            "format": 2,
            "package": {"name":"mail","version":"1.0.0"},
            "exports": [{"name":"send","export_id":"lib_evil_send"}]
        }"#;
        assert!(validate_package_index(forged, "mail", "1.0.0").is_err());
    }

    #[test]
    fn locked_graph_allows_same_private_name_per_root() {
        let dependency = LockedPackage {
            requested: "^1".into(),
            version: "1.4.0".into(),
            artifact_url: "https://registry.example/x.rbe.zip".into(),
            artifact_sha256: "a".repeat(64),
            manifest_sha256: "b".repeat(64),
            artifact_size: 10,
            dependencies: BTreeMap::new(),
        };
        let dependency_v2 = LockedPackage {
            requested: "^2".into(),
            version: "2.1.0".into(),
            ..dependency.clone()
        };
        let root = LockedPackage {
            requested: "*".into(),
            version: "1.0.0".into(),
            artifact_url: "https://registry.example/root.rbe.zip".into(),
            artifact_sha256: "c".repeat(64),
            manifest_sha256: "d".repeat(64),
            artifact_size: 20,
            dependencies: BTreeMap::from([("shared".into(), "^1".into())]),
        };
        let root2 = LockedPackage {
            dependencies: BTreeMap::from([("shared".into(), "^2".into())]),
            ..root.clone()
        };
        validate_locked_dependencies(
            "a",
            &root,
            Some(&BTreeMap::from([("shared".into(), dependency)])),
        )
        .unwrap();
        validate_locked_dependencies(
            "b",
            &root2,
            Some(&BTreeMap::from([("shared".into(), dependency_v2)])),
        )
        .unwrap();
    }
}
