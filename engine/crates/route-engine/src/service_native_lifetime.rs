//! Phase 5 lifetime authority for native Service OIDs and cache artifacts.
//!
//! A live Runtime Image/worker must protect two things together:
//! 1. the exact dynamic OID record meanings it executes; and
//! 2. the content-addressed Service plan/`.bin` selected by that image.
//!
//! Keeping these under one RAII lease prevents a draining worker from retaining
//! its OIDs while selective cache invalidation accidentally deletes the exact
//! plan or native binary the worker would need for restart.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::oid_materialize::NativeOidFragment;
use crate::package_native_link::{
    prepare_package_native_link, PackageFragmentKey, PackageNativeLinkError,
    PackageNativeLinkReport, PreparedPackageNativeLink,
};
use crate::package_native_semantics::commit_package_native_link_with_record_invalidation;
use crate::rel_native_link::{
    prepare_rel_native_link, PreparedRelNativeLink, RelNativeLinkError, RelNativeLinkReport,
    ServiceNativeLinkInput,
};
use crate::rel_native_semantics::{
    commit_rel_native_link_with_semantic_dependencies, RelNativeSemanticError,
};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::relc::PackageLinkContext;
use crate::service_cache_invalidation::ServiceCacheProtection;
use crate::service_native::{
    DynamicOidLease, DynamicOidPinRegistry, NativeRuntimeImagePins, NativeServiceArtifactPin,
    ServiceNativeError,
};
use crate::service_oid::OidCache;

#[derive(Debug, Clone, Default)]
pub struct NativeServiceLifetimeRegistry {
    oid_pins: DynamicOidPinRegistry,
    artifacts: Arc<Mutex<ArtifactPinState>>,
    link_transaction: Arc<Mutex<()>>,
}

#[derive(Debug, Default)]
struct ArtifactPinState {
    plan_holders: BTreeMap<String, u64>,
    assembly_holders: BTreeMap<String, u64>,
}

impl NativeServiceLifetimeRegistry {
    pub fn new(oid_pins: DynamicOidPinRegistry) -> Self {
        Self {
            oid_pins,
            artifacts: Arc::new(Mutex::new(ArtifactPinState::default())),
            link_transaction: Arc::new(Mutex::new(())),
        }
    }

    pub fn oid_pins(&self) -> DynamicOidPinRegistry {
        self.oid_pins.clone()
    }

    /// Hold this guard across native-link prepare, native lowering, and commit.
    ///
    /// New image/worker lifetime acquisition is serialized behind the guard, so
    /// the allocator pin set and cache-protection set observed during prepare do
    /// not become stale before commit deletes retired records/cache artifacts.
    pub fn begin_link_transaction(&self) -> NativeServiceLinkGuard<'_> {
        NativeServiceLinkGuard {
            registry: self,
            _transaction: self
                .link_transaction
                .lock()
                .expect("native Service link transaction registry poisoned"),
        }
    }

    /// Pin every native Service selected by one immutable Runtime Image.
    /// Evaluator fallbacks do not own native cache artifacts.
    pub fn pin_image(
        &self,
        pins: &NativeRuntimeImagePins,
    ) -> Result<NativeServiceLifetimeLease, ServiceNativeError> {
        let _transaction = self
            .link_transaction
            .lock()
            .expect("native Service link transaction registry poisoned");
        let oid_lease = self.oid_pins.pin_image(pins)?;
        let artifacts = pins
            .services
            .values()
            .map(|pin| {
                pin.validate()?;
                Ok((pin.plan_hash.clone(), pin.assembly_hash.clone()))
            })
            .collect::<Result<BTreeSet<_>, ServiceNativeError>>()?;
        let artifact_lease = self.pin_artifacts(artifacts);
        Ok(NativeServiceLifetimeLease {
            _oid_lease: oid_lease,
            _artifact_lease: artifact_lease,
        })
    }

    /// Worker-level lease. Hold this from worker spawn until final teardown so
    /// an overlapping image compile cannot reuse its OIDs or delete its cache.
    pub fn pin_service(
        &self,
        pin: &NativeServiceArtifactPin,
    ) -> Result<NativeServiceLifetimeLease, ServiceNativeError> {
        let _transaction = self
            .link_transaction
            .lock()
            .expect("native Service link transaction registry poisoned");
        pin.validate()?;
        let oid_lease = self.oid_pins.pin_service(pin)?;
        let artifact_lease = self.pin_artifacts(BTreeSet::from([(
            pin.plan_hash.clone(),
            pin.assembly_hash.clone(),
        )]));
        Ok(NativeServiceLifetimeLease {
            _oid_lease: oid_lease,
            _artifact_lease: artifact_lease,
        })
    }

    /// Snapshot current artifact protection. Native-link callers should prefer
    /// `begin_link_transaction` and the guard's commit helpers so this snapshot
    /// cannot become stale before invalidation executes.
    pub fn cache_protection(&self) -> ServiceCacheProtection {
        let active = self
            .artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned");
        protection_from_state(&active)
    }

    pub fn package_allocator_pins(&self) -> BTreeSet<u16> {
        self.oid_pins.package_allocator_pins()
    }

    pub fn rel_allocator_pins(&self) -> BTreeSet<u16> {
        self.oid_pins.rel_allocator_pins()
    }

    pub fn plan_holder_count(&self, hash: &str) -> u64 {
        self.artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned")
            .plan_holders
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    pub fn assembly_holder_count(&self, hash: &str) -> u64 {
        self.artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned")
            .assembly_holders
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    fn pin_artifacts(&self, artifacts: BTreeSet<(String, String)>) -> ArtifactLease {
        let mut active = self
            .artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned");
        for (plan_hash, assembly_hash) in &artifacts {
            *active.plan_holders.entry(plan_hash.clone()).or_default() += 1;
            *active
                .assembly_holders
                .entry(assembly_hash.clone())
                .or_default() += 1;
        }
        drop(active);
        ArtifactLease {
            registry: self.clone(),
            artifacts,
            released: false,
        }
    }

    fn release_artifacts(&self, artifacts: &BTreeSet<(String, String)>) {
        let mut active = self
            .artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned");
        for (plan_hash, assembly_hash) in artifacts {
            decrement_holder(&mut active.plan_holders, plan_hash);
            decrement_holder(&mut active.assembly_holders, assembly_hash);
        }
    }
}

/// Exclusive Phase-3/4 native-link view of Phase-5 liveness.
///
/// Keep this value alive from prepare through commit. While it exists, no new
/// Runtime Image or worker can acquire a native lifetime lease, so OID and cache
/// liveness cannot change underneath the prepared transaction.
#[derive(Debug)]
pub struct NativeServiceLinkGuard<'a> {
    registry: &'a NativeServiceLifetimeRegistry,
    _transaction: MutexGuard<'a, ()>,
}

impl NativeServiceLinkGuard<'_> {
    pub fn prepare_package(
        &self,
        cache: &OidCache,
        links: &PackageLinkContext,
    ) -> Result<PreparedPackageNativeLink, PackageNativeLinkError> {
        let pinned_oids = self.registry.package_allocator_pins();
        prepare_package_native_link(cache, links, &pinned_oids)
    }

    /// Commit package OIDs through the record-hash invalidation wrapper so a
    /// same-OID native record rewrite cannot leave an unpinned stale Service bin.
    pub fn commit_package(
        &self,
        project_root: &Path,
        cache: &mut OidCache,
        prepared: PreparedPackageNativeLink,
        fragments: &BTreeMap<PackageFragmentKey, NativeOidFragment>,
    ) -> Result<PackageNativeLinkReport, PackageNativeLinkError> {
        let protection = self.registry.cache_protection();
        commit_package_native_link_with_record_invalidation(
            project_root,
            cache,
            prepared,
            fragments,
            &protection,
        )
    }

    pub fn prepare_rel(
        &self,
        cache: &OidCache,
        discovery: &LinkedRelDiscovery,
    ) -> Result<PreparedRelNativeLink, RelNativeLinkError> {
        let pinned_oids = self.registry.rel_allocator_pins();
        prepare_rel_native_link(cache, discovery, &pinned_oids)
    }

    /// Commit through Phase 4's semantic binder so every guarded Service plan
    /// pins the exact transitive REL identities/capabilities it was lowered from.
    pub fn commit_rel(
        &self,
        project_root: &Path,
        cache: &mut OidCache,
        prepared: PreparedRelNativeLink,
        fragments: &BTreeMap<String, NativeOidFragment>,
        services: &[ServiceNativeLinkInput],
    ) -> Result<RelNativeLinkReport, RelNativeSemanticError> {
        let protection = self.registry.cache_protection();
        commit_rel_native_link_with_semantic_dependencies(
            project_root,
            cache,
            prepared,
            fragments,
            services,
            &protection,
        )
    }
}

fn protection_from_state(active: &ArtifactPinState) -> ServiceCacheProtection {
    ServiceCacheProtection {
        plan_hashes: active.plan_holders.keys().cloned().collect(),
        assembly_hashes: active.assembly_holders.keys().cloned().collect(),
    }
}

fn decrement_holder(holders: &mut BTreeMap<String, u64>, hash: &str) {
    let remove = match holders.get_mut(hash) {
        Some(count) => {
            *count = count.saturating_sub(1);
            *count == 0
        }
        None => false,
    };
    if remove {
        holders.remove(hash);
    }
}

#[derive(Debug)]
pub struct NativeServiceLifetimeLease {
    _oid_lease: DynamicOidLease,
    _artifact_lease: ArtifactLease,
}

#[derive(Debug)]
struct ArtifactLease {
    registry: NativeServiceLifetimeRegistry,
    artifacts: BTreeSet<(String, String)>,
    released: bool,
}

impl ArtifactLease {
    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.registry.release_artifacts(&self.artifacts);
        self.released = true;
    }
}

impl Drop for ArtifactLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}
