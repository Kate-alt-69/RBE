use std::fs;
use std::path::{Path, PathBuf};

use rbe_project_package::{
    PackageRequirement, ProjectCacheLayout, ProjectPackageError, ProjectPackageLock,
    ProjectPackageManifest,
};
use sha2::{Digest, Sha256};

use crate::VerifiedRootGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedInstallTarget {
    pub manifest: ProjectPackageManifest,
    pub lock: ProjectPackageLock,
    pub manifest_yaml: String,
    pub lock_yaml: String,
    pub manifest_sha256: String,
    pub lock_sha256: String,
}

/// Build the exact project manifest + lock target for one named root install.
///
/// The verified resolver graph only contains the requested root. This merge
/// preserves every other active project root, replaces only the requested
/// root/private graph, and removes stale lock roots that are no longer declared
/// by the human project manifest. No filesystem mutation happens here.
pub fn merge_named_install_target(
    current_manifest: ProjectPackageManifest,
    current_lock: ProjectPackageLock,
    requested_root: &str,
    requested_requirement: &str,
    graph: &VerifiedRootGraph,
) -> Result<NamedInstallTarget, InstallTargetError> {
    if graph.root != requested_root {
        return Err(InstallTargetError::RootMismatch {
            requested: requested_root.to_string(),
            graph: graph.root.clone(),
        });
    }
    if graph.lock.packages.len() != 1 || !graph.lock.packages.contains_key(requested_root) {
        return Err(InstallTargetError::InvalidVerifiedGraph(
            requested_root.to_string(),
        ));
    }
    if !graph.lock.root_graph_complete(requested_root) {
        return Err(InstallTargetError::IncompleteVerifiedGraph(
            requested_root.to_string(),
        ));
    }

    current_manifest.validate()?;
    current_lock.validate()?;

    let selected = graph
        .lock
        .packages
        .get(requested_root)
        .ok_or_else(|| InstallTargetError::MissingVerifiedRoot(requested_root.to_string()))?;
    let requirement = if requested_requirement.trim().is_empty() || requested_requirement == "*" {
        selected.version.clone()
    } else {
        requested_requirement.to_string()
    };

    let mut manifest = current_manifest;
    manifest.packages.insert(
        requested_root.to_string(),
        PackageRequirement::Version(requirement),
    );
    manifest.validate()?;

    let mut lock = current_lock;
    lock.packages
        .insert(requested_root.to_string(), selected.clone());
    lock.private.remove(requested_root);
    if let Some(private) = graph.lock.private.get(requested_root) {
        if !private.is_empty() {
            lock.private
                .insert(requested_root.to_string(), private.clone());
        }
    }

    // Lock roots absent from the human manifest are stale activation state and
    // must not survive a newly committed target.
    lock.packages
        .retain(|root, _| manifest.packages.contains_key(root));
    lock.private
        .retain(|root, _| manifest.packages.contains_key(root));

    for root in manifest.packages.keys() {
        if !lock.packages.contains_key(root) {
            return Err(InstallTargetError::UnresolvedExistingRoot(root.clone()));
        }
        if !lock.root_graph_complete(root) {
            return Err(InstallTargetError::IncompleteExistingRoot(root.clone()));
        }
    }
    lock.validate()?;

    let manifest_yaml = manifest.render_yaml()?;
    let lock_yaml = lock.render_yaml()?;
    let manifest_sha256 = sha256_text(&manifest_yaml);
    let lock_sha256 = sha256_text(&lock_yaml);

    Ok(NamedInstallTarget {
        manifest,
        lock,
        manifest_yaml,
        lock_yaml,
        manifest_sha256,
        lock_sha256,
    })
}

/// Load the current project package state and build the non-mutating target.
pub fn load_named_install_target(
    project_root: &Path,
    requested_root: &str,
    requested_requirement: &str,
    graph: &VerifiedRootGraph,
) -> Result<NamedInstallTarget, InstallTargetError> {
    let layout = ProjectCacheLayout::new(project_root);
    let manifest = read_optional_manifest(&layout.manifest_path())?.unwrap_or_default();
    let lock = read_optional_lock(&layout.lock_path())?.unwrap_or_default();
    merge_named_install_target(
        manifest,
        lock,
        requested_root,
        requested_requirement,
        graph,
    )
}

fn read_optional_manifest(
    path: &Path,
) -> Result<Option<ProjectPackageManifest>, InstallTargetError> {
    read_optional_text(path)?
        .map(|text| ProjectPackageManifest::parse_yaml(&text))
        .transpose()
        .map_err(Into::into)
}

fn read_optional_lock(path: &Path) -> Result<Option<ProjectPackageLock>, InstallTargetError> {
    read_optional_text(path)?
        .map(|text| ProjectPackageLock::parse_yaml(&text))
        .transpose()
        .map_err(Into::into)
}

fn read_optional_text(path: &Path) -> Result<Option<String>, InstallTargetError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(InstallTargetError::UnsafeProjectState(
                    path.display().to_string(),
                ));
            }
            Ok(Some(fs::read_to_string(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Debug, thiserror::Error)]
pub enum InstallTargetError {
    #[error(transparent)]
    ProjectPackage(#[from] ProjectPackageError),
    #[error("requested install root {requested:?} does not match verified graph root {graph:?}")]
    RootMismatch { requested: String, graph: String },
    #[error("verified graph for {0:?} must contain exactly one public root")]
    InvalidVerifiedGraph(String),
    #[error("verified graph is missing requested root {0:?}")]
    MissingVerifiedRoot(String),
    #[error("verified graph for requested root {0:?} is incomplete")]
    IncompleteVerifiedGraph(String),
    #[error("existing project root {0:?} has no locked graph; resolve the full project before activation")]
    UnresolvedExistingRoot(String),
    #[error("existing project root {0:?} has an incomplete private dependency graph")]
    IncompleteExistingRoot(String),
    #[error("project package state path is not a regular file: {0}")]
    UnsafeProjectState(String),
    #[error("read project package state: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rbe_project_package::LockedProjectPackage;

    use super::*;
    use crate::VerifiedRootGraph;

    fn locked(version: &str, dependency: Option<(&str, &str)>) -> LockedProjectPackage {
        LockedProjectPackage {
            version: version.into(),
            resolved_from: "registry:test".into(),
            artifact_url: "https://example.com/test.rbe".into(),
            artifact_sha256: "a".repeat(64),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            dependencies: dependency
                .map(|(name, requirement)| {
                    BTreeMap::from([(name.to_string(), requirement.to_string())])
                })
                .unwrap_or_default(),
            runtime: None,
            sdk: None,
        }
    }

    fn graph(root: &str, version: &str) -> VerifiedRootGraph {
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert(root.into(), locked(version, None));
        VerifiedRootGraph {
            root: root.into(),
            install_order: vec![root.into()],
            lock,
            packages: BTreeMap::new(),
        }
    }

    #[test]
    fn merge_preserves_other_roots_and_replaces_only_requested_root() {
        let mut manifest = ProjectPackageManifest::default();
        manifest
            .packages
            .insert("alpha".into(), PackageRequirement::Version("1.0.0".into()));
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert("alpha".into(), locked("1.0.0", None));

        let target = merge_named_install_target(
            manifest,
            lock,
            "beta",
            "^2",
            &graph("beta", "2.4.0"),
        )
        .unwrap();

        assert_eq!(target.lock.packages.len(), 2);
        assert_eq!(target.lock.packages["alpha"].version, "1.0.0");
        assert_eq!(target.lock.packages["beta"].version, "2.4.0");
        assert_eq!(target.manifest.packages["beta"].version(), Some("^2"));
    }

    #[test]
    fn wildcard_install_records_exact_selected_version() {
        let target = merge_named_install_target(
            ProjectPackageManifest::default(),
            ProjectPackageLock::default(),
            "beta",
            "*",
            &graph("beta", "2.4.0"),
        )
        .unwrap();
        assert_eq!(target.manifest.packages["beta"].version(), Some("2.4.0"));
    }

    #[test]
    fn unresolved_existing_manifest_root_blocks_partial_activation() {
        let mut manifest = ProjectPackageManifest::default();
        manifest
            .packages
            .insert("alpha".into(), PackageRequirement::Version("1.0.0".into()));
        let error = merge_named_install_target(
            manifest,
            ProjectPackageLock::default(),
            "beta",
            "2.0.0",
            &graph("beta", "2.0.0"),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            InstallTargetError::UnresolvedExistingRoot(root) if root == "alpha"
        ));
    }

    #[test]
    fn stale_lock_roots_not_declared_by_manifest_are_removed() {
        let manifest = ProjectPackageManifest::default();
        let mut lock = ProjectPackageLock::default();
        lock.packages.insert("stale".into(), locked("9.0.0", None));
        let target = merge_named_install_target(
            manifest,
            lock,
            "beta",
            "2.0.0",
            &graph("beta", "2.0.0"),
        )
        .unwrap();
        assert!(!target.lock.packages.contains_key("stale"));
        assert!(target.lock.packages.contains_key("beta"));
    }
}
