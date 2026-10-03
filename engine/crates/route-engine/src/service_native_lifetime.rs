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
use std::sync::{Arc, Mutex};

use crate::service_cache_invalidation::ServiceCacheProtection;
use crate::service_native::{
    DynamicOidLease, DynamicOidPinRegistry, NativeRuntimeImagePins, NativeServiceArtifactPin,
    ServiceNativeError,
};

#[derive(Debug, Clone, Default)]
pub struct NativeServiceLifetimeRegistry {
    oid_pins: DynamicOidPinRegistry,
    artifacts: Arc<Mutex<ArtifactPinState>>,
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
        }
    }

    pub fn oid_pins(&self) -> DynamicOidPinRegistry {
        self.oid_pins.clone()
    }

    /// Pin every native Service selected by one immutable Runtime Image.
    /// Evaluator fallbacks do not own native cache artifacts.
    pub fn pin_image(
        &self,
        pins: &NativeRuntimeImagePins,
    ) -> Result<NativeServiceLifetimeLease, ServiceNativeError> {
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

    /// Exact artifact protection to pass into Phase-3/4 selective invalidation.
    /// This includes image- and worker-held leases, not merely the current image.
    pub fn cache_protection(&self) -> ServiceCacheProtection {
        let active = self
            .artifacts
            .lock()
            .expect("native Service artifact pin registry poisoned");
        ServiceCacheProtection {
            plan_hashes: active.plan_holders.keys().cloned().collect(),
            assembly_hashes: active.assembly_holders.keys().cloned().collect(),
        }
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
