use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rbe_install_request::RegistryPackageIndex;
use rbe_library_resolver::Resolution;
use rbe_project_package::{ProjectPackageLock, ProjectCacheLayout};

use crate::{stage_registry_package, InstallRuntimeError, VerifiedRegistryPackage};

#[derive(Debug)]
pub struct VerifiedRootGraph {
    pub root: String,
    pub install_order: Vec<String>,
    pub lock: ProjectPackageLock,
    pub packages: BTreeMap<String, VerifiedRegistryPackage>,
}

/// Execute the registry-artifact trust boundary for one resolver root graph.
///
/// Packages are staged in the resolver's dependency-first install order. The
/// resulting lock candidate keeps the requested root public and every other
/// selected package private to that root. No cache promotion or project-lock
/// activation happens here.
pub async fn stage_resolved_root(
    project_root: &Path,
    root: &str,
    resolution: &Resolution,
    indexes: &BTreeMap<String, RegistryPackageIndex>,
) -> Result<VerifiedRootGraph, InstallRuntimeError> {
    if resolution.release(root).is_none() {
        return Err(InstallRuntimeError::ResolutionPackageMissing {
            root: root.to_string(),
            package: root.to_string(),
        });
    }

    let layout = ProjectCacheLayout::new(project_root);
    let mut seen = BTreeSet::new();
    let mut lock = ProjectPackageLock::default();
    let mut packages = BTreeMap::new();

    for package in &resolution.install_order {
        if !seen.insert(package.clone()) {
            return Err(InstallRuntimeError::ResolutionOrderDuplicate {
                root: root.to_string(),
                package: package.clone(),
            });
        }
        let selected = resolution.release(package).ok_or_else(|| {
            InstallRuntimeError::ResolutionPackageMissing {
                root: root.to_string(),
                package: package.clone(),
            }
        })?;
        let selected_version = selected.version.to_string();
        let index = indexes
            .get(package)
            .ok_or_else(|| InstallRuntimeError::RegistryIndexMissing {
                package: package.clone(),
            })?;
        index.validate_for(package)?;
        let release = index
            .releases
            .iter()
            .find(|release| release.version == selected_version)
            .ok_or_else(|| InstallRuntimeError::RegistryReleaseMissing {
                package: package.clone(),
                version: selected_version.clone(),
            })?;

        let verified = stage_registry_package(&layout, package, release).await?;
        let locked = verified.locked.clone();
        if package == root {
            if lock.packages.insert(package.clone(), locked).is_some() {
                return Err(InstallRuntimeError::DuplicateVerifiedPackage {
                    root: root.to_string(),
                    package: package.clone(),
                });
            }
        } else if lock
            .private
            .entry(root.to_string())
            .or_default()
            .insert(package.clone(), locked)
            .is_some()
        {
            return Err(InstallRuntimeError::DuplicateVerifiedPackage {
                root: root.to_string(),
                package: package.clone(),
            });
        }
        if packages.insert(package.clone(), verified).is_some() {
            return Err(InstallRuntimeError::DuplicateVerifiedPackage {
                root: root.to_string(),
                package: package.clone(),
            });
        }
    }

    for package in resolution.selected.keys() {
        if !seen.contains(package) {
            return Err(InstallRuntimeError::ResolutionOrderIncomplete {
                root: root.to_string(),
                package: package.clone(),
            });
        }
    }
    if !lock.packages.contains_key(root) {
        return Err(InstallRuntimeError::VerifiedRootMissing(root.to_string()));
    }

    lock.validate()?;
    if !lock.root_graph_complete(root) {
        return Err(InstallRuntimeError::VerifiedRootGraphIncomplete(
            root.to_string(),
        ));
    }

    Ok(VerifiedRootGraph {
        root: root.to_string(),
        install_order: resolution.install_order.clone(),
        lock,
        packages,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use rbe_install_request::{
        RegistryArtifact, RegistryPackageRelease, REGISTRY_PACKAGE_INDEX_FORMAT,
    };
    use rbe_library_resolver::PackageRelease;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::registry_artifact_plan;

    fn manifest(name: &str, version: &str, dependencies: &BTreeMap<String, String>) -> String {
        let dependency_lines = dependencies
            .iter()
            .map(|(name, requirement)| format!("{name} = {requirement:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            r#"name = {name:?}
version = {version:?}
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

[dependencies]
{dependency_lines}
"#
        )
    }

    fn package_bytes(manifest: &str) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file(
                    rbe_library_package::LIBRARY_MANIFEST,
                    SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(manifest.as_bytes()).unwrap();
            writer
                .start_file("src/index.js", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"export default 1;").unwrap();
            writer.finish().unwrap();
        }
        bytes.into_inner()
    }

    fn registry_release(
        version: &str,
        dependencies: BTreeMap<String, String>,
        bytes: &[u8],
    ) -> RegistryPackageRelease {
        RegistryPackageRelease {
            version: version.into(),
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            yanked: false,
            dependencies,
            artifact: RegistryArtifact {
                source: format!("https://example.com/{version}.rbe"),
                sha256: format!("{:x}", Sha256::digest(bytes)),
                size_bytes: bytes.len() as u64,
                source_sha256: None,
                publisher: None,
                signature: None,
                reproducible_build: false,
                shipped_binary_sha256: None,
            },
        }
    }

    fn index(package: &str, release: RegistryPackageRelease) -> RegistryPackageIndex {
        RegistryPackageIndex {
            format: REGISTRY_PACKAGE_INDEX_FORMAT,
            package: package.into(),
            releases: vec![release],
        }
    }

    fn seed_verified_partial(
        layout: &ProjectCacheLayout,
        package: &str,
        release: &RegistryPackageRelease,
        bytes: &[u8],
    ) {
        let plan = registry_artifact_plan(layout, package, release).unwrap();
        std::fs::create_dir_all(&plan.staging_dir).unwrap();
        std::fs::write(plan.partial_path, bytes).unwrap();
    }

    #[tokio::test]
    async fn verified_graph_keeps_transitives_private_to_root() {
        let dependency_map = BTreeMap::from([("rbe-core".into(), "^1".into())]);
        let root_manifest = manifest("advancenet", "4.0.1", &dependency_map);
        let dep_manifest = manifest("rbe-core", "1.2.0", &BTreeMap::new());
        let root_bytes = package_bytes(&root_manifest);
        let dep_bytes = package_bytes(&dep_manifest);
        let root_registry = registry_release("4.0.1", dependency_map.clone(), &root_bytes);
        let dep_registry = registry_release("1.2.0", BTreeMap::new(), &dep_bytes);

        let resolution = Resolution {
            selected: BTreeMap::from([
                (
                    "advancenet".into(),
                    PackageRelease::new(
                        "advancenet",
                        "4.0.1",
                        1,
                        1,
                        false,
                        dependency_map,
                    )
                    .unwrap(),
                ),
                (
                    "rbe-core".into(),
                    PackageRelease::new(
                        "rbe-core",
                        "1.2.0",
                        1,
                        1,
                        false,
                        BTreeMap::new(),
                    )
                    .unwrap(),
                ),
            ]),
            install_order: vec!["rbe-core".into(), "advancenet".into()],
        };
        let indexes = BTreeMap::from([
            ("advancenet".into(), index("advancenet", root_registry.clone())),
            ("rbe-core".into(), index("rbe-core", dep_registry.clone())),
        ]);
        let temp = tempdir().unwrap();
        let layout = ProjectCacheLayout::new(temp.path());
        seed_verified_partial(&layout, "advancenet", &root_registry, &root_bytes);
        seed_verified_partial(&layout, "rbe-core", &dep_registry, &dep_bytes);

        let graph = stage_resolved_root(temp.path(), "advancenet", &resolution, &indexes)
            .await
            .unwrap();

        assert_eq!(
            graph.install_order,
            vec!["rbe-core".to_string(), "advancenet".to_string()]
        );
        assert!(graph.lock.packages.contains_key("advancenet"));
        assert!(!graph.lock.packages.contains_key("rbe-core"));
        assert!(graph
            .lock
            .private
            .get("advancenet")
            .unwrap()
            .contains_key("rbe-core"));
        assert!(graph.lock.root_graph_complete("advancenet"));
        assert_eq!(graph.packages.len(), 2);
    }

    #[tokio::test]
    async fn incomplete_install_order_is_rejected_before_activation() {
        let root_release = PackageRelease::new(
            "advancenet",
            "4.0.1",
            1,
            1,
            false,
            BTreeMap::from([("rbe-core".into(), "^1".into())]),
        )
        .unwrap();
        let dependency = PackageRelease::new(
            "rbe-core",
            "1.2.0",
            1,
            1,
            false,
            BTreeMap::new(),
        )
        .unwrap();
        let resolution = Resolution {
            selected: BTreeMap::from([
                ("advancenet".into(), root_release),
                ("rbe-core".into(), dependency),
            ]),
            install_order: vec!["advancenet".into()],
        };

        let error = stage_resolved_root(tempdir().unwrap().path(), "advancenet", &resolution, &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InstallRuntimeError::RegistryIndexMissing { package }
                if package == "advancenet"
        ));
    }
}
