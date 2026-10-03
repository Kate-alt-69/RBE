//! Phase 4 orchestration from linked-REL discovery to sparse OID records.
//!
//! The link is deliberately two-stage. `prepare_rel_native_link` allocates OIDs
//! against a cloned index and exposes those exact numeric bindings to native
//! lowering without mutating the live cache. `commit_rel_native_link` accepts
//! the lowered fragments, invalidates only stale Service artifacts that depend
//! on changed OIDs, materializes sparse records, atomically publishes the
//! prepared index, and emits exact `NativeServiceBuildSpec`s for Phase 5.
//!
//! Phase 4 does not duplicate Phase 5's plan -> `.bin` -> immutable-pin build
//! transaction. The handoff is numeric entry OIDs plus exact source/dependency
//! identity; `service_native_build` consumes that after this link succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::oid_link::{LinkedRelBinding, LinkedRelKind, LinkedRelSymbolSpec};
use crate::oid_materialize::{
    materialize_rel_records, remove_retired_dynamic_records, NativeOidFragment,
    OidMaterializationReport, OidMaterializeError,
};
use crate::rel_oid_bridge::{reconcile_rel_index, RelOidBridgeError, RelReconcileReport};
use crate::rel_symbol_discovery::LinkedRelDiscovery;
use crate::service_cache_invalidation::{
    invalidate_service_cache_for_oids, ServiceCacheInvalidationError,
    ServiceCacheInvalidationReport, ServiceCacheProtection,
};
use crate::service_native::PackageArtifactPin;
use crate::service_native_build::NativeServiceBuildSpec;
use crate::service_oid::{OidCache, OidError, OidIndex};
use crate::source_registry::SourceId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNativeLinkInput {
    /// Logical Service name used by `LinkedRelDiscovery::service_roots`.
    pub logical_name: String,
    /// Stable Runtime Image source identity, normally `service:<logical-name>`.
    pub source_id: SourceId,
    pub source_sha256: String,
    pub service_data: Vec<u8>,
    pub dependency_hashes: BTreeMap<String, String>,
    pub compile_options: BTreeMap<String, String>,
    pub packages: Vec<PackageArtifactPin>,
}

#[derive(Debug, Clone)]
pub struct PreparedRelNativeLink {
    base_index_sha256: String,
    next_index: OidIndex,
    reconcile: RelReconcileReport,
    normalized_symbols: Vec<LinkedRelSymbolSpec>,
    service_roots: BTreeMap<String, BTreeSet<String>>,
    pinned_oids: BTreeSet<u16>,
}

impl PreparedRelNativeLink {
    /// Numeric identities native lowering must target. These are not published
    /// to the live `oid/index` until commit has materialized every requested
    /// sparse record.
    pub fn bindings(&self) -> &BTreeMap<String, LinkedRelBinding> {
        &self.reconcile.bindings
    }

    pub fn normalized_symbols(&self) -> &[LinkedRelSymbolSpec] {
        &self.normalized_symbols
    }

    pub fn service_roots(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.service_roots
    }

    pub fn reconcile_report(&self) -> &RelReconcileReport {
        &self.reconcile
    }
}

#[derive(Debug, Clone)]
pub struct RelNativeLinkReport {
    pub reconcile: RelReconcileReport,
    pub invalidation: ServiceCacheInvalidationReport,
    pub materialization: OidMaterializationReport,
    pub removed_retired_oids: BTreeSet<u16>,
    /// Ready for `service_native_build::build_native_runtime_image_pins`.
    pub service_specs: BTreeMap<SourceId, NativeServiceBuildSpec>,
}

/// Allocate/reuse linked REL OIDs against a private index snapshot.
///
/// Self recursion is intentionally removed from `required_symbols`: recursion
/// remains part of the native function's control flow/relocations, while the
/// OID reachability graph describes dependencies on *other* sparse records.
/// Keeping a self edge here would contradict `oid_materialize` and create a
/// useless service-plan cycle.
pub fn prepare_rel_native_link(
    cache: &OidCache,
    discovery: &LinkedRelDiscovery,
    pinned_oids: &BTreeSet<u16>,
) -> Result<PreparedRelNativeLink, RelNativeLinkError> {
    let normalized_symbols = normalize_symbols(&discovery.symbols)?;
    validate_service_roots(&discovery.service_roots, &normalized_symbols)?;

    let base_index_sha256 = index_sha256(cache.index())?;
    let mut next_index = cache.index().clone();
    let reconcile = reconcile_rel_index(&mut next_index, &normalized_symbols, pinned_oids)
        .map_err(RelNativeLinkError::Bridge)?;

    Ok(PreparedRelNativeLink {
        base_index_sha256,
        next_index,
        reconcile,
        normalized_symbols,
        service_roots: discovery.service_roots.clone(),
        pinned_oids: pinned_oids.clone(),
    })
}

/// Publish one prepared linked-REL generation.
///
/// The live index is checked again before any cache artifact or sparse record is
/// changed, so a stale prepare result cannot silently overwrite a newer compiler
/// generation. Service inputs are also fully validated before publication.
/// Unprotected plans/binaries that reference changed OIDs are selectively
/// removed; live Image A artifacts survive while Image B receives fresh OIDs.
pub fn commit_rel_native_link(
    project_root: &Path,
    cache: &mut OidCache,
    prepared: PreparedRelNativeLink,
    fragments: &BTreeMap<String, NativeOidFragment>,
    services: &[ServiceNativeLinkInput],
    protection: &ServiceCacheProtection,
) -> Result<RelNativeLinkReport, RelNativeLinkError> {
    let observed_index_sha256 = index_sha256(cache.index())?;
    if observed_index_sha256 != prepared.base_index_sha256 {
        return Err(RelNativeLinkError::StalePreparation {
            expected: prepared.base_index_sha256,
            observed: observed_index_sha256,
        });
    }

    let effective_fragments = effective_fragments(&prepared.normalized_symbols, fragments)?;
    let service_specs = build_service_specs(&prepared, services)?;

    // Plans are already content-addressed by their exact required OID record
    // hashes. Delete only stale, unprotected consumers of OIDs whose ownership
    // changed. Pinned Image A artifacts remain available until Phase 5 releases
    // their liveness pins.
    let invalidation = invalidate_service_cache_for_oids(
        project_root,
        prepared.reconcile.delta.changed_oids.iter().copied(),
        protection,
    )
    .map_err(RelNativeLinkError::Invalidation)?;

    // Sparse records are written before the index is published. If a record
    // write fails, the live index still describes the previous complete link
    // state. Successfully written orphan cache records are harmless and
    // rebuildable; they are not source-of-truth state.
    let materialization =
        materialize_rel_records(cache, &prepared.reconcile.bindings, &effective_fragments)
            .map_err(RelNativeLinkError::Materialize)?;

    cache
        .replace_index(prepared.next_index.clone())
        .map_err(RelNativeLinkError::Oid)?;

    let removed_retired_oids = remove_retired_dynamic_records(
        cache,
        prepared.reconcile.delta.retired_oids.iter().copied(),
        &prepared.pinned_oids,
    )
    .map_err(RelNativeLinkError::Materialize)?;

    Ok(RelNativeLinkReport {
        reconcile: prepared.reconcile,
        invalidation,
        materialization,
        removed_retired_oids,
        service_specs,
    })
}

fn build_service_specs(
    prepared: &PreparedRelNativeLink,
    services: &[ServiceNativeLinkInput],
) -> Result<BTreeMap<SourceId, NativeServiceBuildSpec>, RelNativeLinkError> {
    let symbol_sources = prepared
        .normalized_symbols
        .iter()
        .map(|symbol| (symbol.canonical_id.clone(), symbol.source_sha256.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut seen_logical = BTreeSet::new();
    let mut specs = BTreeMap::new();
    for service in services {
        if !seen_logical.insert(service.logical_name.clone()) {
            return Err(RelNativeLinkError::DuplicateServiceLogicalName(
                service.logical_name.clone(),
            ));
        }
        if specs.contains_key(&service.source_id) {
            return Err(RelNativeLinkError::DuplicateServiceIdentity(
                service.source_id.clone(),
            ));
        }

        let roots = prepared
            .service_roots
            .get(&service.logical_name)
            .ok_or_else(|| RelNativeLinkError::MissingServiceRoots(service.logical_name.clone()))?;
        if roots.is_empty() {
            return Err(RelNativeLinkError::EmptyServiceRoots(
                service.logical_name.clone(),
            ));
        }

        let mut entry_oids = BTreeSet::new();
        for root in roots {
            let source_sha256 =
                symbol_sources
                    .get(root)
                    .ok_or_else(|| RelNativeLinkError::UnknownServiceRoot {
                        service: service.logical_name.clone(),
                        symbol: root.clone(),
                    })?;
            if source_sha256 != &service.source_sha256 {
                return Err(RelNativeLinkError::ServiceSourceMismatch {
                    service: service.logical_name.clone(),
                    symbol: root.clone(),
                    expected: service.source_sha256.clone(),
                    observed: source_sha256.clone(),
                });
            }
            let binding = prepared.reconcile.bindings.get(root).ok_or_else(|| {
                RelNativeLinkError::UnboundServiceRoot {
                    service: service.logical_name.clone(),
                    symbol: root.clone(),
                }
            })?;
            entry_oids.insert(binding.oid);
        }

        specs.insert(
            service.source_id.clone(),
            NativeServiceBuildSpec {
                source_id: service.source_id.clone(),
                service_source_sha256: service.source_sha256.clone(),
                entry_oids: entry_oids.into_iter().collect(),
                service_data: service.service_data.clone(),
                dependency_hashes: service.dependency_hashes.clone(),
                compile_options: service.compile_options.clone(),
                packages: service.packages.clone(),
            },
        );
    }
    Ok(specs)
}

fn normalize_symbols(
    symbols: &[LinkedRelSymbolSpec],
) -> Result<Vec<LinkedRelSymbolSpec>, RelNativeLinkError> {
    let all = symbols
        .iter()
        .map(|symbol| symbol.canonical_id.clone())
        .collect::<BTreeSet<_>>();
    if all.len() != symbols.len() {
        return Err(RelNativeLinkError::DuplicateSymbol);
    }

    let mut normalized = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let mut symbol = symbol.clone();
        symbol.required_symbols.remove(&symbol.canonical_id);
        for dependency in &symbol.required_symbols {
            if !all.contains(dependency) {
                return Err(RelNativeLinkError::MissingSymbolDependency {
                    symbol: symbol.canonical_id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        normalized.push(symbol);
    }
    normalized.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
    Ok(normalized)
}

fn validate_service_roots(
    roots: &BTreeMap<String, BTreeSet<String>>,
    symbols: &[LinkedRelSymbolSpec],
) -> Result<(), RelNativeLinkError> {
    let all = symbols
        .iter()
        .map(|symbol| symbol.canonical_id.as_str())
        .collect::<BTreeSet<_>>();
    for (service, service_roots) in roots {
        for root in service_roots {
            if !all.contains(root.as_str()) {
                return Err(RelNativeLinkError::UnknownServiceRoot {
                    service: service.clone(),
                    symbol: root.clone(),
                });
            }
        }
    }
    Ok(())
}

fn effective_fragments(
    symbols: &[LinkedRelSymbolSpec],
    fragments: &BTreeMap<String, NativeOidFragment>,
) -> Result<BTreeMap<String, NativeOidFragment>, RelNativeLinkError> {
    let kinds = symbols
        .iter()
        .map(|symbol| (symbol.canonical_id.clone(), symbol.kind))
        .collect::<BTreeMap<_, _>>();
    for canonical_id in fragments.keys() {
        if !kinds.contains_key(canonical_id) {
            return Err(RelNativeLinkError::UnknownFragment(canonical_id.clone()));
        }
    }

    let mut effective = fragments.clone();
    for (canonical_id, kind) in kinds {
        if kind == LinkedRelKind::Class {
            effective
                .entry(canonical_id)
                .or_insert_with(NativeOidFragment::descriptor);
        }
    }
    Ok(effective)
}

fn index_sha256(index: &OidIndex) -> Result<String, RelNativeLinkError> {
    let bytes = index.to_bytes().map_err(RelNativeLinkError::Oid)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug)]
pub enum RelNativeLinkError {
    Oid(OidError),
    Bridge(RelOidBridgeError),
    Invalidation(ServiceCacheInvalidationError),
    Materialize(OidMaterializeError),
    DuplicateSymbol,
    MissingSymbolDependency {
        symbol: String,
        dependency: String,
    },
    UnknownFragment(String),
    StalePreparation {
        expected: String,
        observed: String,
    },
    DuplicateServiceLogicalName(String),
    DuplicateServiceIdentity(SourceId),
    MissingServiceRoots(String),
    EmptyServiceRoots(String),
    UnknownServiceRoot {
        service: String,
        symbol: String,
    },
    UnboundServiceRoot {
        service: String,
        symbol: String,
    },
    ServiceSourceMismatch {
        service: String,
        symbol: String,
        expected: String,
        observed: String,
    },
}

impl fmt::Display for RelNativeLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "linked REL OID cache failed: {error}"),
            Self::Bridge(error) => write!(formatter, "linked REL OID allocation failed: {error}"),
            Self::Invalidation(error) => {
                write!(formatter, "linked REL Service cache invalidation failed: {error}")
            }
            Self::Materialize(error) => {
                write!(formatter, "linked REL OID materialization failed: {error}")
            }
            Self::DuplicateSymbol => {
                formatter.write_str("linked REL discovery contains duplicate canonical symbols")
            }
            Self::MissingSymbolDependency { symbol, dependency } => write!(
                formatter,
                "linked REL symbol {symbol:?} depends on unreachable/unknown symbol {dependency:?}"
            ),
            Self::UnknownFragment(symbol) => write!(
                formatter,
                "native lowering produced a fragment for unlinked REL symbol {symbol:?}"
            ),
            Self::StalePreparation { expected, observed } => write!(
                formatter,
                "linked REL preparation targeted OID index {expected}, but live index is now {observed}; prepare again"
            ),
            Self::DuplicateServiceLogicalName(service) => {
                write!(formatter, "native Service input repeats logical Service {service:?}")
            }
            Self::DuplicateServiceIdentity(service) => {
                write!(formatter, "native Service input repeats Runtime Image identity {service}")
            }
            Self::MissingServiceRoots(service) => write!(
                formatter,
                "Service {service:?} has no discovered linked-REL root set"
            ),
            Self::EmptyServiceRoots(service) => write!(
                formatter,
                "Service {service:?} has an empty linked-REL root set and cannot produce a native plan"
            ),
            Self::UnknownServiceRoot { service, symbol } => write!(
                formatter,
                "Service {service:?} references unknown linked-REL root {symbol:?}"
            ),
            Self::UnboundServiceRoot { service, symbol } => write!(
                formatter,
                "Service {service:?} root {symbol:?} has no allocated linked-REL OID"
            ),
            Self::ServiceSourceMismatch {
                service,
                symbol,
                expected,
                observed,
            } => write!(
                formatter,
                "Service {service:?} root {symbol:?} belongs to source {observed}, but Service link pins {expected}"
            ),
        }
    }
}

impl std::error::Error for RelNativeLinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(ch: char) -> String {
        std::iter::repeat(ch).take(64).collect()
    }

    fn symbol(id: &str, kind: LinkedRelKind, deps: &[&str]) -> LinkedRelSymbolSpec {
        LinkedRelSymbolSpec {
            canonical_id: id.into(),
            kind,
            source_sha256: sha('a'),
            required_symbols: deps.iter().map(|value| (*value).to_string()).collect(),
            capabilities: BTreeSet::new(),
        }
    }

    #[test]
    fn recursion_is_not_a_sparse_record_self_dependency() {
        let normalized = normalize_symbols(&[symbol(
            "service_loop_again",
            LinkedRelKind::ServiceExport,
            &["service_loop_again"],
        )])
        .unwrap();
        assert!(normalized[0].required_symbols.is_empty());
    }

    #[test]
    fn dependency_must_survive_reachability() {
        let error = normalize_symbols(&[symbol(
            "service_worker_run",
            LinkedRelKind::ServiceExport,
            &["module_missing_call"],
        )])
        .unwrap_err();
        assert!(matches!(
            error,
            RelNativeLinkError::MissingSymbolDependency { .. }
        ));
    }

    #[test]
    fn class_descriptor_fragment_is_synthesized_but_executable_is_not() {
        let symbols = vec![
            symbol("class_Cache", LinkedRelKind::Class, &["method_Cache_get"]),
            symbol("method_Cache_get", LinkedRelKind::Method, &[]),
        ];
        let fragments = BTreeMap::from([(
            "method_Cache_get".into(),
            NativeOidFragment::executable(vec![0xC3]),
        )]);
        let effective = effective_fragments(&symbols, &fragments).unwrap();
        assert!(effective.contains_key("class_Cache"));
        assert!(effective["class_Cache"].machine_code.is_empty());
        assert_eq!(effective["method_Cache_get"].machine_code, vec![0xC3]);
    }
}
