//! Deterministic, source-only dependency resolver for RBE external libraries.
//!
//! Registry/network adapters feed release metadata into [`PackageCatalog`]. This
//! crate performs no I/O and has no knowledge of the registry wire format.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use rbe_library_installer::PackageRequest;
use rbe_sdk::LIBRARY_ABI_VERSION;
use semver::{Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRelease {
    pub name: String,
    pub version: Version,
    pub rbe_abi_min: u32,
    pub rbe_abi_max: u32,
    pub yanked: bool,
    pub dependencies: BTreeMap<String, String>,
}

impl PackageRelease {
    pub fn new(
        name: impl Into<String>,
        version: impl AsRef<str>,
        rbe_abi_min: u32,
        rbe_abi_max: u32,
        yanked: bool,
        dependencies: BTreeMap<String, String>,
    ) -> Result<Self, ResolveError> {
        let name = name.into();
        validate_package_name(&name)?;
        let version = parse_exact_version(version.as_ref())?;
        if rbe_abi_min == 0 || rbe_abi_min > rbe_abi_max {
            return Err(ResolveError::InvalidAbiRange {
                package: name,
                min: rbe_abi_min,
                max: rbe_abi_max,
            });
        }
        for (dependency, requirement) in &dependencies {
            validate_package_name(dependency)?;
            parse_requirement(requirement)?;
        }
        Ok(Self {
            name,
            version,
            rbe_abi_min,
            rbe_abi_max,
            yanked,
            dependencies,
        })
    }

    pub const fn supports_abi(&self, abi: u32) -> bool {
        abi >= self.rbe_abi_min && abi <= self.rbe_abi_max
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageCatalog {
    releases: BTreeMap<String, BTreeMap<Version, PackageRelease>>,
}

impl PackageCatalog {
    pub fn add(&mut self, release: PackageRelease) -> Result<(), ResolveError> {
        let versions = self.releases.entry(release.name.clone()).or_default();
        if versions.contains_key(&release.version) {
            return Err(ResolveError::DuplicateRelease {
                package: release.name,
                version: release.version.to_string(),
            });
        }
        versions.insert(release.version.clone(), release);
        Ok(())
    }

    pub fn releases(&self, package: &str) -> Option<&BTreeMap<Version, PackageRelease>> {
        self.releases.get(package)
    }

    pub fn package_count(&self) -> usize {
        self.releases.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionRequest {
    pub name: String,
    pub requirement: String,
}

impl ResolutionRequest {
    pub fn new(
        name: impl Into<String>,
        requirement: impl Into<String>,
    ) -> Result<Self, ResolveError> {
        let name = name.into();
        validate_package_name(&name)?;
        let requirement = requirement.into();
        parse_requirement(&requirement)?;
        Ok(Self { name, requirement })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub selected: BTreeMap<String, PackageRelease>,
    pub install_order: Vec<String>,
}

impl Resolution {
    pub fn release(&self, package: &str) -> Option<&PackageRelease> {
        self.selected.get(package)
    }
}

#[derive(Debug, Clone)]
struct Constraint {
    raw: String,
    parsed: VersionReq,
}

type Constraints = BTreeMap<String, Vec<Constraint>>;

pub fn resolve(
    catalog: &PackageCatalog,
    requests: impl IntoIterator<Item = ResolutionRequest>,
) -> Result<Resolution, ResolveError> {
    resolve_for_abi(catalog, requests, LIBRARY_ABI_VERSION)
}

pub fn resolve_for_abi(
    catalog: &PackageCatalog,
    requests: impl IntoIterator<Item = ResolutionRequest>,
    host_abi: u32,
) -> Result<Resolution, ResolveError> {
    if host_abi == 0 {
        return Err(ResolveError::InvalidHostAbi);
    }

    let mut constraints = Constraints::new();
    let mut root_count = 0usize;
    for request in requests {
        root_count = root_count.saturating_add(1);
        add_constraint(&mut constraints, &request.name, &request.requirement)?;
    }
    if root_count == 0 {
        return Err(ResolveError::NoRoots);
    }

    let selected = solve(catalog, host_abi, constraints, BTreeMap::new())?;
    let install_order = dependency_first_order(&selected)?;
    Ok(Resolution {
        selected,
        install_order,
    })
}

fn solve(
    catalog: &PackageCatalog,
    host_abi: u32,
    constraints: Constraints,
    selected: BTreeMap<String, PackageRelease>,
) -> Result<BTreeMap<String, PackageRelease>, ResolveError> {
    ensure_selected_satisfy(&selected, &constraints, host_abi)?;

    let next = constraints
        .keys()
        .find(|name| !selected.contains_key(*name))
        .cloned();
    let Some(package) = next else {
        return Ok(selected);
    };

    let requirements = constraints
        .get(&package)
        .ok_or_else(|| ResolveError::InternalMissingConstraint(package.clone()))?;
    let versions = catalog
        .releases(&package)
        .ok_or_else(|| unsatisfied(&package, requirements))?;

    let candidates: Vec<PackageRelease> = versions
        .values()
        .rev()
        .filter(|release| {
            !release.yanked
                && release.supports_abi(host_abi)
                && requirements
                    .iter()
                    .all(|requirement| requirement.parsed.matches(&release.version))
        })
        .cloned()
        .collect();

    if candidates.is_empty() {
        return Err(unsatisfied(&package, requirements));
    }

    let mut last_error = None;
    for candidate in candidates {
        let mut branch_constraints = constraints.clone();
        let mut branch_selected = selected.clone();
        branch_selected.insert(package.clone(), candidate.clone());

        let mut invalid_dependency = None;
        for (dependency, requirement) in &candidate.dependencies {
            if let Err(error) = add_constraint(&mut branch_constraints, dependency, requirement) {
                invalid_dependency = Some(error);
                break;
            }
        }
        if let Some(error) = invalid_dependency {
            last_error = Some(error);
            continue;
        }

        match solve(catalog, host_abi, branch_constraints, branch_selected) {
            Ok(result) => return Ok(result),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| unsatisfied(&package, requirements)))
}

fn ensure_selected_satisfy(
    selected: &BTreeMap<String, PackageRelease>,
    constraints: &Constraints,
    host_abi: u32,
) -> Result<(), ResolveError> {
    for (name, release) in selected {
        if release.yanked || !release.supports_abi(host_abi) {
            return Err(ResolveError::SelectedReleaseInvalid {
                package: name.clone(),
                version: release.version.to_string(),
            });
        }
        if let Some(requirements) = constraints.get(name) {
            if !requirements
                .iter()
                .all(|requirement| requirement.parsed.matches(&release.version))
            {
                return Err(unsatisfied(name, requirements));
            }
        }
    }
    Ok(())
}

fn dependency_first_order(
    selected: &BTreeMap<String, PackageRelease>,
) -> Result<Vec<String>, ResolveError> {
    let mut permanent = BTreeSet::new();
    let mut temporary = BTreeSet::new();
    let mut stack = Vec::new();
    let mut order = Vec::with_capacity(selected.len());

    for package in selected.keys() {
        visit(
            package,
            selected,
            &mut permanent,
            &mut temporary,
            &mut stack,
            &mut order,
        )?;
    }
    Ok(order)
}

fn visit(
    package: &str,
    selected: &BTreeMap<String, PackageRelease>,
    permanent: &mut BTreeSet<String>,
    temporary: &mut BTreeSet<String>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<(), ResolveError> {
    if permanent.contains(package) {
        return Ok(());
    }
    if temporary.contains(package) {
        let start = stack.iter().position(|entry| entry == package).unwrap_or(0);
        let mut cycle = stack[start..].to_vec();
        cycle.push(package.to_string());
        return Err(ResolveError::DependencyCycle(cycle));
    }

    let release = selected
        .get(package)
        .ok_or_else(|| ResolveError::InternalMissingSelection(package.to_string()))?;
    temporary.insert(package.to_string());
    stack.push(package.to_string());

    for dependency in release.dependencies.keys() {
        if selected.contains_key(dependency) {
            visit(dependency, selected, permanent, temporary, stack, order)?;
        }
    }

    stack.pop();
    temporary.remove(package);
    permanent.insert(package.to_string());
    order.push(package.to_string());
    Ok(())
}

fn add_constraint(
    constraints: &mut Constraints,
    package: &str,
    requirement: &str,
) -> Result<(), ResolveError> {
    validate_package_name(package)?;
    let parsed = parse_requirement(requirement)?;
    let entries = constraints.entry(package.to_string()).or_default();
    if !entries.iter().any(|entry| entry.raw == requirement) {
        entries.push(Constraint {
            raw: requirement.to_string(),
            parsed,
        });
        entries.sort_by(|left, right| left.raw.cmp(&right.raw));
    }
    Ok(())
}

fn unsatisfied(package: &str, requirements: &[Constraint]) -> ResolveError {
    ResolveError::NoCompatibleRelease {
        package: package.to_string(),
        requirements: requirements.iter().map(|entry| entry.raw.clone()).collect(),
    }
}

fn validate_package_name(name: &str) -> Result<(), ResolveError> {
    PackageRequest::registry(name.to_string(), None::<String>)
        .map(|_| ())
        .map_err(|error| ResolveError::InvalidPackageName {
            name: name.to_string(),
            reason: error.to_string(),
        })
}

fn parse_exact_version(value: &str) -> Result<Version, ResolveError> {
    Version::parse(value).map_err(|source| ResolveError::InvalidVersion {
        value: value.to_string(),
        source,
    })
}

fn parse_requirement(value: &str) -> Result<VersionReq, ResolveError> {
    VersionReq::parse(value).map_err(|source| ResolveError::InvalidRequirement {
        value: value.to_string(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("at least one root library is required")]
    NoRoots,
    #[error("host ABI must be greater than zero")]
    InvalidHostAbi,
    #[error("invalid package name {name:?}: {reason}")]
    InvalidPackageName { name: String, reason: String },
    #[error("invalid package version {value:?}: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid version requirement {value:?}: {source}")]
    InvalidRequirement {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid ABI range for {package:?}: {min}..={max}")]
    InvalidAbiRange { package: String, min: u32, max: u32 },
    #[error("duplicate package release {package}@{version}")]
    DuplicateRelease { package: String, version: String },
    #[error("no compatible release for {package:?} satisfying {requirements:?}")]
    NoCompatibleRelease {
        package: String,
        requirements: Vec<String>,
    },
    #[error("selected release {package}@{version} became invalid during resolution")]
    SelectedReleaseInvalid { package: String, version: String },
    #[error("dependency cycle detected: {0:?}")]
    DependencyCycle(Vec<String>),
    #[error("resolver internal state lost constraints for {0:?}")]
    InternalMissingConstraint(String),
    #[error("resolver internal state lost selection for {0:?}")]
    InternalMissingSelection(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backtracks_from_highest_version_when_transitive_constraints_conflict() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(release("app", "1.0.0", &[("alpha", "^1"), ("beta", "^1")]))
            .unwrap();
        catalog
            .add(release("alpha", "1.1.0", &[("shared", "^2")]))
            .unwrap();
        catalog
            .add(release("alpha", "1.0.0", &[("shared", "^1")]))
            .unwrap();
        catalog
            .add(release("beta", "1.0.0", &[("shared", "^1")]))
            .unwrap();
        catalog.add(release("shared", "2.0.0", &[])).unwrap();
        catalog.add(release("shared", "1.5.0", &[])).unwrap();

        let result = resolve(&catalog, [ResolutionRequest::new("app", "^1").unwrap()]).unwrap();
        assert_eq!(
            result.release("alpha").unwrap().version,
            Version::new(1, 0, 0)
        );
        assert_eq!(
            result.release("shared").unwrap().version,
            Version::new(1, 5, 0)
        );
        let app = result
            .install_order
            .iter()
            .position(|name| name == "app")
            .unwrap();
        let alpha = result
            .install_order
            .iter()
            .position(|name| name == "alpha")
            .unwrap();
        let shared = result
            .install_order
            .iter()
            .position(|name| name == "shared")
            .unwrap();
        assert!(shared < alpha && alpha < app);
    }

    #[test]
    fn ignores_yanked_and_abi_incompatible_releases() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(PackageRelease::new("lib", "3.0.0", 2, 2, false, BTreeMap::new()).unwrap())
            .unwrap();
        catalog
            .add(PackageRelease::new("lib", "2.0.0", 1, 1, true, BTreeMap::new()).unwrap())
            .unwrap();
        catalog.add(release("lib", "1.5.0", &[])).unwrap();

        let result = resolve(&catalog, [ResolutionRequest::new("lib", ">=1").unwrap()]).unwrap();
        assert_eq!(
            result.release("lib").unwrap().version,
            Version::new(1, 5, 0)
        );
    }

    #[test]
    fn multiple_roots_share_constraints_deterministically() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(release("one", "1.0.0", &[("shared", ">=1,<3")]))
            .unwrap();
        catalog
            .add(release("two", "1.0.0", &[("shared", ">=2,<4")]))
            .unwrap();
        catalog.add(release("shared", "3.0.0", &[])).unwrap();
        catalog.add(release("shared", "2.5.0", &[])).unwrap();

        let result = resolve(
            &catalog,
            [
                ResolutionRequest::new("two", "^1").unwrap(),
                ResolutionRequest::new("one", "^1").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            result.release("shared").unwrap().version,
            Version::new(2, 5, 0)
        );
        assert_eq!(
            result.install_order.first().map(String::as_str),
            Some("shared")
        );
    }

    #[test]
    fn rejects_dependency_cycles_after_version_resolution() {
        let mut catalog = PackageCatalog::default();
        catalog.add(release("a", "1.0.0", &[("b", "^1")])).unwrap();
        catalog.add(release("b", "1.0.0", &[("a", "^1")])).unwrap();

        let error = resolve(&catalog, [ResolutionRequest::new("a", "^1").unwrap()]).unwrap_err();
        assert!(matches!(error, ResolveError::DependencyCycle(_)));
    }

    #[test]
    fn conflicting_requirements_fail_without_silent_downgrade() {
        let mut catalog = PackageCatalog::default();
        catalog
            .add(release("one", "1.0.0", &[("shared", "^1")]))
            .unwrap();
        catalog
            .add(release("two", "1.0.0", &[("shared", "^2")]))
            .unwrap();
        catalog.add(release("shared", "1.0.0", &[])).unwrap();
        catalog.add(release("shared", "2.0.0", &[])).unwrap();

        let error = resolve(
            &catalog,
            [
                ResolutionRequest::new("one", "^1").unwrap(),
                ResolutionRequest::new("two", "^1").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(matches!(error, ResolveError::NoCompatibleRelease { .. }));
    }

    #[test]
    fn duplicate_release_is_rejected() {
        let mut catalog = PackageCatalog::default();
        catalog.add(release("lib", "1.0.0", &[])).unwrap();
        let error = catalog.add(release("lib", "1.0.0", &[])).unwrap_err();
        assert!(matches!(error, ResolveError::DuplicateRelease { .. }));
    }
}
