//! Phase 4 semantic binding for native Service plans.
//!
//! Sparse OID record hashes bind executable bytes/kind/relocations, while a
//! linked REL symbol identity also includes compiler semantics such as source
//! identity, exact linked dependencies and Runtime Image capability identity.
//! A Service plan therefore needs both. This wrapper injects only the transitive
//! linked symbols reachable from each Service root into `dependency_hashes`
//! before the normal Phase-4 commit builds Phase-5 `NativeServiceBuildSpec`s.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;

use crate::oid_materialize::NativeOidFragment;
use crate::rel_native_link::{
    commit_rel_native_link, PreparedRelNativeLink, RelNativeLinkError, RelNativeLinkReport,
    ServiceNativeLinkInput,
};
use crate::service_cache_invalidation::{
    invalidate_service_cache_for_oids, ServiceCacheInvalidationReport, ServiceCacheProtection,
};
use crate::service_oid::OidCache;

const REL_SEMANTIC_DEPENDENCY_PREFIX: &str = "rel-semantic/";

/// Commit a prepared linked-REL generation after binding every Service plan to
/// the exact transitive REL symbol identities it consumes.
///
/// This is intentionally separate from authorization. Capability fingerprints
/// are already part of `LinkedRelBinding::identity_sha256`; Runtime Image stays
/// the authority that validates and lowers actual grants.
///
/// The low-level linker invalidates ownership-rebound OIDs before publication.
/// After materialization we make a second selective pass over the OIDs whose
/// *encoded sparse record bytes actually changed*. This closes the important
/// same-OID case: source/capability/native-code changes can keep a numeric OID,
/// but every unpinned Service plan/bin that consumed the old record hash is
/// still stale and must be removed before this guarded transaction returns.
pub fn commit_rel_native_link_with_semantic_dependencies(
    project_root: &Path,
    cache: &mut OidCache,
    prepared: PreparedRelNativeLink,
    fragments: &BTreeMap<String, NativeOidFragment>,
    services: &[ServiceNativeLinkInput],
    protection: &ServiceCacheProtection,
) -> Result<RelNativeLinkReport, RelNativeSemanticError> {
    let services = bind_service_semantic_dependencies(&prepared, services)?;
    let mut report = commit_rel_native_link(
        project_root,
        cache,
        prepared,
        fragments,
        &services,
        protection,
    )
    .map_err(RelNativeSemanticError::Link)?;

    let record_invalidation = invalidate_service_cache_for_oids(
        project_root,
        report.materialization.changed_oids.iter().copied(),
        protection,
    )
    .map_err(|error| RelNativeSemanticError::Link(RelNativeLinkError::Invalidation(error)))?;
    merge_invalidation(&mut report.invalidation, record_invalidation);
    Ok(report)
}

/// Return cloned Service inputs with deterministic `rel-semantic/<canonical>`
/// hashes added for exactly the symbols reachable from each Service's roots.
/// Unrelated linked symbols are excluded so their changes do not invalidate the
/// Service plan or disposable `.bin`.
pub fn bind_service_semantic_dependencies(
    prepared: &PreparedRelNativeLink,
    services: &[ServiceNativeLinkInput],
) -> Result<Vec<ServiceNativeLinkInput>, RelNativeSemanticError> {
    let symbols = prepared
        .normalized_symbols()
        .iter()
        .map(|symbol| (symbol.canonical_id.as_str(), symbol))
        .collect::<BTreeMap<_, _>>();

    let mut out = Vec::with_capacity(services.len());
    for service in services {
        let roots = prepared
            .service_roots()
            .get(&service.logical_name)
            .ok_or_else(|| {
                RelNativeSemanticError::MissingServiceRoots(service.logical_name.clone())
            })?;
        if roots.is_empty() {
            return Err(RelNativeSemanticError::EmptyServiceRoots(
                service.logical_name.clone(),
            ));
        }

        let reachable = transitive_symbols(roots, &symbols)?;
        let mut service = service.clone();
        for canonical_id in reachable {
            let binding = prepared
                .bindings()
                .get(&canonical_id)
                .ok_or_else(|| RelNativeSemanticError::MissingBinding(canonical_id.clone()))?;
            let key = format!("{REL_SEMANTIC_DEPENDENCY_PREFIX}{canonical_id}");
            if let Some(existing) = service.dependency_hashes.get(&key) {
                if existing != &binding.identity_sha256 {
                    return Err(RelNativeSemanticError::DependencyCollision {
                        service: service.logical_name.clone(),
                        key,
                        caller_hash: existing.clone(),
                        rel_hash: binding.identity_sha256.clone(),
                    });
                }
                continue;
            }
            service
                .dependency_hashes
                .insert(key, binding.identity_sha256.clone());
        }
        out.push(service);
    }
    Ok(out)
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

fn transitive_symbols(
    roots: &BTreeSet<String>,
    symbols: &BTreeMap<&str, &crate::oid_link::LinkedRelSymbolSpec>,
) -> Result<BTreeSet<String>, RelNativeSemanticError> {
    let mut reachable = BTreeSet::new();
    let mut queue = roots.iter().cloned().collect::<VecDeque<_>>();
    while let Some(canonical_id) = queue.pop_front() {
        if !reachable.insert(canonical_id.clone()) {
            continue;
        }
        let symbol = symbols
            .get(canonical_id.as_str())
            .ok_or_else(|| RelNativeSemanticError::UnknownSymbol(canonical_id.clone()))?;
        for dependency in &symbol.required_symbols {
            if !reachable.contains(dependency) {
                queue.push_back(dependency.clone());
            }
        }
    }
    Ok(reachable)
}

#[derive(Debug)]
pub enum RelNativeSemanticError {
    Link(RelNativeLinkError),
    MissingServiceRoots(String),
    EmptyServiceRoots(String),
    UnknownSymbol(String),
    MissingBinding(String),
    DependencyCollision {
        service: String,
        key: String,
        caller_hash: String,
        rel_hash: String,
    },
}

impl fmt::Display for RelNativeSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(error) => write!(formatter, "linked REL native commit failed: {error}"),
            Self::MissingServiceRoots(service) => write!(
                formatter,
                "Service {service:?} has no linked-REL root set for semantic plan binding"
            ),
            Self::EmptyServiceRoots(service) => write!(
                formatter,
                "Service {service:?} has an empty linked-REL root set for semantic plan binding"
            ),
            Self::UnknownSymbol(symbol) => write!(
                formatter,
                "linked REL semantic closure references unknown symbol {symbol:?}"
            ),
            Self::MissingBinding(symbol) => write!(
                formatter,
                "linked REL semantic symbol {symbol:?} has no prepared numeric binding"
            ),
            Self::DependencyCollision {
                service,
                key,
                caller_hash,
                rel_hash,
            } => write!(
                formatter,
                "Service {service:?} dependency key {key:?} was supplied as {caller_hash}, but RELC requires semantic hash {rel_hash}"
            ),
        }
    }
}

impl std::error::Error for RelNativeSemanticError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid_link::{LinkedRelKind, LinkedRelSymbolSpec};

    fn symbol(id: &str, deps: &[&str]) -> LinkedRelSymbolSpec {
        LinkedRelSymbolSpec {
            canonical_id: id.into(),
            kind: LinkedRelKind::Function,
            source_sha256: "a".repeat(64),
            required_symbols: deps.iter().map(|value| (*value).to_string()).collect(),
            capabilities: BTreeSet::new(),
        }
    }

    #[test]
    fn closure_contains_only_transitive_consumers() {
        let a = symbol("service_worker_run", &["module_used_call"]);
        let b = symbol("module_used_call", &[]);
        let c = symbol("module_unrelated_call", &[]);
        let symbols = BTreeMap::from([
            (a.canonical_id.as_str(), &a),
            (b.canonical_id.as_str(), &b),
            (c.canonical_id.as_str(), &c),
        ]);
        let roots = BTreeSet::from([a.canonical_id.clone()]);
        let reachable = transitive_symbols(&roots, &symbols).unwrap();
        assert_eq!(
            reachable,
            BTreeSet::from([a.canonical_id.clone(), b.canonical_id.clone()])
        );
        assert!(!reachable.contains(&c.canonical_id));
    }

    #[test]
    fn missing_transitive_symbol_fails_closed() {
        let a = symbol("service_worker_run", &["module_missing_call"]);
        let symbols = BTreeMap::from([(a.canonical_id.as_str(), &a)]);
        let roots = BTreeSet::from([a.canonical_id.clone()]);
        assert!(matches!(
            transitive_symbols(&roots, &symbols),
            Err(RelNativeSemanticError::UnknownSymbol(_))
        ));
    }

    #[test]
    fn invalidation_reports_merge_without_losing_pin_retention() {
        let mut current = ServiceCacheInvalidationReport {
            affected_oids: BTreeSet::from([30_458]),
            removed_plan_hashes: BTreeSet::from(["a".repeat(64)]),
            retained_assembly_hashes: BTreeSet::from(["b".repeat(64)]),
            skipped_unreadable_plans: 1,
            ..ServiceCacheInvalidationReport::default()
        };
        let additional = ServiceCacheInvalidationReport {
            affected_oids: BTreeSet::from([30_459]),
            removed_assembly_hashes: BTreeSet::from(["c".repeat(64)]),
            retained_plan_hashes: BTreeSet::from(["d".repeat(64)]),
            skipped_unreadable_bins: 2,
            ..ServiceCacheInvalidationReport::default()
        };
        merge_invalidation(&mut current, additional);
        assert_eq!(current.affected_oids, BTreeSet::from([30_458, 30_459]));
        assert_eq!(current.skipped_unreadable_plans, 1);
        assert_eq!(current.skipped_unreadable_bins, 2);
        assert_eq!(current.removed_plan_hashes.len(), 1);
        assert_eq!(current.removed_assembly_hashes.len(), 1);
        assert_eq!(current.retained_plan_hashes.len(), 1);
        assert_eq!(current.retained_assembly_hashes.len(), 1);
    }
}
