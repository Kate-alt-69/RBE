//! Phase 3 package commit hardening for Service-cache validity.
//!
//! Package ownership deltas catch installs/upgrades/removals, but the Service
//! cache contract is ultimately keyed by exact sparse OID record hashes. A
//! compiler/lowering change can rewrite a package record while retaining the
//! same numeric OID, so the guarded transaction must invalidate those consumers
//! too before a new Runtime Image can acquire native lifetime pins.

use std::collections::BTreeMap;
use std::path::Path;

use crate::oid_materialize::NativeOidFragment;
use crate::package_native_link::{
    commit_package_native_link, PackageFragmentKey, PackageNativeLinkError,
    PackageNativeLinkReport, PreparedPackageNativeLink,
};
use crate::service_cache_invalidation::{
    invalidate_service_cache_for_oids, ServiceCacheInvalidationReport, ServiceCacheProtection,
};
use crate::service_oid::OidCache;

pub fn commit_package_native_link_with_record_invalidation(
    project_root: &Path,
    cache: &mut OidCache,
    prepared: PreparedPackageNativeLink,
    fragments: &BTreeMap<PackageFragmentKey, NativeOidFragment>,
    protection: &ServiceCacheProtection,
) -> Result<PackageNativeLinkReport, PackageNativeLinkError> {
    let mut report =
        commit_package_native_link(project_root, cache, prepared, fragments, protection)?;

    let record_invalidation = invalidate_service_cache_for_oids(
        project_root,
        report.materialization.changed_oids.iter().copied(),
        protection,
    )
    .map_err(PackageNativeLinkError::Invalidation)?;
    merge_invalidation(&mut report.invalidation, record_invalidation);
    Ok(report)
}

fn merge_invalidation(
    current: &mut ServiceCacheInvalidationReport,
    additional: ServiceCacheInvalidationReport,
) {
    current.affected_oids.extend(additional.affected_oids);
    current
        .removed_plan_hashes
        .extend(additional.removed_plan_hashes);
    current
        .removed_assembly_hashes
        .extend(additional.removed_assembly_hashes);
    current
        .retained_plan_hashes
        .extend(additional.retained_plan_hashes);
    current
        .retained_assembly_hashes
        .extend(additional.retained_assembly_hashes);
    current.skipped_unreadable_plans = current
        .skipped_unreadable_plans
        .saturating_add(additional.skipped_unreadable_plans);
    current.skipped_unreadable_bins = current
        .skipped_unreadable_bins
        .saturating_add(additional.skipped_unreadable_bins);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn invalidation_merge_keeps_ownership_and_record_changes() {
        let mut current = ServiceCacheInvalidationReport {
            affected_oids: BTreeSet::from([20_086]),
            removed_plan_hashes: BTreeSet::from(["a".repeat(64)]),
            retained_assembly_hashes: BTreeSet::from(["b".repeat(64)]),
            ..ServiceCacheInvalidationReport::default()
        };
        let additional = ServiceCacheInvalidationReport {
            affected_oids: BTreeSet::from([20_087]),
            removed_assembly_hashes: BTreeSet::from(["c".repeat(64)]),
            retained_plan_hashes: BTreeSet::from(["d".repeat(64)]),
            ..ServiceCacheInvalidationReport::default()
        };
        merge_invalidation(&mut current, additional);
        assert_eq!(current.affected_oids, BTreeSet::from([20_086, 20_087]));
        assert_eq!(current.removed_plan_hashes.len(), 1);
        assert_eq!(current.removed_assembly_hashes.len(), 1);
        assert_eq!(current.retained_plan_hashes.len(), 1);
        assert_eq!(current.retained_assembly_hashes.len(), 1);
    }
}
