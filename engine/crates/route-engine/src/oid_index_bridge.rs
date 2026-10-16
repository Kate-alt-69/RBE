//! Phase 3 bridge between verified RPX roots and the one project-local OID index.
//!
//! `oid_link` owns deterministic/reuse policy while `service_oid` owns the
//! physical `.cache/compiler/oid/index` encoding. This module deliberately
//! joins those two layers instead of introducing another package/OID registry.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::oid_link::{
    canonical_package_export_id, reconcile_package_oids, OidLinkError, PackageExportSpec,
    PackageLinkSpec, PackageOidOwner as LinkedPackageOidOwner,
};
use crate::relc::{PackageLinkContext, PackageLinkError};
use crate::service_native::DynamicOidPinRegistry;
use crate::service_oid::{OidCache, OidError, OidIndex, PackageOidOwner as IndexedPackageOidOwner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageOidIdentity {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub export_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIndexDelta {
    pub previous_generation: u64,
    pub generation: u64,
    /// OIDs whose current package/export identity changed. Service plans that
    /// pin any of these records must be regenerated; unrelated plans survive.
    pub changed_oids: BTreeSet<u16>,
    /// OIDs no longer owned by the current image. A Phase-5 pin may still keep
    /// their sparse records alive until the final old Image/worker releases it.
    pub retired_oids: BTreeSet<u16>,
    /// OIDs requiring a new/refreshed PACKAGE_OPERATION record.
    pub materialize_oids: BTreeSet<u16>,
    pub changed_packages: BTreeSet<String>,
}

impl PackageIndexDelta {
    pub fn changed(&self) -> bool {
        self.previous_generation != self.generation
    }

    pub fn affects_required_oids(&self, required: impl IntoIterator<Item = u16>) -> bool {
        required
            .into_iter()
            .any(|oid| self.changed_oids.contains(&oid))
    }
}

/// Convert the verified, explicit-root-only package graph RELC already trusts
/// into Phase-3 allocation requests. RPX's stable public identity is derived
/// again here so the numeric OID never becomes part of the package artifact.
pub fn package_specs_from_links(
    links: &PackageLinkContext,
) -> Result<Vec<PackageLinkSpec>, OidIndexBridgeError> {
    links
        .validate()
        .map_err(OidIndexBridgeError::PackageLinks)?;
    let mut specs = Vec::with_capacity(links.roots.len());
    for (package, root) in &links.roots {
        let mut exports = Vec::with_capacity(root.exports.len());
        for export in root.exports.keys() {
            exports.push(PackageExportSpec {
                name: export.clone(),
                export_id: canonical_package_export_id(package, export)
                    .map_err(OidIndexBridgeError::LinkPolicy)?,
            });
        }
        specs.push(PackageLinkSpec {
            package: package.clone(),
            version: root.version.clone(),
            artifact_sha256: root.artifact_sha256.clone(),
            exports,
        });
    }
    Ok(specs)
}

/// Reconcile the package range directly into Phase 1's real `OidIndex`.
///
/// `pinned_oids` comes from Phase 5's `DynamicOidPinRegistry`; compatible
/// current owners may retain a pinned OID, but retired/changed owners cannot
/// reuse a pinned slot for a different target.
pub fn reconcile_package_index(
    index: &mut OidIndex,
    desired: &[PackageLinkSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<PackageIndexDelta, OidIndexBridgeError> {
    index
        .validate_structure()
        .map_err(OidIndexBridgeError::Oid)?;
    let previous_generation = index.generation;
    let current = index
        .packages
        .iter()
        .map(|(package, owner)| {
            (
                package.clone(),
                LinkedPackageOidOwner {
                    package: owner.name.clone(),
                    version: owner.version.clone(),
                    artifact_sha256: owner.artifact_sha256.clone(),
                    bindings: owner.exports.clone(),
                    owned_oids: owner.owned_oids.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let next = reconcile_package_oids(&current, desired, pinned_oids)
        .map_err(OidIndexBridgeError::LinkPolicy)?;

    let before = package_identity_by_oid(&current)?;
    let after = package_identity_by_oid(&next)?;
    let mut all_oids = before.keys().copied().collect::<BTreeSet<_>>();
    all_oids.extend(after.keys().copied());

    let mut changed_oids = BTreeSet::new();
    let mut retired_oids = BTreeSet::new();
    let mut materialize_oids = BTreeSet::new();
    let mut changed_packages = BTreeSet::new();
    for oid in all_oids {
        let old = before.get(&oid);
        let new = after.get(&oid);
        if old == new {
            continue;
        }
        changed_oids.insert(oid);
        if let Some(old) = old {
            changed_packages.insert(old.package.clone());
        }
        if let Some(new) = new {
            changed_packages.insert(new.package.clone());
            materialize_oids.insert(oid);
        } else {
            retired_oids.insert(oid);
        }
    }

    if !changed_oids.is_empty() {
        let mut next_index = index.clone();
        next_index.packages = next
            .into_iter()
            .map(|(package, owner)| {
                (
                    package,
                    IndexedPackageOidOwner {
                        name: owner.package,
                        version: owner.version,
                        artifact_sha256: owner.artifact_sha256,
                        owned_oids: owner.owned_oids,
                        exports: owner.bindings,
                    },
                )
            })
            .collect();
        next_index.generation = next_index
            .generation
            .checked_add(1)
            .ok_or(OidIndexBridgeError::GenerationExhausted)?;
        next_index
            .validate_structure()
            .map_err(OidIndexBridgeError::Oid)?;
        *index = next_index;
    }

    Ok(PackageIndexDelta {
        previous_generation,
        generation: index.generation,
        changed_oids,
        retired_oids,
        materialize_oids,
        changed_packages,
    })
}

/// Same reconciliation, but commits the updated ownership atomically through
/// the Phase-1 cache writer. A no-op reconciliation does not rewrite the index.
pub fn reconcile_package_cache(
    cache: &mut OidCache,
    desired: &[PackageLinkSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<PackageIndexDelta, OidIndexBridgeError> {
    let mut next = cache.index().clone();
    let delta = reconcile_package_index(&mut next, desired, pinned_oids)?;
    if delta.changed() {
        cache
            .replace_index(next)
            .map_err(OidIndexBridgeError::Oid)?;
    }
    Ok(delta)
}

pub fn reconcile_package_links(
    cache: &mut OidCache,
    links: &PackageLinkContext,
    pinned_oids: &BTreeSet<u16>,
) -> Result<PackageIndexDelta, OidIndexBridgeError> {
    let desired = package_specs_from_links(links)?;
    reconcile_package_cache(cache, &desired, pinned_oids)
}

/// Phase-5 safe package reconciliation. Callers that own the process-wide
/// native Runtime Image/worker pin registry should prefer this over manually
/// assembling a pinned-OID set; it makes old-image liveness part of the
/// reconciliation operation rather than an optional caller convention.
pub fn reconcile_package_links_with_active_pins(
    cache: &mut OidCache,
    links: &PackageLinkContext,
    active_pins: &DynamicOidPinRegistry,
) -> Result<PackageIndexDelta, OidIndexBridgeError> {
    let pinned_oids = active_pins.package_allocator_pins();
    reconcile_package_links(cache, links, &pinned_oids)
}

fn package_identity_by_oid(
    owners: &BTreeMap<String, LinkedPackageOidOwner>,
) -> Result<BTreeMap<u16, PackageOidIdentity>, OidIndexBridgeError> {
    let mut by_oid = BTreeMap::new();
    for (package, owner) in owners {
        for (export_id, &oid) in &owner.bindings {
            let identity = PackageOidIdentity {
                package: package.clone(),
                version: owner.version.clone(),
                artifact_sha256: owner.artifact_sha256.clone(),
                export_id: export_id.clone(),
            };
            if by_oid.insert(oid, identity).is_some() {
                return Err(OidIndexBridgeError::DuplicateOid(oid));
            }
        }
    }
    Ok(by_oid)
}

#[derive(Debug)]
pub enum OidIndexBridgeError {
    PackageLinks(PackageLinkError),
    LinkPolicy(OidLinkError),
    Oid(OidError),
    DuplicateOid(u16),
    GenerationExhausted,
}

impl fmt::Display for OidIndexBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageLinks(error) => write!(
                formatter,
                "verified package-link context is invalid: {error}"
            ),
            Self::LinkPolicy(error) => write!(
                formatter,
                "dynamic package OID reconciliation failed: {error}"
            ),
            Self::Oid(error) => write!(formatter, "project OID index update failed: {error}"),
            Self::DuplicateOid(oid) => write!(
                formatter,
                "package OID {oid} appears more than once while computing ownership delta"
            ),
            Self::GenerationExhausted => {
                write!(formatter, "OID index generation counter is exhausted")
            }
        }
    }
}

impl std::error::Error for OidIndexBridgeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relc::{PackageExportLink, PackageRootLink, PACKAGE_LINK_FORMAT};
    use crate::service_oid::OID_PACKAGE_START;

    fn sha(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn links(version: &str, artifact: char, exports: &[&str]) -> PackageLinkContext {
        PackageLinkContext {
            format: PACKAGE_LINK_FORMAT,
            roots: BTreeMap::from([(
                "mail".to_string(),
                PackageRootLink {
                    version: version.to_string(),
                    artifact_sha256: sha(artifact),
                    exports: exports
                        .iter()
                        .map(|name| {
                            (
                                (*name).to_string(),
                                PackageExportLink {
                                    entry: format!("components/{name}/{name}.ts"),
                                    language: "typescript".into(),
                                },
                            )
                        })
                        .collect(),
                },
            )]),
        }
    }

    #[test]
    fn verified_links_enter_the_single_oid_index() {
        let links = links("1.0.0", 'a', &["send", "receive"]);
        let specs = package_specs_from_links(&links).unwrap();
        let mut index = OidIndex::fresh();
        let delta = reconcile_package_index(&mut index, &specs, &BTreeSet::new()).unwrap();

        assert!(delta.changed());
        assert_eq!(index.packages.len(), 1);
        let owner = &index.packages["mail"];
        assert_eq!(owner.exports["lib_mail_receive"], OID_PACKAGE_START);
        assert_eq!(owner.exports["lib_mail_send"], OID_PACKAGE_START + 1);
        assert_eq!(delta.materialize_oids, owner.owned_oids);
    }

    #[test]
    fn unchanged_graph_does_not_bump_generation() {
        let links = links("1.0.0", 'a', &["send"]);
        let specs = package_specs_from_links(&links).unwrap();
        let mut index = OidIndex::fresh();
        reconcile_package_index(&mut index, &specs, &BTreeSet::new()).unwrap();
        let generation = index.generation;
        let oid = index.packages["mail"].exports["lib_mail_send"];

        let delta = reconcile_package_index(&mut index, &specs, &BTreeSet::from([oid])).unwrap();
        assert!(!delta.changed());
        assert_eq!(index.generation, generation);
        assert_eq!(index.packages["mail"].exports["lib_mail_send"], oid);
    }

    #[test]
    fn package_update_cannot_reuse_old_pinned_oid() {
        let first_links = links("1.0.0", 'a', &["send"]);
        let mut index = OidIndex::fresh();
        reconcile_package_index(
            &mut index,
            &package_specs_from_links(&first_links).unwrap(),
            &BTreeSet::new(),
        )
        .unwrap();
        let old_oid = index.packages["mail"].exports["lib_mail_send"];

        let second_links = links("2.0.0", 'b', &["send"]);
        let delta = reconcile_package_index(
            &mut index,
            &package_specs_from_links(&second_links).unwrap(),
            &BTreeSet::from([old_oid]),
        )
        .unwrap();
        let new_oid = index.packages["mail"].exports["lib_mail_send"];
        assert_ne!(old_oid, new_oid);
        assert!(delta.changed_oids.contains(&old_oid));
        assert!(delta.changed_oids.contains(&new_oid));
        assert!(delta.materialize_oids.contains(&new_oid));
    }

    #[test]
    fn delta_only_invalidates_services_that_use_changed_oids() {
        let first_links = links("1.0.0", 'a', &["send"]);
        let mut index = OidIndex::fresh();
        reconcile_package_index(
            &mut index,
            &package_specs_from_links(&first_links).unwrap(),
            &BTreeSet::new(),
        )
        .unwrap();
        let old_oid = index.packages["mail"].exports["lib_mail_send"];

        let second_links = links("2.0.0", 'b', &["send"]);
        let delta = reconcile_package_index(
            &mut index,
            &package_specs_from_links(&second_links).unwrap(),
            &BTreeSet::from([old_oid]),
        )
        .unwrap();
        assert!(delta.affects_required_oids([old_oid, 96]));
        assert!(!delta.affects_required_oids([96, 113]));
    }
}
