//! Source-only adapter between the public registry wire contract and the
//! deterministic library resolver.
//!
//! The adapter performs no network I/O. `rbe-install-request` owns wire-format
//! and artifact validation; `rbe-library-resolver` owns dependency solving.

#![forbid(unsafe_code)]

use rbe_install_request::{RegistryContractError, RegistryPackageIndex, RegistryPackageRelease};
use rbe_library_resolver::{PackageCatalog, PackageRelease, ResolveError};

/// Add one already-fetched registry package index to a resolver catalog.
///
/// The response identity is validated against `expected_package` before any
/// release metadata reaches the solver. Artifact pins remain in the validated
/// registry object and are intentionally not copied into resolver state.
///
/// Catalog mutation is transactional: either every release is accepted by the
/// resolver or the caller's existing catalog is left unchanged.
pub fn add_registry_index(
    catalog: &mut PackageCatalog,
    expected_package: &str,
    index: &RegistryPackageIndex,
) -> Result<(), RegistryBridgeError> {
    index.validate_for(expected_package)?;

    let mut staged = catalog.clone();
    for release in &index.releases {
        staged.add(to_resolver_release(expected_package, release)?)?;
    }

    *catalog = staged;
    Ok(())
}

/// Build a deterministic resolver catalog from validated registry indexes.
pub fn catalog_from_registry_indexes<'a>(
    indexes: impl IntoIterator<Item = (&'a str, &'a RegistryPackageIndex)>,
) -> Result<PackageCatalog, RegistryBridgeError> {
    let mut catalog = PackageCatalog::default();
    for (expected_package, index) in indexes {
        add_registry_index(&mut catalog, expected_package, index)?;
    }
    Ok(catalog)
}

fn to_resolver_release(
    package: &str,
    release: &RegistryPackageRelease,
) -> Result<PackageRelease, ResolveError> {
    PackageRelease::new(
        package,
        &release.version,
        release.rbe_abi_min,
        release.rbe_abi_max,
        release.yanked,
        release.dependencies.clone(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryBridgeError {
    #[error(transparent)]
    Contract(#[from] RegistryContractError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_install_request::RegistryPackageIndex;
    use rbe_library_resolver::{resolve, ResolutionRequest};

    fn index_json(package: &str, version: &str, dependencies: &str) -> String {
        format!(
            r#"{{
  "format": 1,
  "package": "{package}",
  "releases": [{{
    "version": "{version}",
    "rbe_abi_min": 1,
    "rbe_abi_max": 1,
    "dependencies": {dependencies},
    "artifact": {{
      "source": "https://cdn.kastrick.invalid/{package}-{version}.rbe-pkg",
      "sha256": "{}",
      "size_bytes": 1024
    }}
  }}]
}}"#,
            "a".repeat(64)
        )
    }

    #[test]
    fn validated_registry_metadata_feeds_the_deterministic_solver() {
        let transport =
            RegistryPackageIndex::parse_json(&index_json("transport", "2.1.0", "{}"), "transport")
                .unwrap();
        let advancenet = RegistryPackageIndex::parse_json(
            &index_json("advancenet", "4.0.1", r#"{"transport":"^2"}"#),
            "advancenet",
        )
        .unwrap();

        let catalog =
            catalog_from_registry_indexes([("transport", &transport), ("advancenet", &advancenet)])
                .unwrap();
        let resolution = resolve(
            &catalog,
            [ResolutionRequest::new("advancenet", "=4.0.1").unwrap()],
        )
        .unwrap();

        assert_eq!(
            resolution
                .release("advancenet")
                .unwrap()
                .version
                .to_string(),
            "4.0.1"
        );
        assert_eq!(
            resolution.release("transport").unwrap().version.to_string(),
            "2.1.0"
        );
        assert_eq!(
            resolution.install_order,
            vec!["transport".to_string(), "advancenet".to_string()]
        );
    }

    #[test]
    fn registry_response_cannot_swap_requested_package_identity() {
        let index = RegistryPackageIndex::parse_json(
            &index_json("advancenet", "4.0.1", "{}"),
            "advancenet",
        )
        .unwrap();
        let error = catalog_from_registry_indexes([("different", &index)]).unwrap_err();
        assert!(matches!(
            error,
            RegistryBridgeError::Contract(RegistryContractError::PackageMismatch { .. })
        ));
    }

    #[test]
    fn semver_validation_still_belongs_to_the_resolver() {
        let index = RegistryPackageIndex::parse_json(
            &index_json("advancenet", "not-semver", "{}"),
            "advancenet",
        )
        .unwrap();
        let error = catalog_from_registry_indexes([("advancenet", &index)]).unwrap_err();
        assert!(matches!(
            error,
            RegistryBridgeError::Resolve(ResolveError::InvalidVersion { .. })
        ));
    }

    #[test]
    fn failed_registry_ingestion_does_not_partially_mutate_catalog() {
        let mut index = RegistryPackageIndex::parse_json(
            &index_json("advancenet", "4.0.1", "{}"),
            "advancenet",
        )
        .unwrap();
        let invalid = RegistryPackageIndex::parse_json(
            &index_json("advancenet", "not-semver", "{}"),
            "advancenet",
        )
        .unwrap();
        index.releases.extend(invalid.releases);

        let mut catalog = PackageCatalog::default();
        let before = catalog.clone();
        let error = add_registry_index(&mut catalog, "advancenet", &index).unwrap_err();

        assert!(matches!(
            error,
            RegistryBridgeError::Resolve(ResolveError::InvalidVersion { .. })
        ));
        assert_eq!(catalog, before);
    }
}
