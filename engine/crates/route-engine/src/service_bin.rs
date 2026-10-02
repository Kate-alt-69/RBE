//! Phase 4 service assembly from RELC-produced OID records.
//!
//! The assembler in this module is intentionally ignorant of REL syntax. It
//! consumes a compiler-produced service plan plus already-verified OID record
//! views, lays out current-target machine-code fragments, applies only the
//! relocations RELC described, and emits a disposable `.bin` cache artifact.

use crate::oid_link::END_PACKAGE_OID;
use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const SERVICE_PLAN_FORMAT: u32 = 1;
pub const SERVICE_BIN_FORMAT: u32 = 1;
pub const DONE_OID: u16 = 0;

const PLAN_HASH_DOMAIN: &[u8] = b"RBE_SERVICE_ASSEMBLY_PLAN_V1";
const BIN_HASH_DOMAIN: &[u8] = b"RBE_SERVICE_NATIVE_BIN_V1";
const BIN_MAGIC: &[u8; 8] = b"RBESBIN1";
const MAX_ALIGNMENT: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssemblyRecordKind {
    CoreOperation,
    Function,
    PackageOperation,
    ModuleExport,
    RouteExport,
    ServiceExport,
    Class,
    ClassMethod,
    Constructor,
    ObjectLayout,
    CapabilityStub,
    AsyncEntry,
    EventHandler,
}

impl AssemblyRecordKind {
    fn requires_done_terminator(self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::ModuleExport
                | Self::RouteExport
                | Self::ServiceExport
                | Self::ClassMethod
                | Self::Constructor
                | Self::AsyncEntry
                | Self::EventHandler
        )
    }

    fn descriptor_only(self) -> bool {
        matches!(self, Self::Class | Self::ObjectLayout)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredOid {
    pub oid: u16,
    pub record_hash: String,
    pub kind: AssemblyRecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAssemblyPlan {
    pub format: u32,
    pub service_identity: String,
    pub service_source_sha256: String,
    pub index_identity_sha256: String,
    pub target_fingerprint: String,
    pub entry_oids: Vec<u16>,
    pub required_oids: Vec<RequiredOid>,
    /// Exact deterministic fragment layout order. Must be a permutation of
    /// `required_oids` and therefore cannot smuggle an unpinned latest OID in.
    pub placement_order: Vec<u16>,
    #[serde(default)]
    pub call_graph: BTreeMap<u16, BTreeSet<u16>>,
    #[serde(default)]
    pub service_data: Vec<u8>,
    #[serde(default = "default_data_alignment")]
    pub data_alignment: u32,
    #[serde(default)]
    pub dependency_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub compile_options: BTreeMap<String, String>,
}

fn default_data_alignment() -> u32 {
    8
}

impl ServiceAssemblyPlan {
    pub fn validate(&self) -> Result<(), AssemblyError> {
        if self.format != SERVICE_PLAN_FORMAT {
            return Err(AssemblyError::UnsupportedPlanFormat(self.format));
        }
        if self.service_identity.is_empty() || self.service_identity.chars().any(char::is_control) {
            return Err(AssemblyError::InvalidServiceIdentity(
                self.service_identity.clone(),
            ));
        }
        validate_sha256(&self.service_source_sha256, "service source")?;
        validate_sha256(&self.index_identity_sha256, "OID index identity")?;
        if self.target_fingerprint.is_empty() || self.target_fingerprint.len() > 512 {
            return Err(AssemblyError::InvalidTargetFingerprint(
                self.target_fingerprint.clone(),
            ));
        }
        validate_alignment(self.data_alignment)?;

        let mut required = BTreeMap::new();
        for item in &self.required_oids {
            validate_sha256(&item.record_hash, "OID record")?;
            if required.insert(item.oid, item).is_some() {
                return Err(AssemblyError::DuplicateRequiredOid(item.oid));
            }
        }
        if required.is_empty() {
            return Err(AssemblyError::NoRequiredOids);
        }
        for &entry in &self.entry_oids {
            if !required.contains_key(&entry) {
                return Err(AssemblyError::EntryOidNotRequired(entry));
            }
        }
        if self.entry_oids.is_empty() {
            return Err(AssemblyError::NoEntryOids);
        }

        let placement = self.placement_order.iter().copied().collect::<BTreeSet<_>>();
        if placement.len() != self.placement_order.len()
            || placement != required.keys().copied().collect::<BTreeSet<_>>()
        {
            return Err(AssemblyError::InvalidPlacementOrder);
        }

        for (&caller, callees) in &self.call_graph {
            if !required.contains_key(&caller) {
                return Err(AssemblyError::GraphOidNotRequired(caller));
            }
            for &callee in callees {
                if !required.contains_key(&callee) {
                    return Err(AssemblyError::GraphOidNotRequired(callee));
                }
            }
        }
        for (name, hash) in &self.dependency_hashes {
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err(AssemblyError::InvalidDependencyIdentity(name.clone()));
            }
            validate_sha256(hash, "service dependency")?;
        }
        Ok(())
    }

    pub fn plan_hash(&self) -> Result<String, AssemblyError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)
            .map_err(|error| AssemblyError::Serialization(error.to_string()))?;
        let mut hash = Sha256::new();
        hash.update(PLAN_HASH_DOMAIN);
        hash.update((encoded.len() as u64).to_be_bytes());
        hash.update(encoded);
        Ok(hex::encode(hash.finalize()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    UnknownFatal,
}

impl DiagnosticSeverity {
    pub fn debug_marker(self) -> i8 {
        match self {
            Self::Error => -1,
            Self::Warning => -2,
            Self::UnknownFatal => -3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidDiagnostic {
    pub severity: DiagnosticSeverity,
    /// Existing RBE Error Book code. This module intentionally has no second
    /// diagnostic-code registry.
    pub error_book_code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidRelocation {
    /// Byte offset within this OID's machine-code fragment.
    pub offset: u32,
    pub kind: OidRelocationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OidRelocationKind {
    Rel32ToOid { target_oid: u16, addend: i64 },
    Abs64ToOid { target_oid: u16, addend: i64 },
    Abs64ToData { data_offset: u64, addend: i64 },
}

/// Adapter view produced by the Phase 1 OID-record verifier. The assembler
/// never reads REL source and never trusts an arbitrary record hash: the hash
/// must exactly match the one pinned in the service plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedAssemblyOidRecord {
    pub oid: u16,
    pub record_hash: String,
    pub kind: AssemblyRecordKind,
    pub target_fingerprint: String,
    pub alignment: u32,
    pub entry_offset: u32,
    /// Normal callable frames end with 0; package-operation frames end with
    /// 30457. Non-frame core snippets/descriptors use `None`.
    pub frame_terminator: Option<u16>,
    #[serde(default)]
    pub diagnostics: Vec<OidDiagnostic>,
    #[serde(default)]
    pub machine_code: Vec<u8>,
    #[serde(default)]
    pub relocations: Vec<OidRelocation>,
}

impl VerifiedAssemblyOidRecord {
    fn validate_for_plan(
        &self,
        expected: &RequiredOid,
        target: &str,
    ) -> Result<Vec<OidDiagnostic>, AssemblyError> {
        if self.oid != expected.oid {
            return Err(AssemblyError::RecordIdentityMismatch {
                expected: expected.oid,
                observed: self.oid,
            });
        }
        if self.record_hash != expected.record_hash {
            return Err(AssemblyError::RecordHashMismatch {
                oid: self.oid,
                expected: expected.record_hash.clone(),
                observed: self.record_hash.clone(),
            });
        }
        if self.kind != expected.kind {
            return Err(AssemblyError::RecordKindMismatch {
                oid: self.oid,
                expected: expected.kind,
                observed: self.kind,
            });
        }
        if self.target_fingerprint != target {
            return Err(AssemblyError::RecordTargetMismatch {
                oid: self.oid,
                expected: target.to_string(),
                observed: self.target_fingerprint.clone(),
            });
        }
        validate_alignment(self.alignment)?;
        if self.machine_code.is_empty() {
            if !self.kind.descriptor_only() {
                return Err(AssemblyError::MissingMachineCode(self.oid));
            }
            if self.entry_offset != 0 || !self.relocations.is_empty() {
                return Err(AssemblyError::InvalidDescriptorRecord(self.oid));
            }
        } else if self.entry_offset as usize >= self.machine_code.len() {
            return Err(AssemblyError::InvalidEntryOffset {
                oid: self.oid,
                offset: self.entry_offset,
                len: self.machine_code.len(),
            });
        }

        match self.kind {
            AssemblyRecordKind::PackageOperation => {
                if self.frame_terminator != Some(END_PACKAGE_OID) {
                    return Err(AssemblyError::InvalidFrameTerminator {
                        oid: self.oid,
                        kind: self.kind,
                        expected: Some(END_PACKAGE_OID),
                        observed: self.frame_terminator,
                    });
                }
            }
            kind if kind.requires_done_terminator() => {
                if self.frame_terminator != Some(DONE_OID) {
                    return Err(AssemblyError::InvalidFrameTerminator {
                        oid: self.oid,
                        kind: self.kind,
                        expected: Some(DONE_OID),
                        observed: self.frame_terminator,
                    });
                }
            }
            _ => {
                if self.frame_terminator.is_some() {
                    return Err(AssemblyError::InvalidFrameTerminator {
                        oid: self.oid,
                        kind: self.kind,
                        expected: None,
                        observed: self.frame_terminator,
                    });
                }
            }
        }

        let mut warnings = Vec::new();
        for diagnostic in &self.diagnostics {
            match diagnostic.severity {
                DiagnosticSeverity::Error | DiagnosticSeverity::UnknownFatal => {
                    return Err(AssemblyError::OidDiagnostic {
                        oid: self.oid,
                        severity: diagnostic.severity,
                        code: diagnostic.error_book_code.clone(),
                    })
                }
                DiagnosticSeverity::Warning => {
                    if self.machine_code.is_empty() {
                        return Err(AssemblyError::WarningWithoutExecutablePayload {
                            oid: self.oid,
                            code: diagnostic.error_book_code.clone(),
                        });
                    }
                    warnings.push(diagnostic.clone());
                }
            }
        }
        Ok(warnings)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidPlacement {
    pub start: usize,
    pub entry: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledServiceBin {
    pub target_fingerprint: String,
    pub plan_hash: String,
    pub assembly_hash: String,
    pub payload: Vec<u8>,
    pub placements: BTreeMap<u16, OidPlacement>,
    pub data_start: usize,
    pub warnings: Vec<(u16, OidDiagnostic)>,
}

pub fn assemble_service_bin(
    plan: &ServiceAssemblyPlan,
    records: &BTreeMap<u16, VerifiedAssemblyOidRecord>,
) -> Result<AssembledServiceBin, AssemblyError> {
    plan.validate()?;
    let expected = plan
        .required_oids
        .iter()
        .map(|item| (item.oid, item))
        .collect::<BTreeMap<_, _>>();

    let mut warnings = Vec::new();
    for required in &plan.required_oids {
        let record = records
            .get(&required.oid)
            .ok_or(AssemblyError::MissingOidRecord(required.oid))?;
        for warning in record.validate_for_plan(required, &plan.target_fingerprint)? {
            warnings.push((required.oid, warning));
        }
    }

    let mut payload = Vec::new();
    let mut placements = BTreeMap::new();
    for &oid in &plan.placement_order {
        let required = expected
            .get(&oid)
            .expect("plan validation proves placement OID is required");
        let record = records
            .get(&required.oid)
            .expect("record existence verified above");
        align_vec(&mut payload, record.alignment as usize);
        let start = payload.len();
        payload.extend_from_slice(&record.machine_code);
        placements.insert(
            oid,
            OidPlacement {
                start,
                entry: start + record.entry_offset as usize,
                len: record.machine_code.len(),
            },
        );
    }

    align_vec(&mut payload, plan.data_alignment as usize);
    let data_start = payload.len();
    payload.extend_from_slice(&plan.service_data);

    for &oid in &plan.placement_order {
        let record = records
            .get(&oid)
            .expect("record existence verified above");
        let placement = placements
            .get(&oid)
            .expect("placement created for every required OID");
        for relocation in &record.relocations {
            apply_relocation(
                &mut payload,
                placement,
                relocation,
                &placements,
                data_start,
                plan.service_data.len(),
            )?;
        }
    }

    let plan_hash = plan.plan_hash()?;
    let assembly_hash = compute_assembly_hash(&plan_hash, &payload)?;
    Ok(AssembledServiceBin {
        target_fingerprint: plan.target_fingerprint.clone(),
        plan_hash,
        assembly_hash,
        payload,
        placements,
        data_start,
        warnings,
    })
}

fn apply_relocation(
    payload: &mut [u8],
    owner: &OidPlacement,
    relocation: &OidRelocation,
    placements: &BTreeMap<u16, OidPlacement>,
    data_start: usize,
    data_len: usize,
) -> Result<(), AssemblyError> {
    let patch = owner
        .start
        .checked_add(relocation.offset as usize)
        .ok_or(AssemblyError::RelocationOverflow)?;
    match relocation.kind {
        OidRelocationKind::Rel32ToOid { target_oid, addend } => {
            let target = placements
                .get(&target_oid)
                .ok_or(AssemblyError::RelocationTargetMissing(target_oid))?;
            ensure_patch_bounds(owner, patch, 4)?;
            let next = patch.checked_add(4).ok_or(AssemblyError::RelocationOverflow)?;
            let target = add_signed(target.entry, addend)?;
            let delta = (target as i128) - (next as i128);
            let delta = i32::try_from(delta).map_err(|_| AssemblyError::Rel32OutOfRange {
                target_oid,
                delta,
            })?;
            payload[patch..patch + 4].copy_from_slice(&delta.to_le_bytes());
        }
        OidRelocationKind::Abs64ToOid { target_oid, addend } => {
            let target = placements
                .get(&target_oid)
                .ok_or(AssemblyError::RelocationTargetMissing(target_oid))?;
            ensure_patch_bounds(owner, patch, 8)?;
            let absolute = add_signed(target.entry, addend)?;
            let absolute = u64::try_from(absolute).map_err(|_| AssemblyError::RelocationOverflow)?;
            payload[patch..patch + 8].copy_from_slice(&absolute.to_le_bytes());
        }
        OidRelocationKind::Abs64ToData { data_offset, addend } => {
            let data_offset = usize::try_from(data_offset)
                .map_err(|_| AssemblyError::RelocationOverflow)?;
            if data_offset > data_len {
                return Err(AssemblyError::DataRelocationOutOfBounds {
                    offset: data_offset,
                    len: data_len,
                });
            }
            ensure_patch_bounds(owner, patch, 8)?;
            let base = data_start
                .checked_add(data_offset)
                .ok_or(AssemblyError::RelocationOverflow)?;
            let absolute = add_signed(base, addend)?;
            let absolute = u64::try_from(absolute).map_err(|_| AssemblyError::RelocationOverflow)?;
            payload[patch..patch + 8].copy_from_slice(&absolute.to_le_bytes());
        }
    }
    Ok(())
}

fn ensure_patch_bounds(
    owner: &OidPlacement,
    patch: usize,
    width: usize,
) -> Result<(), AssemblyError> {
    let owner_end = owner
        .start
        .checked_add(owner.len)
        .ok_or(AssemblyError::RelocationOverflow)?;
    let patch_end = patch
        .checked_add(width)
        .ok_or(AssemblyError::RelocationOverflow)?;
    if patch < owner.start || patch_end > owner_end {
        return Err(AssemblyError::RelocationPatchOutOfBounds {
            patch,
            width,
            owner_start: owner.start,
            owner_len: owner.len,
        });
    }
    Ok(())
}

fn add_signed(base: usize, addend: i64) -> Result<usize, AssemblyError> {
    let value = (base as i128) + (addend as i128);
    usize::try_from(value).map_err(|_| AssemblyError::RelocationOverflow)
}

fn align_vec(bytes: &mut Vec<u8>, alignment: usize) {
    let padding = (alignment - (bytes.len() % alignment)) % alignment;
    bytes.resize(bytes.len() + padding, 0);
}

fn validate_alignment(alignment: u32) -> Result<(), AssemblyError> {
    if alignment == 0 || !alignment.is_power_of_two() || alignment > MAX_ALIGNMENT {
        Err(AssemblyError::InvalidAlignment(alignment))
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str, label: &'static str) -> Result<(), AssemblyError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(AssemblyError::InvalidSha256 {
            label,
            value: value.to_string(),
        })
    }
}

fn compute_assembly_hash(plan_hash: &str, payload: &[u8]) -> Result<String, AssemblyError> {
    validate_sha256(plan_hash, "service plan")?;
    let mut hash = Sha256::new();
    hash.update(BIN_HASH_DOMAIN);
    hash.update(plan_hash.as_bytes());
    hash.update((payload.len() as u64).to_be_bytes());
    hash.update(payload);
    Ok(hex::encode(hash.finalize()))
}

pub fn service_bin_path(
    compiler_cache_root: &Path,
    assembly_hash: &str,
) -> Result<PathBuf, AssemblyError> {
    validate_sha256(assembly_hash, "service assembly")?;
    Ok(compiler_cache_root
        .join("service")
        .join("bytecode")
        .join(format!("{assembly_hash}.bin")))
}

pub fn write_service_bin_atomic(
    io: &AtomicIo,
    compiler_cache_root: &Path,
    bin: &AssembledServiceBin,
) -> Result<PathBuf, AssemblyError> {
    let path = service_bin_path(compiler_cache_root, &bin.assembly_hash)?;
    let encoded = encode_cached_service_bin(bin)?;
    io.write_atomic(&path, &encoded)
        .map_err(|error| AssemblyError::Io(error.to_string()))?;
    Ok(path)
}

pub fn encode_cached_service_bin(bin: &AssembledServiceBin) -> Result<Vec<u8>, AssemblyError> {
    validate_sha256(&bin.plan_hash, "service plan")?;
    validate_sha256(&bin.assembly_hash, "service assembly")?;
    let expected = compute_assembly_hash(&bin.plan_hash, &bin.payload)?;
    if expected != bin.assembly_hash {
        return Err(AssemblyError::AssemblyHashMismatch {
            expected,
            observed: bin.assembly_hash.clone(),
        });
    }
    let target = bin.target_fingerprint.as_bytes();
    let target_len = u16::try_from(target.len()).map_err(|_| AssemblyError::TargetTooLong)?;
    let payload_len = u64::try_from(bin.payload.len()).map_err(|_| AssemblyError::BinTooLarge)?;
    let plan_hash = decode_hex32(&bin.plan_hash)?;
    let assembly_hash = decode_hex32(&bin.assembly_hash)?;

    let mut out = Vec::with_capacity(8 + 4 + 2 + 8 + 32 + 32 + target.len() + bin.payload.len());
    out.extend_from_slice(BIN_MAGIC);
    out.extend_from_slice(&SERVICE_BIN_FORMAT.to_le_bytes());
    out.extend_from_slice(&target_len.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&plan_hash);
    out.extend_from_slice(&assembly_hash);
    out.extend_from_slice(target);
    out.extend_from_slice(&bin.payload);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedServiceBin {
    pub target_fingerprint: String,
    pub plan_hash: String,
    pub assembly_hash: String,
    pub payload: Vec<u8>,
}

pub fn decode_cached_service_bin(bytes: &[u8]) -> Result<DecodedServiceBin, AssemblyError> {
    const FIXED: usize = 8 + 4 + 2 + 8 + 32 + 32;
    if bytes.len() < FIXED || &bytes[..8] != BIN_MAGIC {
        return Err(AssemblyError::InvalidBinHeader);
    }
    let format = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed slice"));
    if format != SERVICE_BIN_FORMAT {
        return Err(AssemblyError::UnsupportedBinFormat(format));
    }
    let target_len = u16::from_le_bytes(bytes[12..14].try_into().expect("fixed slice")) as usize;
    let payload_len = u64::from_le_bytes(bytes[14..22].try_into().expect("fixed slice"));
    let payload_len = usize::try_from(payload_len).map_err(|_| AssemblyError::BinTooLarge)?;
    let plan_hash = hex::encode(&bytes[22..54]);
    let observed_assembly_hash = hex::encode(&bytes[54..86]);
    let expected_len = FIXED
        .checked_add(target_len)
        .and_then(|value| value.checked_add(payload_len))
        .ok_or(AssemblyError::BinTooLarge)?;
    if bytes.len() != expected_len {
        return Err(AssemblyError::InvalidBinLength {
            expected: expected_len,
            observed: bytes.len(),
        });
    }
    let target_end = FIXED + target_len;
    let target_fingerprint = std::str::from_utf8(&bytes[FIXED..target_end])
        .map_err(|_| AssemblyError::InvalidBinTarget)?
        .to_string();
    let payload = bytes[target_end..].to_vec();
    let expected_hash = compute_assembly_hash(&plan_hash, &payload)?;
    if expected_hash != observed_assembly_hash {
        return Err(AssemblyError::AssemblyHashMismatch {
            expected: expected_hash,
            observed: observed_assembly_hash,
        });
    }
    Ok(DecodedServiceBin {
        target_fingerprint,
        plan_hash,
        assembly_hash: observed_assembly_hash,
        payload,
    })
}

fn decode_hex32(value: &str) -> Result<[u8; 32], AssemblyError> {
    let decoded = hex::decode(value).map_err(|_| AssemblyError::InvalidSha256 {
        label: "hash",
        value: value.to_string(),
    })?;
    decoded.try_into().map_err(|_| AssemblyError::InvalidSha256 {
        label: "hash",
        value: value.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyError {
    UnsupportedPlanFormat(u32),
    UnsupportedBinFormat(u32),
    InvalidServiceIdentity(String),
    InvalidTargetFingerprint(String),
    InvalidDependencyIdentity(String),
    InvalidSha256 { label: &'static str, value: String },
    InvalidAlignment(u32),
    DuplicateRequiredOid(u16),
    NoRequiredOids,
    NoEntryOids,
    EntryOidNotRequired(u16),
    InvalidPlacementOrder,
    GraphOidNotRequired(u16),
    MissingOidRecord(u16),
    RecordIdentityMismatch { expected: u16, observed: u16 },
    RecordHashMismatch { oid: u16, expected: String, observed: String },
    RecordKindMismatch { oid: u16, expected: AssemblyRecordKind, observed: AssemblyRecordKind },
    RecordTargetMismatch { oid: u16, expected: String, observed: String },
    MissingMachineCode(u16),
    InvalidDescriptorRecord(u16),
    InvalidEntryOffset { oid: u16, offset: u32, len: usize },
    InvalidFrameTerminator { oid: u16, kind: AssemblyRecordKind, expected: Option<u16>, observed: Option<u16> },
    OidDiagnostic { oid: u16, severity: DiagnosticSeverity, code: String },
    WarningWithoutExecutablePayload { oid: u16, code: String },
    RelocationTargetMissing(u16),
    RelocationOverflow,
    Rel32OutOfRange { target_oid: u16, delta: i128 },
    RelocationPatchOutOfBounds { patch: usize, width: usize, owner_start: usize, owner_len: usize },
    DataRelocationOutOfBounds { offset: usize, len: usize },
    Serialization(String),
    Io(String),
    TargetTooLong,
    BinTooLarge,
    InvalidBinHeader,
    InvalidBinLength { expected: usize, observed: usize },
    InvalidBinTarget,
    AssemblyHashMismatch { expected: String, observed: String },
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlanFormat(value) => write!(formatter, "unsupported service plan format {value}"),
            Self::UnsupportedBinFormat(value) => write!(formatter, "unsupported service bin format {value}"),
            Self::InvalidServiceIdentity(value) => write!(formatter, "invalid service identity {value:?}"),
            Self::InvalidTargetFingerprint(value) => write!(formatter, "invalid target fingerprint {value:?}"),
            Self::InvalidDependencyIdentity(value) => write!(formatter, "invalid service dependency identity {value:?}"),
            Self::InvalidSha256 { label, value } => write!(formatter, "invalid {label} SHA-256 {value:?}"),
            Self::InvalidAlignment(value) => write!(formatter, "invalid machine-code alignment {value}"),
            Self::DuplicateRequiredOid(oid) => write!(formatter, "service plan requires OID {oid} more than once"),
            Self::NoRequiredOids => write!(formatter, "service plan has no required OIDs"),
            Self::NoEntryOids => write!(formatter, "service plan has no entry OIDs"),
            Self::EntryOidNotRequired(oid) => write!(formatter, "service entry OID {oid} is not pinned in required_oids"),
            Self::InvalidPlacementOrder => write!(formatter, "service placement order is not an exact permutation of required OIDs"),
            Self::GraphOidNotRequired(oid) => write!(formatter, "service call graph references unpinned OID {oid}"),
            Self::MissingOidRecord(oid) => write!(formatter, "required OID record {oid} is missing"),
            Self::RecordIdentityMismatch { expected, observed } => write!(formatter, "OID record identity mismatch: expected {expected}, observed {observed}"),
            Self::RecordHashMismatch { oid, expected, observed } => write!(formatter, "OID {oid} hash mismatch: expected {expected}, observed {observed}"),
            Self::RecordKindMismatch { oid, expected, observed } => write!(formatter, "OID {oid} kind mismatch: expected {expected:?}, observed {observed:?}"),
            Self::RecordTargetMismatch { oid, expected, observed } => write!(formatter, "OID {oid} target mismatch: expected {expected:?}, observed {observed:?}"),
            Self::MissingMachineCode(oid) => write!(formatter, "executable OID {oid} has no machine-code payload"),
            Self::InvalidDescriptorRecord(oid) => write!(formatter, "descriptor OID {oid} contains executable-only fields"),
            Self::InvalidEntryOffset { oid, offset, len } => write!(formatter, "OID {oid} entry offset {offset} is outside machine-code length {len}"),
            Self::InvalidFrameTerminator { oid, kind, expected, observed } => write!(formatter, "OID {oid} ({kind:?}) has invalid frame terminator: expected {expected:?}, observed {observed:?}"),
            Self::OidDiagnostic { oid, severity, code } => write!(formatter, "OID {oid} assembly stopped by {}-{code}", severity.debug_marker()),
            Self::WarningWithoutExecutablePayload { oid, code } => write!(formatter, "OID {oid} warning -2-{code} cannot continue without executable payload"),
            Self::RelocationTargetMissing(oid) => write!(formatter, "relocation targets unplaced OID {oid}"),
            Self::RelocationOverflow => write!(formatter, "relocation arithmetic overflow"),
            Self::Rel32OutOfRange { target_oid, delta } => write!(formatter, "relative relocation to OID {target_oid} is out of i32 range ({delta})"),
            Self::RelocationPatchOutOfBounds { patch, width, owner_start, owner_len } => write!(formatter, "relocation patch [{patch}, {}) exceeds owner fragment [{owner_start}, {})", patch + width, owner_start + owner_len),
            Self::DataRelocationOutOfBounds { offset, len } => write!(formatter, "data relocation offset {offset} exceeds service-data length {len}"),
            Self::Serialization(message) => write!(formatter, "failed to serialize service plan: {message}"),
            Self::Io(message) => write!(formatter, "service bin cache I/O failed: {message}"),
            Self::TargetTooLong => write!(formatter, "target fingerprint is too long for service bin header"),
            Self::BinTooLarge => write!(formatter, "service bin exceeds representable cache format size"),
            Self::InvalidBinHeader => write!(formatter, "invalid service bin header"),
            Self::InvalidBinLength { expected, observed } => write!(formatter, "invalid service bin length: expected {expected}, observed {observed}"),
            Self::InvalidBinTarget => write!(formatter, "service bin target fingerprint is not UTF-8"),
            Self::AssemblyHashMismatch { expected, observed } => write!(formatter, "service bin assembly hash mismatch: expected {expected}, observed {observed}"),
        }
    }
}

impl std::error::Error for AssemblyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(ch: char) -> String {
        std::iter::repeat(ch).take(64).collect()
    }

    fn required(oid: u16, kind: AssemblyRecordKind, hash: char) -> RequiredOid {
        RequiredOid {
            oid,
            record_hash: sha(hash),
            kind,
        }
    }

    fn record(oid: u16, kind: AssemblyRecordKind, hash: char, code: Vec<u8>) -> VerifiedAssemblyOidRecord {
        VerifiedAssemblyOidRecord {
            oid,
            record_hash: sha(hash),
            kind,
            target_fingerprint: "linux-x86_64-sysv".into(),
            alignment: 1,
            entry_offset: 0,
            frame_terminator: if kind == AssemblyRecordKind::PackageOperation {
                Some(END_PACKAGE_OID)
            } else if kind.requires_done_terminator() {
                Some(DONE_OID)
            } else {
                None
            },
            diagnostics: Vec::new(),
            machine_code: code,
            relocations: Vec::new(),
        }
    }

    fn plan(required_oids: Vec<RequiredOid>, placement_order: Vec<u16>) -> ServiceAssemblyPlan {
        ServiceAssemblyPlan {
            format: SERVICE_PLAN_FORMAT,
            service_identity: "service:test".into(),
            service_source_sha256: sha('1'),
            index_identity_sha256: sha('2'),
            target_fingerprint: "linux-x86_64-sysv".into(),
            entry_oids: vec![placement_order[0]],
            required_oids,
            placement_order,
            call_graph: BTreeMap::new(),
            service_data: Vec::new(),
            data_alignment: 8,
            dependency_hashes: BTreeMap::new(),
            compile_options: BTreeMap::new(),
        }
    }

    #[test]
    fn assembler_applies_rel32_to_pinned_oid_entry() {
        let caller = 30_458;
        let callee = 30_459;
        let mut caller_record = record(caller, AssemblyRecordKind::Function, 'a', vec![0xE8, 0, 0, 0, 0]);
        caller_record.relocations.push(OidRelocation {
            offset: 1,
            kind: OidRelocationKind::Rel32ToOid {
                target_oid: callee,
                addend: 0,
            },
        });
        let records = BTreeMap::from([
            (caller, caller_record),
            (callee, record(callee, AssemblyRecordKind::Function, 'b', vec![0xC3])),
        ]);
        let plan = plan(
            vec![
                required(caller, AssemblyRecordKind::Function, 'a'),
                required(callee, AssemblyRecordKind::Function, 'b'),
            ],
            vec![caller, callee],
        );
        let bin = assemble_service_bin(&plan, &records).unwrap();
        assert_eq!(&bin.payload[1..5], &[0, 0, 0, 0]);
        assert_eq!(bin.placements[&callee].entry, 5);
    }

    #[test]
    fn package_frame_must_end_with_30457() {
        let oid = 20_086;
        let mut bad = record(oid, AssemblyRecordKind::PackageOperation, 'a', vec![0xC3]);
        bad.frame_terminator = Some(DONE_OID);
        let error = assemble_service_bin(
            &plan(vec![required(oid, AssemblyRecordKind::PackageOperation, 'a')], vec![oid]),
            &BTreeMap::from([(oid, bad)]),
        )
        .unwrap_err();
        assert!(matches!(error, AssemblyError::InvalidFrameTerminator { .. }));
    }

    #[test]
    fn warning_continues_only_with_valid_machine_code() {
        let oid = 30_458;
        let mut warning = record(oid, AssemblyRecordKind::Function, 'a', vec![0xC3]);
        warning.diagnostics.push(OidDiagnostic {
            severity: DiagnosticSeverity::Warning,
            error_book_code: "REL2001".into(),
        });
        let bin = assemble_service_bin(
            &plan(vec![required(oid, AssemblyRecordKind::Function, 'a')], vec![oid]),
            &BTreeMap::from([(oid, warning)]),
        )
        .unwrap();
        assert_eq!(bin.warnings.len(), 1);
    }

    #[test]
    fn error_diagnostic_stops_assembly() {
        let oid = 30_458;
        let mut record = record(oid, AssemblyRecordKind::Function, 'a', vec![0xC3]);
        record.diagnostics.push(OidDiagnostic {
            severity: DiagnosticSeverity::Error,
            error_book_code: "SVC4201".into(),
        });
        let error = assemble_service_bin(
            &plan(vec![required(oid, AssemblyRecordKind::Function, 'a')], vec![oid]),
            &BTreeMap::from([(oid, record)]),
        )
        .unwrap_err();
        assert!(matches!(error, AssemblyError::OidDiagnostic { .. }));
    }

    #[test]
    fn cached_bin_round_trip_rechecks_assembly_hash() {
        let oid = 30_458;
        let records = BTreeMap::from([(
            oid,
            record(oid, AssemblyRecordKind::Function, 'a', vec![0x90, 0xC3]),
        )]);
        let bin = assemble_service_bin(
            &plan(vec![required(oid, AssemblyRecordKind::Function, 'a')], vec![oid]),
            &records,
        )
        .unwrap();
        let encoded = encode_cached_service_bin(&bin).unwrap();
        let decoded = decode_cached_service_bin(&encoded).unwrap();
        assert_eq!(decoded.payload, bin.payload);
        assert_eq!(decoded.plan_hash, bin.plan_hash);
        assert_eq!(decoded.assembly_hash, bin.assembly_hash);
        assert_eq!(decoded.target_fingerprint, bin.target_fingerprint);
    }

    #[test]
    fn plan_rejects_unpinned_call_graph_target() {
        let oid = 30_458;
        let mut plan = plan(vec![required(oid, AssemblyRecordKind::Function, 'a')], vec![oid]);
        plan.call_graph.insert(oid, BTreeSet::from([30_459]));
        assert!(matches!(
            plan.validate(),
            Err(AssemblyError::GraphOidNotRequired(30_459))
        ));
    }
}
