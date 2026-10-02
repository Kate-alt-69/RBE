//! Phase 5 native Service activation, cache pinning, and OID liveness.
//!
//! Phase 4 assembles verified OID records into a disposable native Service
//! `.bin`. This module owns the next boundary: pinning that exact assembly to an
//! immutable Runtime Image, keeping dynamic OIDs alive while older images and
//! workers still reference them, and treating a damaged `.bin` as disposable
//! cache instead of executable truth.
//!
//! ServiceManager must not consume these pins as its default execution path
//! until native execution/parity is proven. Until then an evaluator path is
//! allowed only when it is recorded as an explicit fallback with a reason.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::oid_link::{
    reachable_oids, PACKAGE_OID_END, PACKAGE_OID_START, REL_OID_END, REL_OID_START,
};
use crate::runtime_image::RuntimeImage;
use crate::service_bin::{
    decode_cached_service_bin, service_bin_path, AssembledServiceBin, DecodedServiceBin,
    OidRelocationKind, ServiceAssemblyPlan, VerifiedAssemblyOidRecord,
};
use crate::source_registry::{RelSourceKind, SourceId};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageArtifactPin {
    pub name: String,
    pub version: String,
    pub artifact_sha256: String,
}

impl PackageArtifactPin {
    pub fn dependency_key(&self) -> String {
        format!("package:{}@{}", self.name, self.version)
    }

    fn validate(&self) -> Result<(), ServiceNativeError> {
        if self.name.trim().is_empty()
            || self.version.trim().is_empty()
            || self.name.chars().any(char::is_control)
            || self.version.chars().any(char::is_control)
        {
            return Err(ServiceNativeError::InvalidPin(format!(
                "package pin requires a non-empty printable name/version, got {:?}@{:?}",
                self.name, self.version
            )));
        }
        validate_sha256(&self.artifact_sha256, "package artifact")
    }
}

/// Exact native artifact selected for one Service in one Runtime Image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeServiceArtifactPin {
    pub source_id: SourceId,
    pub oid_index_generation: u64,
    pub plan: ServiceAssemblyPlan,
    pub plan_hash: String,
    pub assembly_hash: String,
    pub packages: Vec<PackageArtifactPin>,
}

impl NativeServiceArtifactPin {
    pub fn from_assembled(
        source_id: SourceId,
        oid_index_generation: u64,
        plan: ServiceAssemblyPlan,
        bin: &AssembledServiceBin,
        packages: Vec<PackageArtifactPin>,
    ) -> Result<Self, ServiceNativeError> {
        let plan_hash = plan
            .plan_hash()
            .map_err(|error| ServiceNativeError::Assembly(error.to_string()))?;
        if bin.plan_hash != plan_hash {
            return Err(ServiceNativeError::InvalidPin(format!(
                "assembled Service plan hash {} does not match pinned plan {plan_hash}",
                bin.plan_hash
            )));
        }
        if bin.target_fingerprint != plan.target_fingerprint {
            return Err(ServiceNativeError::InvalidPin(format!(
                "assembled Service target {:?} does not match pinned target {:?}",
                bin.target_fingerprint, plan.target_fingerprint
            )));
        }
        validate_sha256(&bin.assembly_hash, "service assembly")?;

        let pin = Self {
            source_id,
            oid_index_generation,
            plan,
            plan_hash,
            assembly_hash: bin.assembly_hash.clone(),
            packages,
        };
        pin.validate()?;
        Ok(pin)
    }

    pub fn validate(&self) -> Result<(), ServiceNativeError> {
        self.plan
            .validate()
            .map_err(|error| ServiceNativeError::Assembly(error.to_string()))?;
        let expected_plan_hash = self
            .plan
            .plan_hash()
            .map_err(|error| ServiceNativeError::Assembly(error.to_string()))?;
        validate_sha256(&self.plan_hash, "service plan")?;
        validate_sha256(&self.assembly_hash, "service assembly")?;
        if self.plan_hash != expected_plan_hash {
            return Err(ServiceNativeError::InvalidPin(format!(
                "Service {} plan hash is stale: expected {expected_plan_hash}, pinned {}",
                self.source_id, self.plan_hash
            )));
        }

        let mut package_keys = BTreeSet::new();
        for package in &self.packages {
            package.validate()?;
            let key = package.dependency_key();
            if !package_keys.insert(key.clone()) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {} repeats package dependency {key}",
                    self.source_id
                )));
            }
            match self.plan.dependency_hashes.get(&key) {
                Some(hash) if hash == &package.artifact_sha256 => {}
                Some(hash) => {
                    return Err(ServiceNativeError::InvalidPin(format!(
                        "Service {} package dependency {key} pins artifact {} but plan pins {hash}",
                        self.source_id, package.artifact_sha256
                    )))
                }
                None => {
                    return Err(ServiceNativeError::InvalidPin(format!(
                        "Service {} package dependency {key} is not hashed into its assembly plan",
                        self.source_id
                    )))
                }
            }
        }
        Ok(())
    }

    pub fn required_dynamic_oids(&self) -> impl Iterator<Item = (u16, &str)> {
        self.plan
            .required_oids
            .iter()
            .filter(|record| is_dynamic_oid(record.oid))
            .map(|record| (record.oid, record.record_hash.as_str()))
    }

    pub fn cache_path(&self, compiler_cache_root: &Path) -> Result<PathBuf, ServiceNativeError> {
        service_bin_path(compiler_cache_root, &self.assembly_hash)
            .map_err(|error| ServiceNativeError::Assembly(error.to_string()))
    }
}

/// Native/fallback decision for every Service in one immutable Runtime Image.
/// Missing entries are rejected so fallback can never happen silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRuntimeImagePins {
    pub runtime_image_id: String,
    pub services: BTreeMap<SourceId, NativeServiceArtifactPin>,
    pub evaluator_fallbacks: BTreeMap<SourceId, String>,
}

impl NativeRuntimeImagePins {
    pub fn validate_for_image(&self, image: &RuntimeImage) -> Result<(), ServiceNativeError> {
        if self.runtime_image_id != image.image_id {
            return Err(ServiceNativeError::InvalidPin(format!(
                "native Service pins target Runtime Image {}, active image is {}",
                self.runtime_image_id, image.image_id
            )));
        }

        let image_services = image.services.iter().cloned().collect::<BTreeSet<_>>();
        for (id, pin) in &self.services {
            if id != &pin.source_id {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "native Service map key {id} does not match pin source {}",
                    pin.source_id
                )));
            }
            let source = image.source(id).ok_or_else(|| {
                ServiceNativeError::InvalidPin(format!(
                    "native Service pin {id} is not present in Runtime Image {}",
                    image.image_id
                ))
            })?;
            if source.kind != RelSourceKind::Service {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "native pin {id} targets {} instead of a Service",
                    source.kind
                )));
            }
            if pin.plan.service_identity != source.id.as_str() {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {id} plan identity {:?} does not match Runtime Image source {}",
                    pin.plan.service_identity, source.id
                )));
            }
            if self.evaluator_fallbacks.contains_key(id) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {id} cannot be both native and evaluator fallback"
                )));
            }
            pin.validate()?;
        }

        for (id, reason) in &self.evaluator_fallbacks {
            if !image_services.contains(id) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "evaluator fallback {id} is not a Service in Runtime Image {}",
                    image.image_id
                )));
            }
            if reason.trim().is_empty() {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {id} evaluator fallback requires an explicit unsupported-semantics reason"
                )));
            }
            if self.services.contains_key(id) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {id} cannot be both native and evaluator fallback"
                )));
            }
        }

        for id in image_services {
            if !self.services.contains_key(&id) && !self.evaluator_fallbacks.contains_key(&id) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "Service {id} has no native artifact and no explicit evaluator fallback"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DynamicOidPinState {
    record_hash: String,
    holders: u64,
}

/// Shared liveness registry used by Runtime Images and spawned workers.
#[derive(Debug, Clone, Default)]
pub struct DynamicOidPinRegistry {
    inner: Arc<Mutex<BTreeMap<u16, DynamicOidPinState>>>,
}

impl DynamicOidPinRegistry {
    pub fn pin_image(
        &self,
        pins: &NativeRuntimeImagePins,
    ) -> Result<DynamicOidLease, ServiceNativeError> {
        let mut records = BTreeMap::<u16, String>::new();
        for pin in pins.services.values() {
            pin.validate()?;
            for (oid, record_hash) in pin.required_dynamic_oids() {
                match records.insert(oid, record_hash.to_string()) {
                    Some(previous) if previous != record_hash => {
                        return Err(ServiceNativeError::OidPinConflict {
                            oid,
                            active_hash: previous,
                            requested_hash: record_hash.to_string(),
                        })
                    }
                    _ => {}
                }
            }
        }
        self.pin_records(records)
    }

    /// Worker-level lease. ServiceManager should hold this from worker spawn
    /// through worker teardown.
    pub fn pin_service(
        &self,
        pin: &NativeServiceArtifactPin,
    ) -> Result<DynamicOidLease, ServiceNativeError> {
        pin.validate()?;
        self.pin_records(
            pin.required_dynamic_oids()
                .map(|(oid, hash)| (oid, hash.to_string()))
                .collect(),
        )
    }

    /// Feed this directly into Phase-3 package OID reconciliation.
    pub fn package_allocator_pins(&self) -> BTreeSet<u16> {
        self.pins_in_range(PACKAGE_OID_START, PACKAGE_OID_END)
    }

    /// Feed this directly into Phase-3 linked-REL OID reconciliation.
    pub fn rel_allocator_pins(&self) -> BTreeSet<u16> {
        self.pins_in_range(REL_OID_START, REL_OID_END)
    }

    pub fn is_reusable(&self, oid: u16) -> bool {
        !self
            .inner
            .lock()
            .expect("dynamic OID pin registry poisoned")
            .contains_key(&oid)
    }

    pub fn holder_count(&self, oid: u16) -> u64 {
        self.inner
            .lock()
            .expect("dynamic OID pin registry poisoned")
            .get(&oid)
            .map_or(0, |state| state.holders)
    }

    fn pins_in_range(&self, start: u16, end: u16) -> BTreeSet<u16> {
        self.inner
            .lock()
            .expect("dynamic OID pin registry poisoned")
            .range(start..=end)
            .map(|(&oid, _)| oid)
            .collect()
    }

    fn pin_records(
        &self,
        requested: BTreeMap<u16, String>,
    ) -> Result<DynamicOidLease, ServiceNativeError> {
        let mut active = self
            .inner
            .lock()
            .expect("dynamic OID pin registry poisoned");

        // Validate the complete transaction before changing holder counts.
        for (&oid, requested_hash) in &requested {
            if !is_dynamic_oid(oid) {
                return Err(ServiceNativeError::InvalidPin(format!(
                    "attempted to lifetime-pin non-dynamic OID {oid}"
                )));
            }
            validate_sha256(requested_hash, "OID record")?;
            if let Some(existing) = active.get(&oid) {
                if existing.record_hash != *requested_hash {
                    return Err(ServiceNativeError::OidPinConflict {
                        oid,
                        active_hash: existing.record_hash.clone(),
                        requested_hash: requested_hash.clone(),
                    });
                }
            }
        }

        for (&oid, record_hash) in &requested {
            let state = active.entry(oid).or_insert_with(|| DynamicOidPinState {
                record_hash: record_hash.clone(),
                holders: 0,
            });
            state.holders = state.holders.saturating_add(1);
        }
        drop(active);

        Ok(DynamicOidLease {
            registry: self.clone(),
            records: requested,
            released: false,
        })
    }
}

#[derive(Debug)]
pub struct DynamicOidLease {
    registry: DynamicOidPinRegistry,
    records: BTreeMap<u16, String>,
    released: bool,
}

impl DynamicOidLease {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        let mut active = self
            .registry
            .inner
            .lock()
            .expect("dynamic OID pin registry poisoned");
        for (&oid, record_hash) in &self.records {
            let remove = match active.get_mut(&oid) {
                Some(state) if state.record_hash == *record_hash => {
                    state.holders = state.holders.saturating_sub(1);
                    state.holders == 0
                }
                _ => false,
            };
            if remove {
                active.remove(&oid);
            }
        }
        self.released = true;
    }
}

impl Drop for DynamicOidLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Runtime Image + exact native Service pins + a lifetime lease.
#[derive(Debug)]
pub struct NativeRuntimeImageSnapshot {
    image: Arc<RuntimeImage>,
    pins: Arc<NativeRuntimeImagePins>,
    _oid_lease: DynamicOidLease,
}

impl NativeRuntimeImageSnapshot {
    pub fn image(&self) -> &Arc<RuntimeImage> {
        &self.image
    }

    pub fn pins(&self) -> &Arc<NativeRuntimeImagePins> {
        &self.pins
    }
}

pub struct NativeRuntimeImageSlot {
    current: RwLock<Arc<NativeRuntimeImageSnapshot>>,
    oid_pins: DynamicOidPinRegistry,
}

impl NativeRuntimeImageSlot {
    pub fn new(
        initial: RuntimeImage,
        pins: NativeRuntimeImagePins,
        oid_pins: DynamicOidPinRegistry,
    ) -> Result<Self, ServiceNativeError> {
        pins.validate_for_image(&initial)?;
        let lease = oid_pins.pin_image(&pins)?;
        Ok(Self {
            current: RwLock::new(Arc::new(NativeRuntimeImageSnapshot {
                image: Arc::new(initial),
                pins: Arc::new(pins),
                _oid_lease: lease,
            })),
            oid_pins,
        })
    }

    pub fn snapshot(&self) -> Arc<NativeRuntimeImageSnapshot> {
        self.current
            .read()
            .expect("native Runtime Image slot poisoned")
            .clone()
    }

    /// Acquire Image B's OID lease before swapping it in. If B conflicts with
    /// any still-live Image A/worker OID, activation fails and A stays active.
    pub fn activate(
        &self,
        next: RuntimeImage,
        pins: NativeRuntimeImagePins,
    ) -> Result<Arc<NativeRuntimeImageSnapshot>, ServiceNativeError> {
        pins.validate_for_image(&next)?;
        let lease = self.oid_pins.pin_image(&pins)?;
        let next = Arc::new(NativeRuntimeImageSnapshot {
            image: Arc::new(next),
            pins: Arc::new(pins),
            _oid_lease: lease,
        });
        let mut current = self
            .current
            .write()
            .expect("native Runtime Image slot poisoned");
        Ok(std::mem::replace(&mut *current, next))
    }

    pub fn oid_pins(&self) -> DynamicOidPinRegistry {
        self.oid_pins.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceBinCacheLookup {
    Hit(DecodedServiceBin),
    Miss,
    CorruptEvicted { path: PathBuf, reason: String },
}

/// Load only the exact `.bin` selected by the Runtime Image pin. Corruption or
/// stale metadata is evicted and treated as rebuildable cache, never executed.
pub fn load_pinned_service_bin(
    compiler_cache_root: &Path,
    pin: &NativeServiceArtifactPin,
) -> Result<ServiceBinCacheLookup, ServiceNativeError> {
    pin.validate()?;
    let path = pin.cache_path(compiler_cache_root)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ServiceBinCacheLookup::Miss)
        }
        Err(error) => return Err(ServiceNativeError::Io(error.to_string())),
    };

    let decoded = match decode_cached_service_bin(&bytes) {
        Ok(decoded) => decoded,
        Err(error) => return evict_corrupt(path, error.to_string()),
    };
    let mismatch = if decoded.plan_hash != pin.plan_hash {
        Some(format!(
            "cached plan hash {} != pinned {}",
            decoded.plan_hash, pin.plan_hash
        ))
    } else if decoded.assembly_hash != pin.assembly_hash {
        Some(format!(
            "cached assembly hash {} != pinned {}",
            decoded.assembly_hash, pin.assembly_hash
        ))
    } else if decoded.target_fingerprint != pin.plan.target_fingerprint {
        Some(format!(
            "cached target {:?} != pinned {:?}",
            decoded.target_fingerprint, pin.plan.target_fingerprint
        ))
    } else {
        None
    };

    match mismatch {
        Some(reason) => evict_corrupt(path, reason),
        None => Ok(ServiceBinCacheLookup::Hit(decoded)),
    }
}

fn evict_corrupt(
    path: PathBuf,
    reason: String,
) -> Result<ServiceBinCacheLookup, ServiceNativeError> {
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ServiceNativeError::Io(format!(
                "failed to evict corrupt Service cache {}: {error}",
                path.display()
            )))
        }
    }
    Ok(ServiceBinCacheLookup::CorruptEvicted { path, reason })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinimizedServiceAssemblyInputs {
    pub plan: ServiceAssemblyPlan,
    pub records: BTreeMap<u16, VerifiedAssemblyOidRecord>,
    pub removed_oids: BTreeSet<u16>,
}

/// Phase-6 safe dead-code/minimal-OID pass.
///
/// Reachability combines RELC's call graph with OID relocation targets. This
/// pass only removes already-materialized fragments that are unreachable from
/// every Service entry OID; it does not rewrite semantics or machine code.
pub fn minimize_service_assembly_inputs(
    plan: &ServiceAssemblyPlan,
    records: &BTreeMap<u16, VerifiedAssemblyOidRecord>,
) -> Result<MinimizedServiceAssemblyInputs, ServiceNativeError> {
    plan.validate()
        .map_err(|error| ServiceNativeError::Assembly(error.to_string()))?;

    let required = plan
        .required_oids
        .iter()
        .map(|item| (item.oid, item))
        .collect::<BTreeMap<_, _>>();
    let mut graph = plan.call_graph.clone();

    for item in &plan.required_oids {
        let record = records.get(&item.oid).ok_or_else(|| {
            ServiceNativeError::InvalidPin(format!(
                "minimal Service assembly is missing required OID record {}",
                item.oid
            ))
        })?;
        if record.oid != item.oid
            || record.record_hash != item.record_hash
            || record.kind != item.kind
            || record.target_fingerprint != plan.target_fingerprint
        {
            return Err(ServiceNativeError::InvalidPin(format!(
                "OID {} record does not match the pinned Service plan",
                item.oid
            )));
        }

        let edges = graph.entry(item.oid).or_default();
        for relocation in &record.relocations {
            let target = match &relocation.kind {
                OidRelocationKind::Rel32ToOid { target_oid, .. }
                | OidRelocationKind::Abs64ToOid { target_oid, .. } => Some(*target_oid),
                OidRelocationKind::Abs64ToData { .. } => None,
            };
            if let Some(target) = target {
                if !required.contains_key(&target) {
                    return Err(ServiceNativeError::InvalidPin(format!(
                        "OID {} relocation targets unpinned OID {target}",
                        item.oid
                    )));
                }
                edges.insert(target);
            }
        }
    }

    let reachable = reachable_oids(plan.entry_oids.iter().copied(), &graph);
    let all_required = required.keys().copied().collect::<BTreeSet<_>>();
    let removed_oids = all_required
        .difference(&reachable)
        .copied()
        .collect::<BTreeSet<_>>();

    let mut minimized = plan.clone();
    minimized
        .required_oids
        .retain(|item| reachable.contains(&item.oid));
    minimized
        .placement_order
        .retain(|oid| reachable.contains(oid));
    minimized.call_graph = graph;
    minimized.call_graph.retain(|oid, dependencies| {
        if !reachable.contains(oid) {
            return false;
        }
        dependencies.retain(|dependency| reachable.contains(dependency));
        true
    });
    minimized
        .validate()
        .map_err(|error| ServiceNativeError::Assembly(error.to_string()))?;

    let minimized_records = records
        .iter()
        .filter(|(oid, _)| reachable.contains(oid))
        .map(|(&oid, record)| (oid, record.clone()))
        .collect();

    Ok(MinimizedServiceAssemblyInputs {
        plan: minimized,
        records: minimized_records,
        removed_oids,
    })
}

pub fn is_dynamic_oid(oid: u16) -> bool {
    (PACKAGE_OID_START..=PACKAGE_OID_END).contains(&oid)
        || (REL_OID_START..=REL_OID_END).contains(&oid)
}

fn validate_sha256(value: &str, label: &'static str) -> Result<(), ServiceNativeError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ServiceNativeError::InvalidPin(format!(
            "{label} SHA-256 must be 64 hexadecimal characters"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceNativeError {
    InvalidPin(String),
    OidPinConflict {
        oid: u16,
        active_hash: String,
        requested_hash: String,
    },
    Assembly(String),
    Io(String),
}

impl std::fmt::Display for ServiceNativeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPin(message) => formatter.write_str(message),
            Self::OidPinConflict {
                oid,
                active_hash,
                requested_hash,
            } => write!(
                formatter,
                "dynamic OID {oid} is still pinned to record {active_hash}; refusing reuse as {requested_hash}"
            ),
            Self::Assembly(message) => {
                write!(formatter, "Service assembly validation failed: {message}")
            }
            Self::Io(message) => write!(formatter, "Service native cache I/O failed: {message}"),
        }
    }
}

impl std::error::Error for ServiceNativeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_bin::{
        assemble_service_bin, AssemblyRecordKind, OidRelocation, RequiredOid, DONE_OID,
        SERVICE_PLAN_FORMAT,
    };

    fn sha(ch: char) -> String {
        std::iter::repeat(ch).take(64).collect()
    }

    fn record(oid: u16, hash: char) -> VerifiedAssemblyOidRecord {
        VerifiedAssemblyOidRecord {
            oid,
            record_hash: sha(hash),
            kind: AssemblyRecordKind::Function,
            target_fingerprint: "linux-x86_64-sysv".into(),
            alignment: 1,
            entry_offset: 0,
            frame_terminator: Some(DONE_OID),
            diagnostics: Vec::new(),
            machine_code: vec![0xC3],
            relocations: Vec::new(),
        }
    }

    fn plan(oids: &[(u16, char)]) -> ServiceAssemblyPlan {
        ServiceAssemblyPlan {
            format: SERVICE_PLAN_FORMAT,
            service_identity: "service:test".into(),
            service_source_sha256: sha('1'),
            index_identity_sha256: sha('2'),
            target_fingerprint: "linux-x86_64-sysv".into(),
            entry_oids: vec![oids[0].0],
            required_oids: oids
                .iter()
                .map(|(oid, hash)| RequiredOid {
                    oid: *oid,
                    record_hash: sha(*hash),
                    kind: AssemblyRecordKind::Function,
                })
                .collect(),
            placement_order: oids.iter().map(|(oid, _)| *oid).collect(),
            call_graph: BTreeMap::new(),
            service_data: Vec::new(),
            data_alignment: 8,
            dependency_hashes: BTreeMap::new(),
            compile_options: BTreeMap::new(),
        }
    }

    fn pin(oid: u16, hash: char) -> NativeServiceArtifactPin {
        let plan = plan(&[(oid, hash)]);
        let records = BTreeMap::from([(oid, record(oid, hash))]);
        let bin = assemble_service_bin(&plan, &records).unwrap();
        NativeServiceArtifactPin::from_assembled(
            SourceId::physical(RelSourceKind::Service, "test").unwrap(),
            7,
            plan,
            &bin,
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn old_dynamic_oid_cannot_be_reused_until_last_holder_drops() {
        let registry = DynamicOidPinRegistry::default();
        let old = pin(REL_OID_START, 'a');
        let replacement = pin(REL_OID_START, 'b');

        let image = registry.pin_service(&old).unwrap();
        let worker = registry.pin_service(&old).unwrap();
        assert_eq!(registry.holder_count(REL_OID_START), 2);
        assert!(registry.pin_service(&replacement).is_err());

        drop(image);
        assert_eq!(registry.holder_count(REL_OID_START), 1);
        assert!(registry.pin_service(&replacement).is_err());
        drop(worker);
        assert!(registry.is_reusable(REL_OID_START));
        assert!(registry.pin_service(&replacement).is_ok());
    }

    #[test]
    fn allocator_pin_sets_split_package_and_rel_ranges() {
        let registry = DynamicOidPinRegistry::default();
        let package = pin(PACKAGE_OID_START, 'a');
        let rel = pin(REL_OID_START, 'b');
        let _package_lease = registry.pin_service(&package).unwrap();
        let _rel_lease = registry.pin_service(&rel).unwrap();
        assert_eq!(
            registry.package_allocator_pins(),
            BTreeSet::from([PACKAGE_OID_START])
        );
        assert_eq!(
            registry.rel_allocator_pins(),
            BTreeSet::from([REL_OID_START])
        );
    }

    #[test]
    fn package_artifact_must_be_hashed_into_plan() {
        let oid = REL_OID_START;
        let mut plan = plan(&[(oid, 'a')]);
        let package = PackageArtifactPin {
            name: "mail".into(),
            version: "3.0.0".into(),
            artifact_sha256: sha('9'),
        };
        plan.dependency_hashes
            .insert(package.dependency_key(), package.artifact_sha256.clone());
        let records = BTreeMap::from([(oid, record(oid, 'a'))]);
        let bin = assemble_service_bin(&plan, &records).unwrap();
        let pin = NativeServiceArtifactPin::from_assembled(
            SourceId::physical(RelSourceKind::Service, "test").unwrap(),
            1,
            plan,
            &bin,
            vec![package],
        )
        .unwrap();
        pin.validate().unwrap();
    }

    #[test]
    fn corrupt_cached_bin_is_evicted_instead_of_returned() {
        let pin = pin(REL_OID_START, 'a');
        let root = std::env::temp_dir().join(format!(
            "rbe-service-native-cache-test-{}",
            std::process::id()
        ));
        let path = pin.cache_path(&root).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"definitely-not-a-service-bin").unwrap();

        let lookup = load_pinned_service_bin(&root, &pin).unwrap();
        assert!(matches!(
            lookup,
            ServiceBinCacheLookup::CorruptEvicted { .. }
        ));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn phase6_minimizer_keeps_call_and_relocation_dependencies() {
        let entry = REL_OID_START;
        let call_dep = REL_OID_START + 1;
        let relocation_dep = REL_OID_START + 2;
        let dead = REL_OID_START + 3;
        let mut plan = plan(&[
            (entry, 'a'),
            (call_dep, 'b'),
            (relocation_dep, 'c'),
            (dead, 'd'),
        ]);
        plan.call_graph.insert(entry, BTreeSet::from([call_dep]));

        let mut call_record = record(call_dep, 'b');
        call_record.machine_code = vec![0; 8];
        call_record.relocations.push(OidRelocation {
            offset: 0,
            kind: OidRelocationKind::Abs64ToOid {
                target_oid: relocation_dep,
                addend: 0,
            },
        });
        let records = BTreeMap::from([
            (entry, record(entry, 'a')),
            (call_dep, call_record),
            (relocation_dep, record(relocation_dep, 'c')),
            (dead, record(dead, 'd')),
        ]);

        let minimized = minimize_service_assembly_inputs(&plan, &records).unwrap();
        assert_eq!(minimized.removed_oids, BTreeSet::from([dead]));
        assert_eq!(
            minimized
                .plan
                .required_oids
                .iter()
                .map(|item| item.oid)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([entry, call_dep, relocation_dep])
        );
        assert!(!minimized.records.contains_key(&dead));
        minimized.plan.validate().unwrap();
    }
}
