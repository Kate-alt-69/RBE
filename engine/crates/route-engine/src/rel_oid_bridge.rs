//! Phase 4 linked-REL allocation against the single project-local OID index.
//!
//! The physical v1 index stores canonical REL symbol -> OID bindings. Rich
//! symbol metadata remains in RELC/OID records; this bridge therefore follows a
//! conservative active-pin rule: an unpinned canonical binding may keep its OID,
//! while a pinned binding is rebound to a fresh OID so an old Runtime Image can
//! keep executing its exact sparse record unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::oid_link::{reconcile_rel_oids, LinkedRelBinding, LinkedRelSymbolSpec, OidLinkError};
use crate::service_native::DynamicOidPinRegistry;
use crate::service_oid::{OidCache, OidError, OidIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelIndexDelta {
    pub previous_generation: u64,
    pub generation: u64,
    /// OIDs whose canonical linked-symbol ownership changed.
    pub changed_oids: BTreeSet<u16>,
    /// Old bindings removed from the current image. Their sparse record may
    /// remain on disk while Phase 5 still pins the OID for an older image.
    pub retired_oids: BTreeSet<u16>,
    /// Every reachable desired REL OID should pass through Phase-2 record
    /// materialization. `OidCache::write_record_if_changed` decides whether the
    /// encoded record hash actually changed.
    pub materialize_oids: BTreeSet<u16>,
    pub rebound_symbols: BTreeSet<String>,
}

impl RelIndexDelta {
    pub fn changed(&self) -> bool {
        self.previous_generation != self.generation
    }

    pub fn affects_required_oids(&self, required: impl IntoIterator<Item = u16>) -> bool {
        required
            .into_iter()
            .any(|oid| self.changed_oids.contains(&oid))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelReconcileReport {
    pub delta: RelIndexDelta,
    /// Rich in-memory bindings used by RELC to build class/function records and
    /// required-OID graphs. The physical index keeps the canonical -> numeric
    /// binding; record hashes and kind metadata stay in sparse OID records.
    pub bindings: BTreeMap<String, LinkedRelBinding>,
}

pub fn reconcile_rel_index(
    index: &mut OidIndex,
    desired: &[LinkedRelSymbolSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<RelReconcileReport, RelOidBridgeError> {
    index.validate_structure().map_err(RelOidBridgeError::Oid)?;
    let previous_generation = index.generation;

    let desired_by_id = desired
        .iter()
        .map(|spec| (spec.canonical_id.clone(), spec))
        .collect::<BTreeMap<_, _>>();
    if desired_by_id.len() != desired.len() {
        return Err(RelOidBridgeError::DuplicateDesiredSymbol);
    }

    // Reconstruct only bindings that are safe to preserve. For an unpinned
    // symbol, the desired metadata is sufficient to retain the numeric slot;
    // Phase 2 will regenerate/compare its sparse record. A pinned slot is not
    // offered as a preservation candidate, which forces a new OID while the old
    // image's exact record remains live.
    let mut current = BTreeMap::new();
    for (symbol, spec) in &desired_by_id {
        let Some(&oid) = index.rel_bindings.get(symbol) else {
            continue;
        };
        if pinned_oids.contains(&oid) {
            continue;
        }
        current.insert(
            symbol.clone(),
            LinkedRelBinding {
                canonical_id: symbol.clone(),
                kind: spec.kind,
                oid,
                source_sha256: spec.source_sha256.clone(),
                identity_sha256: spec
                    .identity_sha256()
                    .map_err(RelOidBridgeError::LinkPolicy)?,
                required_symbols: spec.required_symbols.clone(),
                capabilities: spec.capabilities.clone(),
            },
        );
    }

    let next = reconcile_rel_oids(&current, desired, pinned_oids)
        .map_err(RelOidBridgeError::LinkPolicy)?;
    let next_bindings = next
        .iter()
        .map(|(symbol, binding)| (symbol.clone(), binding.oid))
        .collect::<BTreeMap<_, _>>();

    let old_by_oid = invert_bindings(&index.rel_bindings)?;
    let new_by_oid = invert_bindings(&next_bindings)?;
    let mut all_oids = old_by_oid.keys().copied().collect::<BTreeSet<_>>();
    all_oids.extend(new_by_oid.keys().copied());

    let mut changed_oids = BTreeSet::new();
    let mut retired_oids = BTreeSet::new();
    let mut rebound_symbols = BTreeSet::new();
    for oid in all_oids {
        let old = old_by_oid.get(&oid);
        let new = new_by_oid.get(&oid);
        if old == new {
            continue;
        }
        changed_oids.insert(oid);
        if new.is_none() {
            retired_oids.insert(oid);
        }
        if let Some(symbol) = old {
            rebound_symbols.insert(symbol.clone());
        }
        if let Some(symbol) = new {
            rebound_symbols.insert(symbol.clone());
        }
    }

    if index.rel_bindings != next_bindings {
        let mut next_index = index.clone();
        next_index.rel_bindings = next_bindings;
        next_index.generation = next_index
            .generation
            .checked_add(1)
            .ok_or(RelOidBridgeError::GenerationExhausted)?;
        next_index
            .validate_structure()
            .map_err(RelOidBridgeError::Oid)?;
        *index = next_index;
    }

    let materialize_oids = next.values().map(|binding| binding.oid).collect();
    Ok(RelReconcileReport {
        delta: RelIndexDelta {
            previous_generation,
            generation: index.generation,
            changed_oids,
            retired_oids,
            materialize_oids,
            rebound_symbols,
        },
        bindings: next,
    })
}

pub fn reconcile_rel_cache(
    cache: &mut OidCache,
    desired: &[LinkedRelSymbolSpec],
    pinned_oids: &BTreeSet<u16>,
) -> Result<RelReconcileReport, RelOidBridgeError> {
    let mut next = cache.index().clone();
    let report = reconcile_rel_index(&mut next, desired, pinned_oids)?;
    if report.delta.changed() {
        cache.replace_index(next).map_err(RelOidBridgeError::Oid)?;
    }
    Ok(report)
}

/// Phase-5 safe linked-REL reconciliation. Prefer this at Runtime Image
/// boundaries so active Image/worker OID liveness cannot be omitted by callers.
pub fn reconcile_rel_cache_with_active_pins(
    cache: &mut OidCache,
    desired: &[LinkedRelSymbolSpec],
    active_pins: &DynamicOidPinRegistry,
) -> Result<RelReconcileReport, RelOidBridgeError> {
    let pinned_oids = active_pins.rel_allocator_pins();
    reconcile_rel_cache(cache, desired, &pinned_oids)
}

fn invert_bindings(
    bindings: &BTreeMap<String, u16>,
) -> Result<BTreeMap<u16, String>, RelOidBridgeError> {
    let mut out = BTreeMap::new();
    for (symbol, &oid) in bindings {
        if out.insert(oid, symbol.clone()).is_some() {
            return Err(RelOidBridgeError::DuplicateOid(oid));
        }
    }
    Ok(out)
}

#[derive(Debug)]
pub enum RelOidBridgeError {
    Oid(OidError),
    LinkPolicy(OidLinkError),
    DuplicateDesiredSymbol,
    DuplicateOid(u16),
    GenerationExhausted,
}

impl fmt::Display for RelOidBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "linked REL OID index update failed: {error}"),
            Self::LinkPolicy(error) => {
                write!(formatter, "linked REL OID reconciliation failed: {error}")
            }
            Self::DuplicateDesiredSymbol => write!(
                formatter,
                "linked REL input contains duplicate canonical symbols"
            ),
            Self::DuplicateOid(oid) => write!(
                formatter,
                "linked REL OID {oid} is bound to more than one canonical symbol"
            ),
            Self::GenerationExhausted => {
                write!(formatter, "OID index generation counter is exhausted")
            }
        }
    }
}

impl std::error::Error for RelOidBridgeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid_link::{rel_edges_by_oid, LinkedRelKind};
    use crate::service_oid::{OidIndex, OID_REL_START};

    fn sha(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
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
    fn clean_build_allocates_from_rel_range_start() {
        let mut index = OidIndex::fresh();
        let report = reconcile_rel_index(
            &mut index,
            &[symbol(
                "module_users_findUser",
                LinkedRelKind::ModuleExport,
                &[],
            )],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(index.rel_bindings["module_users_findUser"], OID_REL_START);
        assert_eq!(report.bindings["module_users_findUser"].oid, OID_REL_START);
    }

    #[test]
    fn unpinned_binding_keeps_its_numeric_oid() {
        let desired = vec![symbol(
            "module_users_findUser",
            LinkedRelKind::ModuleExport,
            &[],
        )];
        let mut index = OidIndex::fresh();
        reconcile_rel_index(&mut index, &desired, &BTreeSet::new()).unwrap();
        let oid = index.rel_bindings["module_users_findUser"];
        let generation = index.generation;

        let report = reconcile_rel_index(&mut index, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(index.rel_bindings["module_users_findUser"], oid);
        assert_eq!(index.generation, generation);
        assert!(!report.delta.changed());
    }

    #[test]
    fn active_pin_rebinds_same_canonical_symbol_instead_of_overwriting_old_record() {
        let desired = vec![symbol(
            "module_users_findUser",
            LinkedRelKind::ModuleExport,
            &[],
        )];
        let mut index = OidIndex::fresh();
        reconcile_rel_index(&mut index, &desired, &BTreeSet::new()).unwrap();
        let old_oid = index.rel_bindings["module_users_findUser"];

        let report = reconcile_rel_index(&mut index, &desired, &BTreeSet::from([old_oid])).unwrap();
        let new_oid = index.rel_bindings["module_users_findUser"];
        assert_ne!(old_oid, new_oid);
        assert!(report.delta.changed_oids.contains(&old_oid));
        assert!(report.delta.changed_oids.contains(&new_oid));
        assert!(report.delta.retired_oids.contains(&old_oid));
    }

    #[test]
    fn class_binding_graph_resolves_to_numeric_method_oids() {
        let desired = vec![
            symbol(
                "class_UserCache",
                LinkedRelKind::Class,
                &["ctor_UserCache", "method_UserCache_get"],
            ),
            symbol("ctor_UserCache", LinkedRelKind::Constructor, &[]),
            symbol("method_UserCache_get", LinkedRelKind::Method, &[]),
        ];
        let mut index = OidIndex::fresh();
        let report = reconcile_rel_index(&mut index, &desired, &BTreeSet::new()).unwrap();
        let edges = rel_edges_by_oid(&report.bindings).unwrap();
        let class_oid = report.bindings["class_UserCache"].oid;
        assert_eq!(edges[&class_oid].len(), 2);
        assert!(edges[&class_oid].contains(&report.bindings["ctor_UserCache"].oid));
        assert!(edges[&class_oid].contains(&report.bindings["method_UserCache_get"].oid));
    }
}
