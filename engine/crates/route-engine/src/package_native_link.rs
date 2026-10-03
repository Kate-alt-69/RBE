//! Phase 3 transactional package-OID publication.
//!
//! RPX export IDs are stable identities, while numeric package OIDs are local
//! to one project/Runtime Image lineage. This module applies that contract to
//! native records without rebuilding every package export: only the OIDs in
//! `PackageCacheDelta::materialize_oids` require fresh native fragments.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::oid_index_bridge::{
    package_specs_from_links, reconcile_package_index, OidIndexBridgeError, PackageCacheDelta,
    PackageLinkSnapshot,
};
use crate::oid_materialize::{
    remove_retired_dynamic_records, NativeOidFragment, OidMaterializationReport,
    OidMaterializeError,
};
use crate::service_cache_invalidation::{
    invalidate_service_cache_for_oids, ServiceCacheInvalidationError,
    ServiceCacheInvalidationReport, ServiceCacheProtection,
};
use crate::service_oid::{OidCache, OidError, OidIndex, OidRecord, OidRecordKind};

pub type PackageFragmentKey = (String, String);

#[derive(Debug, Clone)]
pub struct PreparedPackageNativeLink {
    base_index_sha256: String,
    next_index: OidIndex,
    delta: PackageCacheDelta,
    materialize: BTreeMap<PackageFragmentKey, u16>,
    pinned_oids: BTreeSet<u16>,
}

impl PreparedPackageNativeLink {
    pub fn delta(&self) -> &PackageCacheDelta {
        &self.delta
    }

    /// Exact RPX `(package, export_id) -> OID` targets Phase 2 must lower for
    /// this generation. Unchanged package records are deliberately absent.
    pub fn materialize_bindings(&self) -> &BTreeMap<PackageFragmentKey, u16> {
        &self.materialize
    }
}

#[derive(Debug, Clone)]
pub struct PackageNativeLinkReport {
    pub delta: PackageCacheDelta,
    pub invalidation: ServiceCacheInvalidationReport,
    pub materialization: OidMaterializationReport,
    pub removed_retired_oids: BTreeSet<u16>,
}

/// Resolve verified RPX package links into the next single-index generation
/// without mutating the live cache. Active package OIDs must be supplied by the
/// Runtime Image/worker liveness registry so an overlapping image cannot lose
/// its old numeric ownership.
pub fn prepare_package_native_link(
    cache: &OidCache,
    links: &[PackageLinkSnapshot],
    pinned_oids: &BTreeSet<u16>,
) -> Result<PreparedPackageNativeLink, PackageNativeLinkError> {
    let specs = package_specs_from_links(links).map_err(PackageNativeLinkError::Bridge)?;
    let base_index_sha256 = index_sha256(cache.index())?;
    let mut next_index = cache.index().clone();
    let delta = reconcile_package_index(&mut next_index, &specs, pinned_oids)
        .map_err(PackageNativeLinkError::Bridge)?;
    let materialize = package_materialization_bindings(&next_index, &delta.materialize_oids)?;

    Ok(PreparedPackageNativeLink {
        base_index_sha256,
        next_index,
        delta,
        materialize,
        pinned_oids: pinned_oids.clone(),
    })
}

/// Commit one prepared package generation.
///
/// Publication order intentionally mirrors the linked-REL transaction:
/// 1. reject stale prepare results;
/// 2. validate exactly the changed package fragments;
/// 3. invalidate only unprotected Service plans/bins that consume changed OIDs;
/// 4. write sparse package OID records;
/// 5. atomically publish the new single `oid/index`;
/// 6. delete only retired OID records no live image still pins.
pub fn commit_package_native_link(
    project_root: &Path,
    cache: &mut OidCache,
    prepared: PreparedPackageNativeLink,
    fragments: &BTreeMap<PackageFragmentKey, NativeOidFragment>,
    protection: &ServiceCacheProtection,
) -> Result<PackageNativeLinkReport, PackageNativeLinkError> {
    let observed_index_sha256 = index_sha256(cache.index())?;
    if observed_index_sha256 != prepared.base_index_sha256 {
        return Err(PackageNativeLinkError::StalePreparation {
            expected: prepared.base_index_sha256,
            observed: observed_index_sha256,
        });
    }

    validate_fragment_surface(&prepared.materialize, fragments)?;

    let invalidation = invalidate_service_cache_for_oids(
        project_root,
        prepared.delta.changed_oids.iter().copied(),
        protection,
    )
    .map_err(PackageNativeLinkError::Invalidation)?;

    let materialization = materialize_changed_package_records(
        cache,
        &prepared.next_index,
        &prepared.materialize,
        fragments,
    )?;

    cache
        .replace_index(prepared.next_index.clone())
        .map_err(PackageNativeLinkError::Oid)?;

    let removed_retired_oids = remove_retired_dynamic_records(
        cache,
        prepared.delta.retired_oids.iter().copied(),
        &prepared.pinned_oids,
    )
    .map_err(PackageNativeLinkError::Materialize)?;

    Ok(PackageNativeLinkReport {
        delta: prepared.delta,
        invalidation,
        materialization,
        removed_retired_oids,
    })
}

fn package_materialization_bindings(
    index: &OidIndex,
    materialize_oids: &BTreeSet<u16>,
) -> Result<BTreeMap<PackageFragmentKey, u16>, PackageNativeLinkError> {
    let mut bindings = BTreeMap::new();
    let mut found = BTreeSet::new();
    for owner in index.packages.values() {
        for (export_id, oid) in &owner.exports {
            if !materialize_oids.contains(oid) {
                continue;
            }
            let key = (owner.name.clone(), export_id.clone());
            if bindings.insert(key.clone(), *oid).is_some() || !found.insert(*oid) {
                return Err(PackageNativeLinkError::DuplicateMaterialization {
                    package: key.0,
                    export_id: key.1,
                    oid: *oid,
                });
            }
        }
    }
    if found != *materialize_oids {
        let missing = materialize_oids
            .difference(&found)
            .copied()
            .collect::<BTreeSet<_>>();
        return Err(PackageNativeLinkError::MissingMaterializationOwnership(
            missing,
        ));
    }
    Ok(bindings)
}

fn validate_fragment_surface(
    expected: &BTreeMap<PackageFragmentKey, u16>,
    fragments: &BTreeMap<PackageFragmentKey, NativeOidFragment>,
) -> Result<(), PackageNativeLinkError> {
    for key in expected.keys() {
        if !fragments.contains_key(key) {
            return Err(PackageNativeLinkError::MissingFragment {
                package: key.0.clone(),
                export_id: key.1.clone(),
            });
        }
    }
    for key in fragments.keys() {
        if !expected.contains_key(key) {
            return Err(PackageNativeLinkError::UnexpectedFragment {
                package: key.0.clone(),
                export_id: key.1.clone(),
            });
        }
    }
    Ok(())
}

fn materialize_changed_package_records(
    cache: &OidCache,
    index: &OidIndex,
    bindings: &BTreeMap<PackageFragmentKey, u16>,
    fragments: &BTreeMap<PackageFragmentKey, NativeOidFragment>,
) -> Result<OidMaterializationReport, PackageNativeLinkError> {
    let mut written = BTreeSet::new();
    let mut reused = BTreeSet::new();
    let mut record_sha256 = BTreeMap::new();

    for ((package_name, export_id), oid) in bindings {
        let owner = index
            .packages
            .get(package_name)
            .ok_or_else(|| PackageNativeLinkError::MissingPackageOwner(package_name.clone()))?;
        let fragment = fragments
            .get(&(package_name.clone(), export_id.clone()))
            .ok_or_else(|| PackageNativeLinkError::MissingFragment {
                package: package_name.clone(),
                export_id: export_id.clone(),
            })?;
        fragment
            .validate_for_oid(*oid)
            .map_err(PackageNativeLinkError::Materialize)?;

        let record = OidRecord {
            record_version: 1,
            oid: *oid,
            kind: OidRecordKind::PackageOperation,
            flags: fragment.flags,
            target: index.target.clone(),
            name: export_id.clone(),
            owner: Some(format!("{}@{}", owner.name, owner.version)),
            source_identity: Some(owner.artifact_sha256.clone()),
            required_oids: fragment.required_oids.clone(),
            capabilities: fragment.capabilities.clone(),
            relocations: fragment.relocations.clone(),
            diagnostics: fragment.diagnostics.clone(),
            machine_code: fragment.machine_code.clone(),
        };
        record.validate().map_err(PackageNativeLinkError::Oid)?;
        let hash = record
            .record_hash_hex()
            .map_err(PackageNativeLinkError::Oid)?;
        if cache
            .write_record_if_changed(&record)
            .map_err(PackageNativeLinkError::Oid)?
        {
            written.insert(*oid);
        } else {
            reused.insert(*oid);
        }
        record_sha256.insert(*oid, hash);
    }

    Ok(OidMaterializationReport {
        index_generation: index.generation,
        written_oids: written,
        reused_oids: reused,
        record_sha256,
    })
}

fn index_sha256(index: &OidIndex) -> Result<String, PackageNativeLinkError> {
    let bytes = index.to_bytes().map_err(PackageNativeLinkError::Oid)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug)]
pub enum PackageNativeLinkError {
    Oid(OidError),
    Bridge(OidIndexBridgeError),
    Materialize(OidMaterializeError),
    Invalidation(ServiceCacheInvalidationError),
    StalePreparation {
        expected: String,
        observed: String,
    },
    DuplicateMaterialization {
        package: String,
        export_id: String,
        oid: u16,
    },
    MissingMaterializationOwnership(BTreeSet<u16>),
    MissingPackageOwner(String),
    MissingFragment {
        package: String,
        export_id: String,
    },
    UnexpectedFragment {
        package: String,
        export_id: String,
    },
}

impl fmt::Display for PackageNativeLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "package OID cache failed: {error}"),
            Self::Bridge(error) => write!(formatter, "package OID reconciliation failed: {error}"),
            Self::Materialize(error) => write!(formatter, "package OID materialization failed: {error}"),
            Self::Invalidation(error) => write!(formatter, "package Service cache invalidation failed: {error}"),
            Self::StalePreparation { expected, observed } => write!(
                formatter,
                "package OID preparation targeted index {expected}, but live index is now {observed}; prepare again"
            ),
            Self::DuplicateMaterialization {
                package,
                export_id,
                oid,
            } => write!(
                formatter,
                "package materialization assigns duplicate OID {oid} at {package:?}/{export_id}"
            ),
            Self::MissingMaterializationOwnership(oids) => write!(
                formatter,
                "package reconciliation requested materialization for OIDs with no next-index owner: {oids:?}"
            ),
            Self::MissingPackageOwner(package) => {
                write!(formatter, "next OID index has no package owner {package:?}")
            }
            Self::MissingFragment { package, export_id } => write!(
                formatter,
                "native lowering did not provide changed package fragment {package:?}/{export_id}"
            ),
            Self::UnexpectedFragment { package, export_id } => write!(
                formatter,
                "native lowering provided unchanged/unlinked package fragment {package:?}/{export_id}"
            ),
        }
    }
}

impl std::error::Error for PackageNativeLinkError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_oid::{PackageOidOwner, OID_PACKAGE_START};

    fn owner(exports: &[(&str, u16)]) -> PackageOidOwner {
        let exports = exports
            .iter()
            .map(|(name, oid)| ((*name).to_string(), *oid))
            .collect::<BTreeMap<_, _>>();
        PackageOidOwner {
            name: "mail".into(),
            version: "2.0.0".into(),
            artifact_sha256: "a".repeat(64),
            owned_oids: exports.values().copied().collect(),
            exports,
        }
    }

    #[test]
    fn incremental_materialization_selects_only_changed_oids() {
        let mut index = OidIndex::fresh();
        index.packages.insert(
            "mail".into(),
            owner(&[
                ("lib_mail_send", OID_PACKAGE_START),
                ("lib_mail_recv", OID_PACKAGE_START + 1),
            ]),
        );
        let bindings =
            package_materialization_bindings(&index, &BTreeSet::from([OID_PACKAGE_START + 1]))
                .unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings.get(&("mail".into(), "lib_mail_recv".into())),
            Some(&(OID_PACKAGE_START + 1))
        );
    }

    #[test]
    fn extra_fragment_for_unchanged_export_is_rejected() {
        let expected =
            BTreeMap::from([(("mail".into(), "lib_mail_send".into()), OID_PACKAGE_START)]);
        let fragments = BTreeMap::from([
            (
                ("mail".into(), "lib_mail_send".into()),
                NativeOidFragment::executable(vec![0xC3]),
            ),
            (
                ("mail".into(), "lib_mail_recv".into()),
                NativeOidFragment::executable(vec![0xC3]),
            ),
        ]);
        assert!(matches!(
            validate_fragment_surface(&expected, &fragments),
            Err(PackageNativeLinkError::UnexpectedFragment { .. })
        ));
    }
}
