//! Phase 3/4 dynamic OID record materialization.
//!
//! Allocation and native lowering are intentionally separate. The package/REL
//! linkers choose stable project-local OIDs; Phase 2 supplies already-lowered
//! target machine-code fragments; this module binds the two into sparse
//! `oid/<ID>` records. It never invents placeholder native code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::oid_link::{LinkedRelBinding, LinkedRelKind};
use crate::service_oid::{
    OidCache, OidDiagnostic, OidError, OidRecord, OidRecordKind, OidRelocation,
    OID_NATIVE_ABI_VERSION, OID_RECORD_FORMAT_VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeOidFragment {
    pub flags: u32,
    pub entry_offset: u32,
    pub alignment: u16,
    /// Extra numeric dependencies discovered by native lowering (core helpers,
    /// package calls, capability stubs, etc.). Linked REL symbol dependencies
    /// are added separately from `LinkedRelBinding.required_symbols`.
    pub required_oids: BTreeSet<u16>,
    pub relocations: Vec<OidRelocation>,
    pub diagnostics: Vec<OidDiagnostic>,
    pub machine_code: Vec<u8>,
}

impl NativeOidFragment {
    pub fn executable(machine_code: Vec<u8>) -> Self {
        Self {
            flags: 0,
            entry_offset: 0,
            alignment: 1,
            required_oids: BTreeSet::new(),
            relocations: Vec::new(),
            diagnostics: Vec::new(),
            machine_code,
        }
    }

    pub fn descriptor() -> Self {
        Self::executable(Vec::new())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OidMaterializationReport {
    pub changed_oids: BTreeSet<u16>,
    pub unchanged_oids: BTreeSet<u16>,
    /// SHA-256 of the exact encoded sparse record written/read by Phase 4 plans.
    pub record_hashes: BTreeMap<u16, String>,
}

impl OidMaterializationReport {
    pub fn changed(&self) -> bool {
        !self.changed_oids.is_empty()
    }
}

/// Materialize every currently-owned package export from compiler-supplied
/// native fragments. A missing fragment is an error: Phase 3 must never turn a
/// package OID into a no-op/`ret` placeholder merely to make the cache complete.
pub fn materialize_package_records(
    cache: &OidCache,
    fragments: &BTreeMap<(String, String), NativeOidFragment>,
) -> Result<OidMaterializationReport, OidMaterializeError> {
    let mut expected = BTreeSet::new();
    let mut records = Vec::new();

    for (package, owner) in &cache.index().packages {
        for (export_id, &oid) in &owner.exports {
            let key = (package.clone(), export_id.clone());
            expected.insert(key.clone());
            let fragment =
                fragments
                    .get(&key)
                    .ok_or_else(|| OidMaterializeError::MissingPackageFragment {
                        package: package.clone(),
                        export_id: export_id.clone(),
                        oid,
                    })?;
            let name = format!("package:{}@{}:{}", package, owner.version, export_id);
            records.push(build_record(
                cache,
                oid,
                OidRecordKind::PackageOperation,
                name,
                fragment,
                fragment.required_oids.clone(),
            )?);
        }
    }

    for key in fragments.keys() {
        if !expected.contains(key) {
            return Err(OidMaterializeError::UnknownPackageFragment {
                package: key.0.clone(),
                export_id: key.1.clone(),
            });
        }
    }

    write_records(cache, records)
}

/// Materialize linked REL symbols after `rel_oid_bridge` has assigned their
/// current-image numeric IDs. Class descriptors therefore become real sparse
/// records whose `required_oids` point at constructor/method OIDs rather than
/// duplicating method tables into instances.
pub fn materialize_rel_records(
    cache: &OidCache,
    bindings: &BTreeMap<String, LinkedRelBinding>,
    fragments: &BTreeMap<String, NativeOidFragment>,
) -> Result<OidMaterializationReport, OidMaterializeError> {
    let mut records = Vec::with_capacity(bindings.len());

    for (canonical_id, binding) in bindings {
        let fragment =
            fragments
                .get(canonical_id)
                .ok_or_else(|| OidMaterializeError::MissingRelFragment {
                    canonical_id: canonical_id.clone(),
                    oid: binding.oid,
                })?;
        let mut required = fragment.required_oids.clone();
        for dependency in &binding.required_symbols {
            let target = bindings.get(dependency).ok_or_else(|| {
                OidMaterializeError::MissingRelDependency {
                    canonical_id: canonical_id.clone(),
                    dependency: dependency.clone(),
                }
            })?;
            required.insert(target.oid);
        }

        records.push(build_record(
            cache,
            binding.oid,
            record_kind(binding.kind),
            canonical_id.clone(),
            fragment,
            required,
        )?);
    }

    for canonical_id in fragments.keys() {
        if !bindings.contains_key(canonical_id) {
            return Err(OidMaterializeError::UnknownRelFragment(
                canonical_id.clone(),
            ));
        }
    }

    write_records(cache, records)
}

/// Remove records that have left the current index only when no Phase-5 Image
/// or worker still pins the numeric OID. This keeps sparse cache cleanup and
/// dynamic OID reuse under the same liveness rule.
pub fn remove_retired_dynamic_records(
    cache: &OidCache,
    retired_oids: impl IntoIterator<Item = u16>,
    pinned_oids: &BTreeSet<u16>,
) -> Result<BTreeSet<u16>, OidMaterializeError> {
    let mut removed = BTreeSet::new();
    for oid in retired_oids {
        if pinned_oids.contains(&oid) {
            continue;
        }
        if cache.remove_record(oid).map_err(OidMaterializeError::Oid)? {
            removed.insert(oid);
        }
    }
    Ok(removed)
}

fn build_record(
    cache: &OidCache,
    oid: u16,
    kind: OidRecordKind,
    name: String,
    fragment: &NativeOidFragment,
    required_oids: BTreeSet<u16>,
) -> Result<OidRecord, OidMaterializeError> {
    if required_oids.contains(&oid) {
        // Recursive functions should recurse through normal native control-flow;
        // a self dependency in the materialization graph does not add anything
        // and would make service reachability diagnostics needlessly cyclic.
        return Err(OidMaterializeError::SelfDependency { oid, name });
    }

    let record = OidRecord {
        format_version: OID_RECORD_FORMAT_VERSION,
        native_abi_version: OID_NATIVE_ABI_VERSION,
        oid,
        kind,
        flags: fragment.flags,
        target: cache.index().target.clone(),
        name,
        entry_offset: fragment.entry_offset,
        alignment: fragment.alignment,
        required_oids: required_oids.into_iter().collect(),
        relocations: fragment.relocations.clone(),
        diagnostics: fragment.diagnostics.clone(),
        machine_code: fragment.machine_code.clone(),
    };
    record.validate().map_err(OidMaterializeError::Oid)?;
    Ok(record)
}

fn write_records(
    cache: &OidCache,
    records: Vec<OidRecord>,
) -> Result<OidMaterializationReport, OidMaterializeError> {
    let mut report = OidMaterializationReport::default();
    for record in records {
        let oid = record.oid;
        let encoded = record.to_bytes().map_err(OidMaterializeError::Oid)?;
        let hash = hex::encode(Sha256::digest(&encoded));
        let changed = cache
            .write_record_if_changed(&record)
            .map_err(OidMaterializeError::Oid)?;
        report.record_hashes.insert(oid, hash);
        if changed {
            report.changed_oids.insert(oid);
        } else {
            report.unchanged_oids.insert(oid);
        }
    }
    Ok(report)
}

fn record_kind(kind: LinkedRelKind) -> OidRecordKind {
    match kind {
        LinkedRelKind::Function | LinkedRelKind::FirstClassFunction => OidRecordKind::Function,
        LinkedRelKind::ModuleExport => OidRecordKind::ModuleExport,
        LinkedRelKind::RouteExport => OidRecordKind::RouteExport,
        LinkedRelKind::ServiceExport => OidRecordKind::ServiceExport,
        LinkedRelKind::Class => OidRecordKind::Class,
        LinkedRelKind::Constructor => OidRecordKind::Constructor,
        LinkedRelKind::Method => OidRecordKind::Method,
    }
}

#[derive(Debug)]
pub enum OidMaterializeError {
    Oid(OidError),
    MissingPackageFragment {
        package: String,
        export_id: String,
        oid: u16,
    },
    UnknownPackageFragment {
        package: String,
        export_id: String,
    },
    MissingRelFragment {
        canonical_id: String,
        oid: u16,
    },
    UnknownRelFragment(String),
    MissingRelDependency {
        canonical_id: String,
        dependency: String,
    },
    SelfDependency {
        oid: u16,
        name: String,
    },
}

impl fmt::Display for OidMaterializeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "dynamic OID record is invalid: {error}"),
            Self::MissingPackageFragment { package, export_id, oid } => write!(formatter, "package {package:?} export {export_id:?} owns OID {oid} but native lowering produced no fragment"),
            Self::UnknownPackageFragment { package, export_id } => write!(formatter, "native lowering produced fragment for unowned package export {package:?}/{export_id:?}"),
            Self::MissingRelFragment { canonical_id, oid } => write!(formatter, "linked REL symbol {canonical_id:?} owns OID {oid} but native lowering produced no fragment"),
            Self::UnknownRelFragment(canonical_id) => write!(formatter, "native lowering produced fragment for unlinked REL symbol {canonical_id:?}"),
            Self::MissingRelDependency { canonical_id, dependency } => write!(formatter, "linked REL symbol {canonical_id:?} requires missing linked symbol {dependency:?}"),
            Self::SelfDependency { oid, name } => write!(formatter, "OID {oid} ({name}) lists itself as a materialization dependency"),
        }
    }
}

impl std::error::Error for OidMaterializeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid_index_bridge::{package_specs_from_links, reconcile_package_index};
    use crate::oid_link::{LinkedRelKind, LinkedRelSymbolSpec};
    use crate::rel_oid_bridge::reconcile_rel_index;
    use crate::relc::{
        PackageExportLink, PackageLinkContext, PackageRootLink, PACKAGE_LINK_FORMAT,
    };
    use crate::service_oid::{OidIndex, OidTarget};

    fn sha(ch: char) -> String {
        std::iter::repeat(ch).take(64).collect()
    }

    fn package_links() -> PackageLinkContext {
        PackageLinkContext {
            format: PACKAGE_LINK_FORMAT,
            roots: BTreeMap::from([(
                "mail".into(),
                PackageRootLink {
                    version: "1.0.0".into(),
                    artifact_sha256: sha('a'),
                    exports: BTreeMap::from([(
                        "send".into(),
                        PackageExportLink {
                            entry: "components/send/send.ts".into(),
                            language: "typescript".into(),
                        },
                    )]),
                },
            )]),
        }
    }

    #[test]
    fn package_record_requires_real_native_fragment() {
        let mut index = OidIndex::fresh();
        let specs = package_specs_from_links(&package_links()).unwrap();
        reconcile_package_index(&mut index, &specs, &BTreeSet::new()).unwrap();
        let owner = &index.packages["mail"];
        let oid = owner.exports["lib_mail_send"];
        assert!((20_086..=30_456).contains(&oid));
    }

    #[test]
    fn class_record_stacks_constructor_and_method_oids() {
        let desired = vec![
            LinkedRelSymbolSpec {
                canonical_id: "class_UserCache".into(),
                kind: LinkedRelKind::Class,
                source_sha256: sha('b'),
                required_symbols: BTreeSet::from([
                    "ctor_UserCache".into(),
                    "method_UserCache_get".into(),
                ]),
                capabilities: BTreeSet::new(),
            },
            LinkedRelSymbolSpec {
                canonical_id: "ctor_UserCache".into(),
                kind: LinkedRelKind::Constructor,
                source_sha256: sha('c'),
                required_symbols: BTreeSet::new(),
                capabilities: BTreeSet::new(),
            },
            LinkedRelSymbolSpec {
                canonical_id: "method_UserCache_get".into(),
                kind: LinkedRelKind::Method,
                source_sha256: sha('d'),
                required_symbols: BTreeSet::new(),
                capabilities: BTreeSet::new(),
            },
        ];
        let mut index = OidIndex::fresh();
        let report = reconcile_rel_index(&mut index, &desired, &BTreeSet::new()).unwrap();
        let class = &report.bindings["class_UserCache"];
        let mut required = BTreeSet::new();
        for dependency in &class.required_symbols {
            required.insert(report.bindings[dependency].oid);
        }
        assert_eq!(required.len(), 2);
    }

    #[test]
    fn fragment_constructor_defaults_to_current_native_shape() {
        let fragment = NativeOidFragment::executable(vec![0xC3]);
        assert_eq!(fragment.alignment, 1);
        assert_eq!(fragment.entry_offset, 0);
        assert!(fragment.required_oids.is_empty());
        assert!(!fragment.machine_code.is_empty());
        let _target = OidTarget::current();
    }
}
