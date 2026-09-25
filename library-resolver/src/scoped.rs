use std::collections::BTreeMap;

use crate::{resolve_for_abi, PackageCatalog, Resolution, ResolutionRequest, ResolveError};
use rbe_sdk::LIBRARY_ABI_VERSION;

/// A dependency graph per explicitly requested root package.
///
/// Each root is solved independently, so two public/root packages may use
/// different versions of the same transitive RBE package. Inside one root graph,
/// ordinary semver constraint unification and backtracking still apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedResolution {
    pub roots: BTreeMap<String, Resolution>,
}

impl ScopedResolution {
    pub fn root(&self, name: &str) -> Option<&Resolution> {
        self.roots.get(name)
    }

    pub fn root_count(&self) -> usize {
        self.roots.len()
    }

    pub fn package_instance_count(&self) -> usize {
        self.roots
            .values()
            .map(|resolution| resolution.selected.len())
            .sum()
    }
}

pub fn resolve_scoped(
    catalog: &PackageCatalog,
    requests: impl IntoIterator<Item = ResolutionRequest>,
) -> Result<ScopedResolution, ScopedResolveError> {
    resolve_scoped_for_abi(catalog, requests, LIBRARY_ABI_VERSION)
}

pub fn resolve_scoped_for_abi(
    catalog: &PackageCatalog,
    requests: impl IntoIterator<Item = ResolutionRequest>,
    host_abi: u32,
) -> Result<ScopedResolution, ScopedResolveError> {
    let requests = requests.into_iter().collect::<Vec<_>>();
    if requests.is_empty() {
        return Err(ScopedResolveError::Resolve(ResolveError::NoRoots));
    }

    let mut roots = BTreeMap::new();
    for request in requests {
        let root = request.name.clone();
        if roots.contains_key(&root) {
            return Err(ScopedResolveError::DuplicateRoot(root));
        }
        let resolution = resolve_for_abi(catalog, [request], host_abi)?;
        roots.insert(root, resolution);
    }

    Ok(ScopedResolution { roots })
}

#[derive(Debug, thiserror::Error)]
pub enum ScopedResolveError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error("root package {0:?} was requested more than once")]
    DuplicateRoot(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PackageRelease;

    fn deps(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(name, requirement)| ((*name).to_string(), (*requirement).to_string()))
            .collect()
    }

    fn release(name: &str, version: &str, dependencies: &[(&str, &str)]) -> PackageRelease {
        PackageRelease::new(name, version, 1, 1, false, deps(dependencies)).unwrap()
    }

    #[test]
    fn roots_can_select_conflicting_private_dependency_versions() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(release(
                "advancenet",
                "2.0.0",
                &[("rbe-compiler-syntax", "^1.5")],
            ))
            .unwrap();
        catalog
            .add(release(
                "awesome-http",
                "1.0.0",
                &[("rbe-compiler-syntax", "~1.3")],
            ))
            .unwrap();
        catalog
            .add(release("rbe-compiler-syntax", "1.5.2", &[]))
            .unwrap();
        catalog
            .add(release("rbe-compiler-syntax", "1.3.9", &[]))
            .unwrap();

        let result = resolve_scoped(
            &catalog,
            [
                ResolutionRequest::new("advancenet", "^2").unwrap(),
                ResolutionRequest::new("awesome-http", "^1").unwrap(),
            ],
        )
        .unwrap();

        assert_eq!(result.root_count(), 2);
        assert_eq!(result.package_instance_count(), 4);
        assert_eq!(
            result
                .root("advancenet")
                .unwrap()
                .release("rbe-compiler-syntax")
                .unwrap()
                .version
                .to_string(),
            "1.5.2"
        );
        assert_eq!(
            result
                .root("awesome-http")
                .unwrap()
                .release("rbe-compiler-syntax")
                .unwrap()
                .version
                .to_string(),
            "1.3.9"
        );
    }

    #[test]
    fn constraints_still_unify_inside_one_root_graph() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(release("app", "1.0.0", &[("alpha", "^1"), ("beta", "^1")]))
            .unwrap();
        catalog
            .add(release("alpha", "1.0.0", &[("shared", "^1")]))
            .unwrap();
        catalog
            .add(release("beta", "1.0.0", &[("shared", ">=1.4,<2")]))
            .unwrap();
        catalog.add(release("shared", "1.5.0", &[])).unwrap();
        catalog.add(release("shared", "1.2.0", &[])).unwrap();

        let result =
            resolve_scoped(&catalog, [ResolutionRequest::new("app", "^1").unwrap()]).unwrap();
        assert_eq!(
            result
                .root("app")
                .unwrap()
                .release("shared")
                .unwrap()
                .version
                .to_string(),
            "1.5.0"
        );
    }

    #[test]
    fn duplicate_root_request_is_rejected() {
        let mut catalog = PackageCatalog::default();
        catalog.add(release("app", "1.0.0", &[])).unwrap();
        let error = resolve_scoped(
            &catalog,
            [
                ResolutionRequest::new("app", "^1").unwrap(),
                ResolutionRequest::new("app", "^1").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(matches!(error, ScopedResolveError::DuplicateRoot(name) if name == "app"));
    }
}
