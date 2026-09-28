//! Cache-aware install planning for application-level `package.rbe.json`.
//!
//! This layer deliberately performs no network I/O and no package activation.
//! It decides which roots can reuse an exact locked artifact, which locked
//! artifacts need rehydration, and which roots require registry/index work.
//! The network/activation executor can consume this plan without duplicating
//! manifest and cache decision logic.

use crate::project::{
    LocalIndex, ProjectLock, ProjectManifest, ProjectPackageRequirement, ProjectPaths,
};
use anyhow::{Context, Result};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlanAction {
    UseCachedLocked {
        package: String,
        version: String,
        artifact_sha256: String,
        artifact_path: PathBuf,
    },
    RehydrateLocked {
        package: String,
        version: String,
        artifact_url: String,
        artifact_sha256: String,
        destination: PathBuf,
    },
    ResolveExactMetadata {
        package: String,
        requirement: String,
        version: String,
    },
    RefreshIndex {
        package: String,
        requirement: String,
    },
    ResolveExternalSource {
        package: String,
        requirement: Option<String>,
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationInstallPlan {
    pub project_root: PathBuf,
    pub manifest_sha256: String,
    pub reusable_lock: bool,
    pub index_revision: Option<String>,
    pub actions: Vec<InstallPlanAction>,
}

impl ApplicationInstallPlan {
    pub fn from_project(project_root: impl AsRef<Path>) -> Result<Self> {
        let project_root = project_root.as_ref();
        let manifest = ProjectManifest::load(project_root)?;
        let lock = ProjectLock::load(project_root)?;
        let index = LocalIndex::load(project_root)?;
        Self::build(project_root, &manifest, lock.as_ref(), index.as_ref())
    }

    pub fn build(
        project_root: impl AsRef<Path>,
        manifest: &ProjectManifest,
        lock: Option<&ProjectLock>,
        index: Option<&LocalIndex>,
    ) -> Result<Self> {
        manifest.validate()?;
        if let Some(lock) = lock {
            lock.validate()?;
        }
        if let Some(index) = index {
            index.validate()?;
        }

        let project_root = project_root.as_ref().to_path_buf();
        let paths = ProjectPaths::new(&project_root);
        let manifest_sha256 = manifest.sha256()?;
        let reusable_lock = match lock {
            Some(lock) => lock.matches_manifest(manifest)?,
            None => false,
        };
        let mut actions = Vec::with_capacity(manifest.packages.len());

        for (package, requirement) in &manifest.packages {
            if reusable_lock {
                if let Some(locked) = lock.and_then(|lock| lock.packages.get(package)) {
                    if requirement.source().is_none()
                        && locked_version_satisfies(requirement, &locked.version)?
                    {
                        let artifact_path = paths.artifact_path(&locked.artifact_sha256)?;
                        if artifact_matches(&artifact_path, &locked.artifact_sha256)? {
                            actions.push(InstallPlanAction::UseCachedLocked {
                                package: package.clone(),
                                version: locked.version.clone(),
                                artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
                                artifact_path,
                            });
                        } else {
                            actions.push(InstallPlanAction::RehydrateLocked {
                                package: package.clone(),
                                version: locked.version.clone(),
                                artifact_url: locked.artifact_url.clone(),
                                artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
                                destination: artifact_path,
                            });
                        }
                        continue;
                    }
                }
            }

            if let Some(source) = requirement.source() {
                actions.push(InstallPlanAction::ResolveExternalSource {
                    package: package.clone(),
                    requirement: requirement.version().map(ToOwned::to_owned),
                    source: source.to_string(),
                });
                continue;
            }

            let requirement_text = requirement.version().unwrap_or("*").to_string();
            let resolved = index
                .map(|index| index.resolve(package, &requirement_text))
                .transpose()?
                .flatten();
            if let Some(version) = resolved {
                actions.push(InstallPlanAction::ResolveExactMetadata {
                    package: package.clone(),
                    requirement: requirement_text,
                    version,
                });
            } else {
                actions.push(InstallPlanAction::RefreshIndex {
                    package: package.clone(),
                    requirement: requirement_text,
                });
            }
        }

        Ok(Self {
            project_root,
            manifest_sha256,
            reusable_lock,
            index_revision: index.map(|index| index.revision.clone()),
            actions,
        })
    }

    pub fn requires_network(&self) -> bool {
        self.actions.iter().any(|action| {
            !matches!(action, InstallPlanAction::UseCachedLocked { .. })
        })
    }

    pub fn uses_only_verified_cache(&self) -> bool {
        !self.actions.is_empty()
            && self
                .actions
                .iter()
                .all(|action| matches!(action, InstallPlanAction::UseCachedLocked { .. }))
    }

    pub fn is_empty_project(&self) -> bool {
        self.actions.is_empty()
    }
}

fn locked_version_satisfies(
    requirement: &ProjectPackageRequirement,
    locked_version: &str,
) -> Result<bool> {
    let Some(requirement) = requirement.version() else {
        return Ok(true);
    };
    let requirement = VersionReq::parse(requirement)
        .with_context(|| format!("invalid package requirement {requirement:?}"))?;
    let locked_version = Version::parse(locked_version)
        .with_context(|| format!("invalid locked package version {locked_version:?}"))?;
    Ok(requirement.matches(&locked_version))
}

fn artifact_matches(path: &Path, expected_sha256: &str) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect cached artifact {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }

    let mut file = fs::File::open(path)
        .with_context(|| format!("open cached artifact {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let observed = format!("{:x}", hasher.finalize());
    Ok(observed.eq_ignore_ascii_case(expected_sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{LockedPackage, ProjectLock};
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rpx-install-plan-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn manifest() -> ProjectManifest {
        ProjectManifest::parse(
            r#"{"packages":{"advancenet":"^1.4.0"},"scripts":{}}"#,
        )
        .unwrap()
    }

    fn locked(manifest: &ProjectManifest, artifact_sha256: String) -> ProjectLock {
        ProjectLock {
            format: 1,
            manifest_sha256: manifest.sha256().unwrap(),
            index_revision: Some("rev-12".into()),
            packages: BTreeMap::from([(
                "advancenet".into(),
                LockedPackage {
                    requested: "^1.4.0".into(),
                    version: "1.5.0".into(),
                    artifact_url: "https://registry.example/artifacts/advancenet.rbe.zip".into(),
                    artifact_sha256,
                    manifest_sha256: "b".repeat(64),
                    dependencies: BTreeMap::new(),
                },
            )]),
        }
    }

    #[test]
    fn unchanged_lock_reuses_only_hash_verified_artifact() {
        let root = temp_root("cache-hit");
        let manifest = manifest();
        let bytes = b"verified package bytes";
        let sha = format!("{:x}", Sha256::digest(bytes));
        let lock = locked(&manifest, sha.clone());
        let path = ProjectPaths::new(&root).artifact_path(&sha).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();

        let plan = ApplicationInstallPlan::build(&root, &manifest, Some(&lock), None).unwrap();
        assert!(plan.reusable_lock);
        assert!(plan.uses_only_verified_cache());
        assert_eq!(
            plan.actions,
            vec![InstallPlanAction::UseCachedLocked {
                package: "advancenet".into(),
                version: "1.5.0".into(),
                artifact_sha256: sha,
                artifact_path: path,
            }]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupted_locked_artifact_is_rehydrated_instead_of_trusted() {
        let root = temp_root("cache-corrupt");
        let manifest = manifest();
        let sha = "a".repeat(64);
        let lock = locked(&manifest, sha.clone());
        let path = ProjectPaths::new(&root).artifact_path(&sha).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"wrong bytes").unwrap();

        let plan = ApplicationInstallPlan::build(&root, &manifest, Some(&lock), None).unwrap();
        assert!(matches!(
            &plan.actions[0],
            InstallPlanAction::RehydrateLocked { artifact_sha256, .. }
                if artifact_sha256 == &sha
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_lock_uses_local_index_to_select_exact_stable_release() {
        let root = temp_root("resolve");
        let manifest = manifest();
        let mut stale = locked(&manifest, "a".repeat(64));
        stale.manifest_sha256 = "c".repeat(64);
        let index = LocalIndex::parse(
            r#"{
  "format": 1,
  "revision": "rev-13",
  "packages": {
    "advancenet": {
      "latest": "1.5.0",
      "versions": ["1.4.0", "1.4.8", "1.5.0", "1.6.0-beta.1"]
    }
  }
}"#,
        )
        .unwrap();

        let plan =
            ApplicationInstallPlan::build(&root, &manifest, Some(&stale), Some(&index)).unwrap();
        assert!(!plan.reusable_lock);
        assert_eq!(plan.index_revision.as_deref(), Some("rev-13"));
        assert_eq!(
            plan.actions,
            vec![InstallPlanAction::ResolveExactMetadata {
                package: "advancenet".into(),
                requirement: "^1.4.0".into(),
                version: "1.5.0".into(),
            }]
        );
    }

    #[test]
    fn absent_index_requests_refresh_instead_of_guessing_latest() {
        let root = temp_root("refresh");
        let manifest = manifest();
        let plan = ApplicationInstallPlan::build(&root, &manifest, None, None).unwrap();
        assert_eq!(
            plan.actions,
            vec![InstallPlanAction::RefreshIndex {
                package: "advancenet".into(),
                requirement: "^1.4.0".into(),
            }]
        );
        assert!(plan.requires_network());
    }

    #[test]
    fn empty_application_needs_no_registry_work() {
        let root = temp_root("empty");
        let manifest = ProjectManifest::parse(r#"{"packages":{},"scripts":{}}"#).unwrap();
        let plan = ApplicationInstallPlan::build(&root, &manifest, None, None).unwrap();
        assert!(plan.is_empty_project());
        assert!(!plan.requires_network());
    }
}
