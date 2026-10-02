//! Phase 4 adapter from Phase-1 sparse OID records to the native service assembler.
//!
//! This is the deliberately narrow boundary the design calls for: once RELC has
//! emitted `oid/index`, sparse `oid/<ID>` records and a service assembly plan,
//! assembly can proceed without a REL lexer/parser, package-name resolver, AST,
//! or `ModuleExecutor`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use atomic_io::AtomicIo;
use sha2::{Digest, Sha256};

use crate::service_bin::{
    assemble_service_bin, write_service_bin_atomic, AssembledServiceBin, AssemblyError,
    AssemblyRecordKind, DiagnosticSeverity, OidDiagnostic as AssemblyDiagnostic,
    OidRelocation as AssemblyRelocation, OidRelocationKind as AssemblyRelocationKind, RequiredOid,
    ServiceAssemblyPlan, SERVICE_PLAN_FORMAT,
};
use crate::service_oid::{
    OidCache, OidDiagnosticSeverity, OidError, OidIndex, OidRecord, OidRecordKind,
    OidRelocationKind, OID_DONE, OID_END_PACKAGE,
};

/// Natural hard bound from the v1 16-bit OID address space. The traversal also
/// keeps a visited set, so cycles cannot create unbounded plan growth.
pub const MAX_SERVICE_REQUIRED_OIDS: usize = (u16::MAX as usize) + 1;

pub fn oid_index_identity_sha256(index: &OidIndex) -> Result<String, ServiceOidAdapterError> {
    let encoded = index.to_bytes().map_err(ServiceOidAdapterError::Oid)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

pub fn oid_record_identity_sha256(record: &OidRecord) -> Result<String, ServiceOidAdapterError> {
    let encoded = record.to_bytes().map_err(ServiceOidAdapterError::Oid)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

/// Convert one already-verified Phase-1 record into the smaller assembler view.
///
/// Frame terminators are a property of the validated record kind/lowering ABI:
/// normal callable REL frames return through `0`, package-operation frames
/// return through `30457`, and descriptor/core records do not own a frame.
pub fn adapt_oid_record(
    record: &OidRecord,
) -> Result<crate::service_bin::VerifiedAssemblyOidRecord, ServiceOidAdapterError> {
    record.validate().map_err(ServiceOidAdapterError::Oid)?;
    let kind = assembly_kind(record.kind)?;
    let frame_terminator = match record.kind {
        OidRecordKind::PackageOperation => Some(OID_END_PACKAGE),
        OidRecordKind::Function
        | OidRecordKind::Method
        | OidRecordKind::Constructor
        | OidRecordKind::ModuleExport
        | OidRecordKind::RouteExport
        | OidRecordKind::ServiceExport
        | OidRecordKind::AsyncOperation
        | OidRecordKind::EventHandler => Some(OID_DONE),
        OidRecordKind::CoreOperation
        | OidRecordKind::Class
        | OidRecordKind::Capability
        | OidRecordKind::ObjectLayout => None,
        OidRecordKind::Sentinel => {
            return Err(ServiceOidAdapterError::SentinelInServicePlan(record.oid))
        }
    };

    let diagnostics = record
        .diagnostics
        .iter()
        .map(|diagnostic| AssemblyDiagnostic {
            severity: match diagnostic.severity {
                OidDiagnosticSeverity::Error => DiagnosticSeverity::Error,
                OidDiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
                OidDiagnosticSeverity::UnknownFatal => DiagnosticSeverity::UnknownFatal,
            },
            error_book_code: diagnostic.code.clone(),
        })
        .collect();

    let mut relocations = Vec::with_capacity(record.relocations.len());
    for relocation in &record.relocations {
        let kind = match relocation.kind {
            OidRelocationKind::Rel32ToOid => AssemblyRelocationKind::Rel32ToOid {
                target_oid: relocation.target_oid,
                addend: relocation.addend,
            },
            // Phase-1's compact v1 record uses `target_oid=0` for a service-data
            // base patch and stores the byte displacement in `addend`. Refuse
            // any other spelling rather than guessing at a data identity.
            OidRelocationKind::Abs64ToData if relocation.target_oid == 0 => {
                AssemblyRelocationKind::Abs64ToData {
                    data_offset: 0,
                    addend: relocation.addend,
                }
            }
            OidRelocationKind::Abs64ToData => {
                return Err(ServiceOidAdapterError::InvalidDataRelocation {
                    oid: record.oid,
                    offset: relocation.offset,
                    target_oid: relocation.target_oid,
                })
            }
            // These patch classes need target-specific metadata beyond the
            // current compact Phase-1 fields. Fail closed until their lowering
            // contract is explicit; never reinterpret bytes heuristically.
            OidRelocationKind::CallTarget
            | OidRelocationKind::ConstAddress
            | OidRelocationKind::JumpTable => {
                return Err(ServiceOidAdapterError::UnsupportedRelocation {
                    oid: record.oid,
                    offset: relocation.offset,
                    kind: relocation.kind,
                })
            }
        };
        relocations.push(AssemblyRelocation {
            offset: relocation.offset,
            kind,
        });
    }

    Ok(crate::service_bin::VerifiedAssemblyOidRecord {
        oid: record.oid,
        record_hash: oid_record_identity_sha256(record)?,
        kind,
        target_fingerprint: record.target.label(),
        alignment: u32::from(record.alignment),
        entry_offset: record.entry_offset,
        frame_terminator,
        diagnostics,
        machine_code: record.machine_code.clone(),
        relocations,
    })
}

/// Build an exact service plan by traversing only `required_oids` from the
/// supplied entry OIDs. Unreachable package/functions/classes never enter the
/// plan and therefore cannot invalidate its final service binary.
pub fn build_service_plan_from_oid_cache(
    cache: &OidCache,
    service_identity: impl Into<String>,
    service_source_sha256: impl Into<String>,
    entry_oids: Vec<u16>,
    service_data: Vec<u8>,
    dependency_hashes: BTreeMap<String, String>,
    compile_options: BTreeMap<String, String>,
) -> Result<ServiceAssemblyPlan, ServiceOidAdapterError> {
    if entry_oids.is_empty() {
        return Err(ServiceOidAdapterError::NoEntryOids);
    }

    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    for oid in &entry_oids {
        if seen.insert(*oid) {
            queue.push_back(*oid);
        }
    }

    let mut required = BTreeMap::<u16, RequiredOid>::new();
    let mut graph = BTreeMap::<u16, BTreeSet<u16>>::new();
    while let Some(oid) = queue.pop_front() {
        if seen.len() > MAX_SERVICE_REQUIRED_OIDS {
            return Err(ServiceOidAdapterError::RequiredOidLimitExceeded {
                observed: seen.len(),
                maximum: MAX_SERVICE_REQUIRED_OIDS,
            });
        }
        let record = cache.read_record(oid).map_err(ServiceOidAdapterError::Oid)?;
        let adapted = adapt_oid_record(&record)?;
        let dependencies = record.required_oids.iter().copied().collect::<BTreeSet<_>>();
        graph.insert(oid, dependencies.clone());
        required.insert(
            oid,
            RequiredOid {
                oid,
                record_hash: adapted.record_hash,
                kind: adapted.kind,
            },
        );
        for dependency in dependencies {
            if seen.insert(dependency) {
                queue.push_back(dependency);
            }
        }
    }

    let placement_order = required.keys().copied().collect::<Vec<_>>();
    let plan = ServiceAssemblyPlan {
        format: SERVICE_PLAN_FORMAT,
        service_identity: service_identity.into(),
        service_source_sha256: service_source_sha256.into(),
        index_identity_sha256: oid_index_identity_sha256(cache.index())?,
        target_fingerprint: cache.index().target.label(),
        entry_oids,
        required_oids: required.into_values().collect(),
        placement_order,
        call_graph: graph,
        service_data,
        data_alignment: 8,
        dependency_hashes,
        compile_options,
    };
    plan.validate().map_err(ServiceOidAdapterError::Assembly)?;
    Ok(plan)
}

pub fn service_plan_path(
    project_root: &Path,
    plan_hash: &str,
) -> Result<PathBuf, ServiceOidAdapterError> {
    validate_hash(plan_hash, "service plan")?;
    Ok(project_root
        .join(".cache/compiler/service/plan")
        .join(plan_hash))
}

pub fn write_service_plan_atomic(
    project_root: &Path,
    plan: &ServiceAssemblyPlan,
) -> Result<(String, PathBuf), ServiceOidAdapterError> {
    let plan_hash = plan.plan_hash().map_err(ServiceOidAdapterError::Assembly)?;
    let path = service_plan_path(project_root, &plan_hash)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ServiceOidAdapterError::Io)?;
    }
    let bytes = serde_json::to_vec(plan)
        .map_err(|error| ServiceOidAdapterError::PlanSerialization(error.to_string()))?;
    AtomicIo::new()
        .write_atomic(&path, &bytes)
        .map_err(ServiceOidAdapterError::Io)?;
    Ok((plan_hash, path))
}

pub fn read_service_plan(
    project_root: &Path,
    expected_hash: &str,
) -> Result<ServiceAssemblyPlan, ServiceOidAdapterError> {
    let path = service_plan_path(project_root, expected_hash)?;
    let bytes = fs::read(&path).map_err(ServiceOidAdapterError::Io)?;
    let plan: ServiceAssemblyPlan = serde_json::from_slice(&bytes)
        .map_err(|error| ServiceOidAdapterError::PlanSerialization(error.to_string()))?;
    let observed = plan.plan_hash().map_err(ServiceOidAdapterError::Assembly)?;
    if observed != expected_hash {
        return Err(ServiceOidAdapterError::PlanHashMismatch {
            expected: expected_hash.to_string(),
            observed,
        });
    }
    Ok(plan)
}

/// Assemble strictly from an already-produced plan plus sparse OID records.
/// No REL source is opened here. Exact record hashes pinned by the plan are
/// rechecked by `assemble_service_bin`, so an arbitrary newer OID meaning cannot
/// leak into an older Runtime Image.
pub fn assemble_service_from_oid_cache(
    project_root: &Path,
    cache: &OidCache,
    plan: &ServiceAssemblyPlan,
) -> Result<(AssembledServiceBin, PathBuf), ServiceOidAdapterError> {
    plan.validate().map_err(ServiceOidAdapterError::Assembly)?;
    let current_index_hash = oid_index_identity_sha256(cache.index())?;
    if current_index_hash != plan.index_identity_sha256 {
        return Err(ServiceOidAdapterError::IndexHashMismatch {
            expected: plan.index_identity_sha256.clone(),
            observed: current_index_hash,
        });
    }
    let target = cache.index().target.label();
    if target != plan.target_fingerprint {
        return Err(ServiceOidAdapterError::TargetMismatch {
            expected: plan.target_fingerprint.clone(),
            observed: target,
        });
    }

    let mut records = BTreeMap::new();
    for required in &plan.required_oids {
        let record = cache
            .read_record(required.oid)
            .map_err(ServiceOidAdapterError::Oid)?;
        records.insert(required.oid, adapt_oid_record(&record)?);
    }
    let bin = assemble_service_bin(plan, &records).map_err(ServiceOidAdapterError::Assembly)?;
    let compiler_root = project_root.join(".cache/compiler");
    fs::create_dir_all(compiler_root.join("service/bytecode"))
        .map_err(ServiceOidAdapterError::Io)?;
    let path = write_service_bin_atomic(&AtomicIo::new(), &compiler_root, &bin)
        .map_err(ServiceOidAdapterError::Assembly)?;
    Ok((bin, path))
}

fn assembly_kind(kind: OidRecordKind) -> Result<AssemblyRecordKind, ServiceOidAdapterError> {
    match kind {
        OidRecordKind::CoreOperation => Ok(AssemblyRecordKind::CoreOperation),
        OidRecordKind::Function => Ok(AssemblyRecordKind::Function),
        OidRecordKind::PackageOperation => Ok(AssemblyRecordKind::PackageOperation),
        OidRecordKind::ModuleExport => Ok(AssemblyRecordKind::ModuleExport),
        OidRecordKind::RouteExport => Ok(AssemblyRecordKind::RouteExport),
        OidRecordKind::ServiceExport => Ok(AssemblyRecordKind::ServiceExport),
        OidRecordKind::Class => Ok(AssemblyRecordKind::Class),
        OidRecordKind::Method => Ok(AssemblyRecordKind::ClassMethod),
        OidRecordKind::Constructor => Ok(AssemblyRecordKind::Constructor),
        OidRecordKind::ObjectLayout => Ok(AssemblyRecordKind::ObjectLayout),
        OidRecordKind::Capability => Ok(AssemblyRecordKind::CapabilityStub),
        OidRecordKind::AsyncOperation => Ok(AssemblyRecordKind::AsyncEntry),
        OidRecordKind::EventHandler => Ok(AssemblyRecordKind::EventHandler),
        OidRecordKind::Sentinel => Err(ServiceOidAdapterError::SentinelInServicePlan(0)),
    }
}

fn validate_hash(value: &str, label: &'static str) -> Result<(), ServiceOidAdapterError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ServiceOidAdapterError::InvalidHash {
            label,
            value: value.to_string(),
        })
    }
}

#[derive(Debug)]
pub enum ServiceOidAdapterError {
    Oid(OidError),
    Assembly(AssemblyError),
    Io(std::io::Error),
    NoEntryOids,
    SentinelInServicePlan(u16),
    InvalidDataRelocation {
        oid: u16,
        offset: u32,
        target_oid: u16,
    },
    UnsupportedRelocation {
        oid: u16,
        offset: u32,
        kind: OidRelocationKind,
    },
    RequiredOidLimitExceeded {
        observed: usize,
        maximum: usize,
    },
    PlanSerialization(String),
    InvalidHash {
        label: &'static str,
        value: String,
    },
    PlanHashMismatch {
        expected: String,
        observed: String,
    },
    IndexHashMismatch {
        expected: String,
        observed: String,
    },
    TargetMismatch {
        expected: String,
        observed: String,
    },
}

impl fmt::Display for ServiceOidAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oid(error) => write!(formatter, "OID cache verification failed: {error}"),
            Self::Assembly(error) => write!(formatter, "service assembly failed: {error}"),
            Self::Io(error) => write!(formatter, "service plan cache I/O failed: {error}"),
            Self::NoEntryOids => write!(formatter, "service plan requires at least one entry OID"),
            Self::SentinelInServicePlan(oid) => write!(formatter, "sentinel OID {oid} cannot be copied as a service dependency fragment"),
            Self::InvalidDataRelocation { oid, offset, target_oid } => write!(formatter, "OID {oid} data relocation at byte {offset} uses unsupported target OID {target_oid}"),
            Self::UnsupportedRelocation { oid, offset, kind } => write!(formatter, "OID {oid} relocation {kind:?} at byte {offset} is not representable by the Phase-4 assembler yet"),
            Self::RequiredOidLimitExceeded { observed, maximum } => write!(formatter, "service dependency closure contains {observed} OIDs; maximum is {maximum}"),
            Self::PlanSerialization(message) => write!(formatter, "service plan serialization failed: {message}"),
            Self::InvalidHash { label, value } => write!(formatter, "invalid {label} SHA-256 {value:?}"),
            Self::PlanHashMismatch { expected, observed } => write!(formatter, "service plan hash mismatch: expected {expected}, observed {observed}"),
            Self::IndexHashMismatch { expected, observed } => write!(formatter, "OID index identity mismatch: expected {expected}, observed {observed}"),
            Self::TargetMismatch { expected, observed } => write!(formatter, "service plan target mismatch: expected {expected}, observed {observed}"),
        }
    }
}

impl std::error::Error for ServiceOidAdapterError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_oid::{
        OidDiagnostic, OidRelocation, OidTarget, OID_NATIVE_ABI_VERSION,
        OID_RECORD_FORMAT_VERSION,
    };

    fn function_record(oid: u16, required_oids: Vec<u16>) -> OidRecord {
        OidRecord {
            format_version: OID_RECORD_FORMAT_VERSION,
            native_abi_version: OID_NATIVE_ABI_VERSION,
            oid,
            kind: OidRecordKind::Function,
            flags: 0,
            target: OidTarget::current(),
            name: format!("fn_{oid}"),
            entry_offset: 0,
            alignment: 1,
            required_oids,
            relocations: Vec::new(),
            diagnostics: Vec::new(),
            machine_code: vec![0xC3],
        }
    }

    #[test]
    fn record_hash_is_stable_for_identical_encoded_record() {
        let record = function_record(30_458, Vec::new());
        let first = oid_record_identity_sha256(&record).unwrap();
        let second = oid_record_identity_sha256(&record).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn adapter_preserves_error_book_diagnostic_control() {
        let mut record = function_record(30_458, Vec::new());
        record.diagnostics.push(OidDiagnostic {
            severity: OidDiagnosticSeverity::Warning,
            code: "RELC2000".into(),
        });
        let adapted = adapt_oid_record(&record).unwrap();
        assert_eq!(adapted.diagnostics.len(), 1);
        assert_eq!(adapted.diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(adapted.diagnostics[0].error_book_code, "RELC2000");
        assert_eq!(adapted.frame_terminator, Some(OID_DONE));
    }

    #[test]
    fn package_record_uses_end_package_terminator() {
        let mut record = function_record(20_086, Vec::new());
        record.kind = OidRecordKind::PackageOperation;
        let adapted = adapt_oid_record(&record).unwrap();
        assert_eq!(adapted.frame_terminator, Some(OID_END_PACKAGE));
    }

    #[test]
    fn ambiguous_patch_kinds_fail_closed() {
        let mut record = function_record(30_458, vec![30_459]);
        record.relocations.push(OidRelocation {
            kind: OidRelocationKind::CallTarget,
            offset: 0,
            target_oid: 30_459,
            addend: 0,
        });
        assert!(matches!(
            adapt_oid_record(&record),
            Err(ServiceOidAdapterError::UnsupportedRelocation { .. })
        ));
    }
}
