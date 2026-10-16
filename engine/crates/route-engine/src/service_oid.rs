//! Project-local RELC Operation ID (OID) cache and native core-operation lowering.
//!
//! OIDs are not a CPU ISA. RELC owns the stable numeric address space and writes
//! sparse, target-local OID records under `.cache/compiler/oid/<ID>`. Each record
//! may contain machine-code bytes for the host target plus relocation/link metadata.
//! The single `.cache/compiler/oid/index` defines the complete 0..=65535 slot map
//! and owns dynamic package/REL bindings. Everything in this directory is cache:
//! deleting it is always recoverable by recompiling the project.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomic_io::AtomicIo;
use sha2::{Digest, Sha256};

use crate::oid_security::{record_aad, OidSecurity, OidVaultAuthority};

pub const OID_DONE: u16 = 0;
pub const OID_RBE_CORE_START: u16 = 1;
pub const OID_RBE_CORE_END: u16 = 5_026;
pub const OID_RBE_RESERVED_START: u16 = 5_027;
pub const OID_RBE_RESERVED_END: u16 = 20_085;
pub const OID_PACKAGE_START: u16 = 20_086;
pub const OID_PACKAGE_END: u16 = 30_456;
pub const OID_END_PACKAGE: u16 = 30_457;
pub const OID_REL_START: u16 = 30_458;
pub const OID_REL_END: u16 = u16::MAX;

pub const OID_INDEX_FORMAT_VERSION: u16 = 1;
pub const OID_RECORD_FORMAT_VERSION: u16 = 1;
pub const OID_NATIVE_ABI_VERSION: u16 = 1;
pub const OID_COMPILER_ABI: &str = "relc-service-oid-v1";

pub const OID_SIGNAL_ERROR: i8 = -1;
pub const OID_SIGNAL_WARNING: i8 = -2;
pub const OID_SIGNAL_UNKNOWN_FATAL: i8 = -3;

pub const OID_FLAG_CALLABLE_LEAF: u32 = 1 << 0;
pub const OID_FLAG_CORE: u32 = 1 << 1;
pub const OID_FLAG_SENTINEL: u32 = 1 << 2;
pub const OID_FLAG_BASELINE_CPU: u32 = 1 << 3;
/// Native callable returns a canonical REL Boolean as integer 0/1 in the platform result register.
pub const OID_FLAG_RETURNS_BOOL: u32 = 1 << 4;

const INDEX_MAGIC: [u8; 8] = *b"RBEOIDX1";
const RECORD_MAGIC: [u8; 8] = *b"RBEOIDR1";
const CHECKSUM_BYTES: usize = 32;
const SLOT_COUNT: usize = (u16::MAX as usize) + 1;

#[derive(Debug)]
pub enum OidError {
    Io(std::io::Error),
    InvalidIndex(String),
    InvalidRecord(String),
    TargetMismatch { expected: String, observed: String },
    UnsupportedTarget(String),
    Exhausted(&'static str),
    Invariant(String),
    Security(String),
    Vault(String),
    Locked(String),
}

impl OidError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_)
            | Self::InvalidIndex(_)
            | Self::InvalidRecord(_)
            | Self::TargetMismatch { .. } => "RELC9001",
            Self::UnsupportedTarget(_) => "RELC3001",
            Self::Exhausted(_) => "RELC2000",
            Self::Invariant(_) | Self::Security(_) | Self::Vault(_) | Self::Locked(_) => "RELC9001",
        }
    }
}

impl fmt::Display for OidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ", self.code())?;
        match self {
            Self::Io(error) => write!(formatter, "OID cache I/O failed: {error}"),
            Self::InvalidIndex(message) => write!(formatter, "OID index is invalid: {message}"),
            Self::InvalidRecord(message) => write!(formatter, "OID record is invalid: {message}"),
            Self::TargetMismatch { expected, observed } => write!(
                formatter,
                "OID cache target mismatch: expected {expected}, observed {observed}"
            ),
            Self::UnsupportedTarget(target) => write!(
                formatter,
                "native Service OID lowering is not implemented for host target {target}"
            ),
            Self::Exhausted(range) => write!(formatter, "OID range {range} is exhausted"),
            Self::Invariant(message) => {
                write!(formatter, "OID compiler invariant failed: {message}")
            }
            Self::Security(message) => {
                write!(formatter, "OID cache security check failed: {message}")
            }
            Self::Vault(message) => write!(formatter, "OID Vault authority failed: {message}"),
            Self::Locked(message) => write!(formatter, "OID cache lease unavailable: {message}"),
        }
    }
}

impl std::error::Error for OidError {}

impl From<std::io::Error> for OidError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OidSlotClass {
    Done = 0,
    RbeCore = 1,
    RbeReserved = 2,
    PackageDynamic = 3,
    EndPackage = 4,
    RelDynamic = 5,
}

impl OidSlotClass {
    fn from_byte(value: u8) -> Result<Self, OidError> {
        match value {
            0 => Ok(Self::Done),
            1 => Ok(Self::RbeCore),
            2 => Ok(Self::RbeReserved),
            3 => Ok(Self::PackageDynamic),
            4 => Ok(Self::EndPackage),
            5 => Ok(Self::RelDynamic),
            other => Err(OidError::InvalidIndex(format!(
                "unknown slot class {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidTarget {
    pub os: String,
    pub arch: String,
    pub abi: String,
    pub pointer_width: u8,
    pub little_endian: bool,
}

impl OidTarget {
    pub fn current() -> Self {
        let abi = if cfg!(target_env = "msvc") {
            "msvc"
        } else if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else if cfg!(target_env = "sgx") {
            "sgx"
        } else {
            "native"
        };
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            abi: abi.to_string(),
            pointer_width: (std::mem::size_of::<usize>() * 8) as u8,
            little_endian: cfg!(target_endian = "little"),
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}-{}-{}-{}{}",
            self.os,
            self.arch,
            self.abi,
            self.pointer_width,
            if self.little_endian { "le" } else { "be" }
        )
    }

    fn encode(&self, writer: &mut Encoder) -> Result<(), OidError> {
        writer.string(&self.os)?;
        writer.string(&self.arch)?;
        writer.string(&self.abi)?;
        writer.u8(self.pointer_width);
        writer.u8(u8::from(self.little_endian));
        Ok(())
    }

    fn decode(reader: &mut Decoder<'_>) -> Result<Self, OidError> {
        let target = Self {
            os: reader.string()?,
            arch: reader.string()?,
            abi: reader.string()?,
            pointer_width: reader.u8()?,
            little_endian: match reader.u8()? {
                0 => false,
                1 => true,
                other => {
                    return Err(OidError::InvalidIndex(format!(
                        "invalid target endianness marker {other}"
                    )))
                }
            },
        };
        if !matches!(target.pointer_width, 32 | 64) {
            return Err(OidError::InvalidIndex(format!(
                "unsupported pointer width {}",
                target.pointer_width
            )));
        }
        Ok(target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageOidOwner {
    pub name: String,
    pub version: String,
    pub artifact_sha256: String,
    pub owned_oids: BTreeSet<u16>,
    pub exports: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidIndex {
    pub format_version: u16,
    pub native_abi_version: u16,
    pub compiler_abi: String,
    pub generation: u64,
    pub target: OidTarget,
    /// Exactly 65,536 entries. New stable RBE OIDs are already represented by
    /// their RBE-core slot and therefore do not require an index rewrite.
    pub slots: Vec<OidSlotClass>,
    /// Dynamic package ownership lives in this same index; there is no second
    /// package/library OID index.
    pub packages: BTreeMap<String, PackageOidOwner>,
    pub rel_bindings: BTreeMap<String, u16>,
}

impl OidIndex {
    pub fn fresh() -> Self {
        Self {
            format_version: OID_INDEX_FORMAT_VERSION,
            native_abi_version: OID_NATIVE_ABI_VERSION,
            compiler_abi: OID_COMPILER_ABI.to_string(),
            generation: 1,
            target: OidTarget::current(),
            slots: canonical_slot_map(),
            packages: BTreeMap::new(),
            rel_bindings: BTreeMap::new(),
        }
    }

    pub fn slot_class(&self, oid: u16) -> OidSlotClass {
        self.slots[oid as usize]
    }

    pub fn first_free_package_oid(&self) -> Result<u16, OidError> {
        let used = self
            .packages
            .values()
            .flat_map(|owner| owner.owned_oids.iter().copied())
            .collect::<BTreeSet<_>>();
        (OID_PACKAGE_START..=OID_PACKAGE_END)
            .find(|oid| !used.contains(oid))
            .ok_or(OidError::Exhausted("20086..30456 package"))
    }

    pub fn first_free_rel_oid(&self) -> Result<u16, OidError> {
        let used = self.rel_bindings.values().copied().collect::<BTreeSet<_>>();
        (OID_REL_START..=OID_REL_END)
            .find(|oid| !used.contains(oid))
            .ok_or(OidError::Exhausted("30458..65535 linked REL"))
    }

    /// Assigns a complete package export surface transactionally in-memory.
    /// Phase 3 owns the RPX caller that supplies canonical `lib_<package>...`
    /// export IDs; Phase 1 provides the reusable allocator/ownership primitive.
    pub fn assign_package_exports(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        artifact_sha256: impl Into<String>,
        export_ids: impl IntoIterator<Item = String>,
    ) -> Result<PackageOidOwner, OidError> {
        let name = name.into();
        let mut next = self.clone();
        next.packages.remove(&name);

        let mut requested = export_ids.into_iter().collect::<Vec<_>>();
        requested.sort();
        if requested
            .windows(2)
            .any(|pair| pair[0].as_str() == pair[1].as_str())
        {
            return Err(OidError::Invariant(format!(
                "package {name:?} contains duplicate export IDs"
            )));
        }

        let used = next
            .packages
            .values()
            .flat_map(|owner| owner.owned_oids.iter().copied())
            .collect::<BTreeSet<_>>();
        let mut free = (OID_PACKAGE_START..=OID_PACKAGE_END).filter(|oid| !used.contains(oid));

        let mut owned_oids = BTreeSet::new();
        let mut exports = BTreeMap::new();
        for export_id in requested {
            let oid = free
                .next()
                .ok_or(OidError::Exhausted("20086..30456 package"))?;
            owned_oids.insert(oid);
            exports.insert(export_id, oid);
        }

        let owner = PackageOidOwner {
            name: name.clone(),
            version: version.into(),
            artifact_sha256: artifact_sha256.into(),
            owned_oids,
            exports,
        };
        next.packages.insert(name, owner.clone());
        next.generation = next.generation.saturating_add(1);
        next.validate_structure()?;
        *self = next;
        Ok(owner)
    }

    pub fn remove_package(&mut self, name: &str) -> Option<PackageOidOwner> {
        let removed = self.packages.remove(name);
        if removed.is_some() {
            self.generation = self.generation.saturating_add(1);
        }
        removed
    }

    pub fn assign_rel_binding(&mut self, symbol: impl Into<String>) -> Result<u16, OidError> {
        let symbol = symbol.into();
        if let Some(existing) = self.rel_bindings.get(&symbol) {
            return Ok(*existing);
        }
        let oid = self.first_free_rel_oid()?;
        self.rel_bindings.insert(symbol, oid);
        self.generation = self.generation.saturating_add(1);
        self.validate_structure()?;
        Ok(oid)
    }

    pub fn remove_rel_binding(&mut self, symbol: &str) -> Option<u16> {
        let removed = self.rel_bindings.remove(symbol);
        if removed.is_some() {
            self.generation = self.generation.saturating_add(1);
        }
        removed
    }

    pub fn validate_structure(&self) -> Result<(), OidError> {
        if self.format_version != OID_INDEX_FORMAT_VERSION {
            return Err(OidError::InvalidIndex(format!(
                "format version {} != supported {}",
                self.format_version, OID_INDEX_FORMAT_VERSION
            )));
        }
        if self.native_abi_version != OID_NATIVE_ABI_VERSION {
            return Err(OidError::InvalidIndex(format!(
                "native ABI {} != supported {}",
                self.native_abi_version, OID_NATIVE_ABI_VERSION
            )));
        }
        if self.compiler_abi != OID_COMPILER_ABI {
            return Err(OidError::InvalidIndex(format!(
                "compiler ABI {:?} != {:?}",
                self.compiler_abi, OID_COMPILER_ABI
            )));
        }
        if self.slots.len() != SLOT_COUNT {
            return Err(OidError::InvalidIndex(format!(
                "slot map contains {} entries instead of {SLOT_COUNT}",
                self.slots.len()
            )));
        }
        let expected = canonical_slot_map();
        if self.slots != expected {
            let mismatch = self
                .slots
                .iter()
                .zip(expected.iter())
                .position(|(actual, expected)| actual != expected)
                .unwrap_or(0);
            return Err(OidError::InvalidIndex(format!(
                "fixed slot class differs at OID {mismatch}"
            )));
        }

        let mut package_oids = BTreeSet::new();
        for (key, owner) in &self.packages {
            if key != &owner.name {
                return Err(OidError::InvalidIndex(format!(
                    "package map key {key:?} does not match owner {:?}",
                    owner.name
                )));
            }
            if owner.version.trim().is_empty() {
                return Err(OidError::InvalidIndex(format!(
                    "package {key:?} has an empty version"
                )));
            }
            for oid in &owner.owned_oids {
                if !(OID_PACKAGE_START..=OID_PACKAGE_END).contains(oid) {
                    return Err(OidError::InvalidIndex(format!(
                        "package {key:?} owns out-of-range OID {oid}"
                    )));
                }
                if !package_oids.insert(*oid) {
                    return Err(OidError::InvalidIndex(format!(
                        "package OID {oid} is owned more than once"
                    )));
                }
            }
            for (export_id, oid) in &owner.exports {
                if export_id.trim().is_empty() {
                    return Err(OidError::InvalidIndex(format!(
                        "package {key:?} contains an empty export ID"
                    )));
                }
                if !owner.owned_oids.contains(oid) {
                    return Err(OidError::InvalidIndex(format!(
                        "package {key:?} export {export_id:?} points at unowned OID {oid}"
                    )));
                }
            }
        }

        let mut rel_oids = BTreeSet::new();
        for (symbol, oid) in &self.rel_bindings {
            if symbol.trim().is_empty() {
                return Err(OidError::InvalidIndex(
                    "linked REL binding has an empty symbol".into(),
                ));
            }
            if !(OID_REL_START..=OID_REL_END).contains(oid) {
                return Err(OidError::InvalidIndex(format!(
                    "linked REL symbol {symbol:?} uses out-of-range OID {oid}"
                )));
            }
            if !rel_oids.insert(*oid) {
                return Err(OidError::InvalidIndex(format!(
                    "linked REL OID {oid} is bound more than once"
                )));
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, OidError> {
        self.validate_structure()?;
        let mut writer = Encoder::new();
        writer.bytes(&INDEX_MAGIC);
        writer.u16(self.format_version);
        writer.u16(self.native_abi_version);
        writer.string(&self.compiler_abi)?;
        writer.u64(self.generation);
        self.target.encode(&mut writer)?;

        writer.u32(
            u32::try_from(self.slots.len())
                .map_err(|_| OidError::Invariant("slot map length overflow".into()))?,
        );
        for slot in &self.slots {
            writer.u8(*slot as u8);
        }

        writer.u32(
            u32::try_from(self.packages.len())
                .map_err(|_| OidError::Invariant("package owner count overflow".into()))?,
        );
        for owner in self.packages.values() {
            writer.string(&owner.name)?;
            writer.string(&owner.version)?;
            writer.string(&owner.artifact_sha256)?;
            writer.u16(
                u16::try_from(owner.owned_oids.len())
                    .map_err(|_| OidError::Invariant("package OID count overflow".into()))?,
            );
            for oid in &owner.owned_oids {
                writer.u16(*oid);
            }
            writer.u16(
                u16::try_from(owner.exports.len())
                    .map_err(|_| OidError::Invariant("package export count overflow".into()))?,
            );
            for (export_id, oid) in &owner.exports {
                writer.string(export_id)?;
                writer.u16(*oid);
            }
        }

        writer.u32(
            u32::try_from(self.rel_bindings.len())
                .map_err(|_| OidError::Invariant("REL binding count overflow".into()))?,
        );
        for (symbol, oid) in &self.rel_bindings {
            writer.string(symbol)?;
            writer.u16(*oid);
        }

        Ok(with_checksum(writer.finish()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OidError> {
        let body = verified_body(bytes, INDEX_MAGIC, "OID index")?;
        let mut reader = Decoder::new(body);
        reader.expect_magic(INDEX_MAGIC, "OID index")?;
        let format_version = reader.u16()?;
        let native_abi_version = reader.u16()?;
        let compiler_abi = reader.string()?;
        let generation = reader.u64()?;
        let target = OidTarget::decode(&mut reader)?;

        let slot_count = reader.u32()? as usize;
        if slot_count != SLOT_COUNT {
            return Err(OidError::InvalidIndex(format!(
                "encoded slot map contains {slot_count} entries instead of {SLOT_COUNT}"
            )));
        }
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            slots.push(OidSlotClass::from_byte(reader.u8()?)?);
        }

        let package_count = reader.u32()? as usize;
        let mut packages = BTreeMap::new();
        for _ in 0..package_count {
            let name = reader.string()?;
            let version = reader.string()?;
            let artifact_sha256 = reader.string()?;
            let owned_count = reader.u16()? as usize;
            let mut owned_oids = BTreeSet::new();
            for _ in 0..owned_count {
                owned_oids.insert(reader.u16()?);
            }
            let export_count = reader.u16()? as usize;
            let mut exports = BTreeMap::new();
            for _ in 0..export_count {
                let export_id = reader.string()?;
                let oid = reader.u16()?;
                if exports.insert(export_id.clone(), oid).is_some() {
                    return Err(OidError::InvalidIndex(format!(
                        "package {name:?} repeats export ID {export_id:?}"
                    )));
                }
            }
            let owner = PackageOidOwner {
                name: name.clone(),
                version,
                artifact_sha256,
                owned_oids,
                exports,
            };
            if packages.insert(name.clone(), owner).is_some() {
                return Err(OidError::InvalidIndex(format!(
                    "package owner {name:?} appears more than once"
                )));
            }
        }

        let rel_count = reader.u32()? as usize;
        let mut rel_bindings = BTreeMap::new();
        for _ in 0..rel_count {
            let symbol = reader.string()?;
            let oid = reader.u16()?;
            if rel_bindings.insert(symbol.clone(), oid).is_some() {
                return Err(OidError::InvalidIndex(format!(
                    "linked REL symbol {symbol:?} appears more than once"
                )));
            }
        }
        reader.finish("OID index")?;

        let index = Self {
            format_version,
            native_abi_version,
            compiler_abi,
            generation,
            target,
            slots,
            packages,
            rel_bindings,
        };
        index.validate_structure()?;
        Ok(index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OidRecordKind {
    Sentinel = 1,
    CoreOperation = 2,
    Function = 3,
    Class = 4,
    Method = 5,
    Constructor = 6,
    PackageOperation = 7,
    ModuleExport = 8,
    RouteExport = 9,
    ServiceExport = 10,
    Capability = 11,
    ObjectLayout = 12,
    AsyncOperation = 13,
    EventHandler = 14,
}

impl OidRecordKind {
    fn from_byte(value: u8) -> Result<Self, OidError> {
        match value {
            1 => Ok(Self::Sentinel),
            2 => Ok(Self::CoreOperation),
            3 => Ok(Self::Function),
            4 => Ok(Self::Class),
            5 => Ok(Self::Method),
            6 => Ok(Self::Constructor),
            7 => Ok(Self::PackageOperation),
            8 => Ok(Self::ModuleExport),
            9 => Ok(Self::RouteExport),
            10 => Ok(Self::ServiceExport),
            11 => Ok(Self::Capability),
            12 => Ok(Self::ObjectLayout),
            13 => Ok(Self::AsyncOperation),
            14 => Ok(Self::EventHandler),
            other => Err(OidError::InvalidRecord(format!(
                "unknown record kind {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum OidDiagnosticSeverity {
    Error = OID_SIGNAL_ERROR,
    Warning = OID_SIGNAL_WARNING,
    UnknownFatal = OID_SIGNAL_UNKNOWN_FATAL,
}

impl OidDiagnosticSeverity {
    fn from_i8(value: i8) -> Result<Self, OidError> {
        match value {
            OID_SIGNAL_ERROR => Ok(Self::Error),
            OID_SIGNAL_WARNING => Ok(Self::Warning),
            OID_SIGNAL_UNKNOWN_FATAL => Ok(Self::UnknownFatal),
            other => Err(OidError::InvalidRecord(format!(
                "unknown OID diagnostic signal {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidDiagnostic {
    pub severity: OidDiagnosticSeverity,
    /// Existing Error Code Book code such as RELC3001/SVC2000. OID does not
    /// invent a second diagnostic registry.
    pub code: String,
}

impl OidDiagnostic {
    pub fn render_control(&self) -> String {
        format!("{}-{}", self.severity as i8, self.code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OidRelocationKind {
    Rel32ToOid = 1,
    Abs64ToData = 2,
    CallTarget = 3,
    ConstAddress = 4,
    JumpTable = 5,
}

impl OidRelocationKind {
    fn from_byte(value: u8) -> Result<Self, OidError> {
        match value {
            1 => Ok(Self::Rel32ToOid),
            2 => Ok(Self::Abs64ToData),
            3 => Ok(Self::CallTarget),
            4 => Ok(Self::ConstAddress),
            5 => Ok(Self::JumpTable),
            other => Err(OidError::InvalidRecord(format!(
                "unknown relocation kind {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidRelocation {
    pub kind: OidRelocationKind,
    pub offset: u32,
    pub target_oid: u16,
    pub addend: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidRecord {
    pub format_version: u16,
    pub native_abi_version: u16,
    pub oid: u16,
    pub kind: OidRecordKind,
    pub flags: u32,
    pub target: OidTarget,
    pub name: String,
    pub entry_offset: u32,
    pub alignment: u16,
    pub required_oids: Vec<u16>,
    pub relocations: Vec<OidRelocation>,
    pub diagnostics: Vec<OidDiagnostic>,
    pub machine_code: Vec<u8>,
}

impl OidRecord {
    pub fn validate(&self) -> Result<(), OidError> {
        if self.format_version != OID_RECORD_FORMAT_VERSION {
            return Err(OidError::InvalidRecord(format!(
                "format version {} != supported {}",
                self.format_version, OID_RECORD_FORMAT_VERSION
            )));
        }
        if self.native_abi_version != OID_NATIVE_ABI_VERSION {
            return Err(OidError::InvalidRecord(format!(
                "native ABI {} != supported {}",
                self.native_abi_version, OID_NATIVE_ABI_VERSION
            )));
        }
        if self.name.trim().is_empty() {
            return Err(OidError::InvalidRecord(format!(
                "OID {} has an empty name",
                self.oid
            )));
        }
        if self.alignment == 0 || !self.alignment.is_power_of_two() {
            return Err(OidError::InvalidRecord(format!(
                "OID {} alignment {} is not a non-zero power of two",
                self.oid, self.alignment
            )));
        }
        if self.entry_offset as usize > self.machine_code.len() {
            return Err(OidError::InvalidRecord(format!(
                "OID {} entry offset {} exceeds machine-code length {}",
                self.oid,
                self.entry_offset,
                self.machine_code.len()
            )));
        }

        let class = canonical_slot_class(self.oid);
        match class {
            OidSlotClass::RbeReserved => {
                return Err(OidError::InvalidRecord(format!(
                    "OID {} belongs to the RBE-reserved range and cannot execute",
                    self.oid
                )))
            }
            OidSlotClass::Done | OidSlotClass::EndPackage => {
                if self.kind != OidRecordKind::Sentinel {
                    return Err(OidError::InvalidRecord(format!(
                        "sentinel OID {} must use Sentinel record kind",
                        self.oid
                    )));
                }
            }
            OidSlotClass::RbeCore => {
                if self.kind != OidRecordKind::CoreOperation {
                    return Err(OidError::InvalidRecord(format!(
                        "RBE core OID {} must use CoreOperation record kind",
                        self.oid
                    )));
                }
            }
            OidSlotClass::PackageDynamic | OidSlotClass::RelDynamic => {}
        }

        let code_len = self.machine_code.len();
        for relocation in &self.relocations {
            if relocation.offset as usize >= code_len {
                return Err(OidError::InvalidRecord(format!(
                    "OID {} relocation offset {} exceeds machine-code length {}",
                    self.oid, relocation.offset, code_len
                )));
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, OidError> {
        self.validate()?;
        let mut writer = Encoder::new();
        writer.bytes(&RECORD_MAGIC);
        writer.u16(self.format_version);
        writer.u16(self.native_abi_version);
        writer.u16(self.oid);
        writer.u8(self.kind as u8);
        writer.u32(self.flags);
        self.target.encode(&mut writer)?;
        writer.string(&self.name)?;
        writer.u32(self.entry_offset);
        writer.u16(self.alignment);

        writer.u16(
            u16::try_from(self.required_oids.len())
                .map_err(|_| OidError::Invariant("required OID count overflow".into()))?,
        );
        for oid in &self.required_oids {
            writer.u16(*oid);
        }

        writer.u16(
            u16::try_from(self.relocations.len())
                .map_err(|_| OidError::Invariant("relocation count overflow".into()))?,
        );
        for relocation in &self.relocations {
            writer.u8(relocation.kind as u8);
            writer.u32(relocation.offset);
            writer.u16(relocation.target_oid);
            writer.i64(relocation.addend);
        }

        writer.u16(
            u16::try_from(self.diagnostics.len())
                .map_err(|_| OidError::Invariant("diagnostic count overflow".into()))?,
        );
        for diagnostic in &self.diagnostics {
            writer.i8(diagnostic.severity as i8);
            writer.string(&diagnostic.code)?;
        }

        writer.u32(
            u32::try_from(self.machine_code.len())
                .map_err(|_| OidError::Invariant("machine-code length overflow".into()))?,
        );
        writer.bytes(&self.machine_code);
        Ok(with_checksum(writer.finish()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OidError> {
        let body = verified_body(bytes, RECORD_MAGIC, "OID record")?;
        let mut reader = Decoder::new(body);
        reader.expect_magic(RECORD_MAGIC, "OID record")?;
        let format_version = reader.u16()?;
        let native_abi_version = reader.u16()?;
        let oid = reader.u16()?;
        let kind = OidRecordKind::from_byte(reader.u8()?)?;
        let flags = reader.u32()?;
        let target = OidTarget::decode(&mut reader)?;
        let name = reader.string()?;
        let entry_offset = reader.u32()?;
        let alignment = reader.u16()?;

        let required_count = reader.u16()? as usize;
        let mut required_oids = Vec::with_capacity(required_count);
        for _ in 0..required_count {
            required_oids.push(reader.u16()?);
        }

        let relocation_count = reader.u16()? as usize;
        let mut relocations = Vec::with_capacity(relocation_count);
        for _ in 0..relocation_count {
            relocations.push(OidRelocation {
                kind: OidRelocationKind::from_byte(reader.u8()?)?,
                offset: reader.u32()?,
                target_oid: reader.u16()?,
                addend: reader.i64()?,
            });
        }

        let diagnostic_count = reader.u16()? as usize;
        let mut diagnostics = Vec::with_capacity(diagnostic_count);
        for _ in 0..diagnostic_count {
            diagnostics.push(OidDiagnostic {
                severity: OidDiagnosticSeverity::from_i8(reader.i8()?)?,
                code: reader.string()?,
            });
        }

        let code_len = reader.u32()? as usize;
        let machine_code = reader.take(code_len)?.to_vec();
        reader.finish("OID record")?;

        let record = Self {
            format_version,
            native_abi_version,
            oid,
            kind,
            flags,
            target,
            name,
            entry_offset,
            alignment,
            required_oids,
            relocations,
            diagnostics,
            machine_code,
        };
        record.validate()?;
        Ok(record)
    }
}

#[derive(Clone)]
pub struct OidCache {
    root: PathBuf,
    io: AtomicIo,
    index: OidIndex,
    security: Option<OidSecurity>,
}

impl OidCache {
    pub fn open_or_rebuild(project_root: &Path) -> Result<Self, OidError> {
        Self::open_or_rebuild_with_io(project_root, AtomicIo::new())
    }

    pub fn open_or_rebuild_with_io(project_root: &Path, io: AtomicIo) -> Result<Self, OidError> {
        let root = project_root.join(".cache/compiler/oid");
        let index_path = root.join("index");
        let current_target = OidTarget::current();

        if index_path.is_file() {
            let bytes = io.read(&index_path)?;
            let index = OidIndex::from_bytes(&bytes)?;
            if index.target != current_target
                || index.native_abi_version != OID_NATIVE_ABI_VERSION
                || index.compiler_abi != OID_COMPILER_ABI
            {
                if root.exists() {
                    fs::remove_dir_all(&root)?;
                }
                return Self::create_fresh(root, io);
            }
            index.validate_structure()?;
            return Ok(Self {
                root,
                io,
                index,
                security: None,
            });
        }

        Self::create_fresh(root, io)
    }

    pub fn open_or_rebuild_with_vault(
        project_root: &Path,
        authority: Arc<dyn OidVaultAuthority>,
    ) -> Result<Self, OidError> {
        let io = AtomicIo::new();
        let security = OidSecurity::acquire(project_root, authority)?;
        let root = project_root.join(".cache/compiler/oid");
        let index_path = root.join("index");
        let current_target = OidTarget::current();

        if index_path.is_file() {
            let bytes = security.open_index(&root, &io)?;
            let index = OidIndex::from_bytes(&bytes)?;
            security.verify_generation(index.generation)?;
            if index.target != current_target
                || index.native_abi_version != OID_NATIVE_ABI_VERSION
                || index.compiler_abi != OID_COMPILER_ABI
            {
                if root.exists() {
                    fs::remove_dir_all(&root)?;
                }
                security.reset()?;
                return Self::create_fresh_secured(root, io, security);
            }
            index.validate_structure()?;
            return Ok(Self {
                root,
                io,
                index,
                security: Some(security),
            });
        }

        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        security.reset()?;
        Self::create_fresh_secured(root, io, security)
    }

    fn create_fresh(root: PathBuf, io: AtomicIo) -> Result<Self, OidError> {
        let index = OidIndex::fresh();
        let cache = Self {
            root,
            io,
            index,
            security: None,
        };
        cache.persist_index()?;
        Ok(cache)
    }

    fn create_fresh_secured(
        root: PathBuf,
        io: AtomicIo,
        security: OidSecurity,
    ) -> Result<Self, OidError> {
        fs::create_dir_all(&root)?;
        let index = OidIndex::fresh();
        let cache = Self {
            root,
            io,
            index,
            security: Some(security),
        };
        cache.persist_index()?;
        Ok(cache)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index(&self) -> &OidIndex {
        &self.index
    }

    pub fn index_mut(&mut self) -> &mut OidIndex {
        &mut self.index
    }

    pub fn persist_index(&self) -> Result<(), OidError> {
        self.index.validate_structure()?;
        let bytes = self.index.to_bytes()?;
        if let Some(security) = &self.security {
            security.write_index(&self.root, &self.io, &bytes, self.index.generation)?;
        } else {
            self.io.write_atomic(&self.root.join("index"), &bytes)?;
        }
        Ok(())
    }

    pub fn replace_index(&mut self, next: OidIndex) -> Result<(), OidError> {
        next.validate_structure()?;
        if next.target != OidTarget::current() {
            return Err(OidError::TargetMismatch {
                expected: OidTarget::current().label(),
                observed: next.target.label(),
            });
        }
        let bytes = next.to_bytes()?;
        if let Some(security) = &self.security {
            security.write_index(&self.root, &self.io, &bytes, next.generation)?;
        } else {
            self.io.write_atomic(&self.root.join("index"), &bytes)?;
        }
        self.index = next;

        Ok(())
    }

    pub fn rebuild(&mut self) -> Result<(), OidError> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        self.index = OidIndex::fresh();
        if let Some(security) = &self.security {
            security.reset()?;
        }
        self.persist_index()
    }

    pub fn record_path(&self, oid: u16) -> PathBuf {
        self.root.join(oid.to_string())
    }

    pub fn read_record(&self, oid: u16) -> Result<OidRecord, OidError> {
        let bytes = if let Some(security) = &self.security {
            let aad = record_aad(oid, &self.index.target.label(), &self.index.compiler_abi);
            security.open_record(&self.root, &self.io, oid, &aad)?
        } else {
            self.io.read(&self.record_path(oid))?
        };
        let record = OidRecord::from_bytes(&bytes)?;
        if record.oid != oid {
            return Err(OidError::InvalidRecord(format!(
                "file {oid} declares OID {}",
                record.oid
            )));
        }
        if record.target != self.index.target {
            return Err(OidError::TargetMismatch {
                expected: self.index.target.label(),
                observed: record.target.label(),
            });
        }
        Ok(record)
    }

    pub fn write_record_if_changed(&self, record: &OidRecord) -> Result<bool, OidError> {
        record.validate()?;
        if record.target != self.index.target {
            return Err(OidError::TargetMismatch {
                expected: self.index.target.label(),
                observed: record.target.label(),
            });
        }
        let bytes = record.to_bytes()?;
        let path = self.record_path(record.oid);
        if let Some(security) = &self.security {
            let aad = record_aad(
                record.oid,
                &self.index.target.label(),
                &self.index.compiler_abi,
            );
            if security.record_matches(&self.root, &self.io, record.oid, &aad, &bytes)? {
                return Ok(false);
            }
            security.stage_record_write(&self.root, &self.io, record.oid, &aad, &bytes)?;
        } else {
            if path.is_file() {
                if let Ok(existing) = self.io.read(&path) {
                    if existing == bytes {
                        return Ok(false);
                    }
                }
            }
            self.io.write_atomic(&path, &bytes)?;
        }
        Ok(true)
    }

    pub fn remove_record(&self, oid: u16) -> Result<bool, OidError> {
        if let Some(security) = &self.security {
            let changed = security.stage_record_remove(&self.root, oid)?;
            if changed {
                self.persist_index()?;
            }
            return Ok(changed);
        }
        let path = self.record_path(oid);
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn inspect(&self) -> Result<String, OidError> {
        let mut materialized = Vec::new();
        if self.root.is_dir() {
            for entry in fs::read_dir(&self.root)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name == "index" {
                    continue;
                }
                if let Ok(oid) = name.parse::<u16>() {
                    materialized.push(oid);
                }
            }
        }
        materialized.sort_unstable();

        Ok(format!(
            "OID index\n  generation: {}\n  target: {}\n  slots: {}\n  packages: {}\n  linked REL: {}\n  materialized records: {}\n  OIDs: {}\n",
            self.index.generation,
            self.index.target.label(),
            self.index.slots.len(),
            self.index.packages.len(),
            self.index.rel_bindings.len(),
            materialized.len(),
            materialized
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreMaterializationReport {
    pub target: OidTarget,
    pub index_generation: u64,
    pub total: usize,
    pub written: usize,
    pub reused: usize,
    pub oids: Vec<u16>,
}

/// Initializes/verifies the single project-local OID index and materializes the
/// first native RBE core leaf operations for the current host target.
///
/// This deliberately does not change `.service` execution yet. Phase 4 consumes
/// these records when assembling service-specific native `.bin` cache files.
pub fn prepare_service_oid_cache(
    project_root: &Path,
) -> Result<CoreMaterializationReport, OidError> {
    let cache = OidCache::open_or_rebuild(project_root)?;
    materialize_bootstrap_core(&cache)
}

pub fn prepare_service_oid_cache_with_vault(
    project_root: &Path,
    authority: Arc<dyn OidVaultAuthority>,
) -> Result<CoreMaterializationReport, OidError> {
    let cache = OidCache::open_or_rebuild_with_vault(project_root, authority)?;
    let report = materialize_bootstrap_core(&cache)?;
    cache.persist_index()?;
    Ok(report)
}

fn materialize_bootstrap_core(cache: &OidCache) -> Result<CoreMaterializationReport, OidError> {
    let target = cache.index.target.clone();
    ensure_supported_native_target(&target)?;

    let mut written = 0usize;
    let mut reused = 0usize;
    let mut oids = Vec::new();
    for spec in bootstrap_core_specs() {
        let record = lower_core_spec(spec, &target)?;
        if cache.write_record_if_changed(&record)? {
            written += 1;
        } else {
            reused += 1;
        }
        oids.push(spec.oid);
    }

    Ok(CoreMaterializationReport {
        target,
        index_generation: cache.index.generation,
        total: oids.len(),
        written,
        reused,
        oids,
    })
}

#[derive(Debug, Clone, Copy)]
enum CoreLowering {
    Done,
    Nop,
    ReturnBool(bool),
    Binary32(BinaryOp),
    Binary64(BinaryOp),
}

#[derive(Debug, Clone, Copy)]
enum BinaryOp {
    Add,
    Sub,
    Mul,
}

#[derive(Debug, Clone, Copy)]
struct CoreSpec {
    oid: u16,
    name: &'static str,
    kind: OidRecordKind,
    lowering: CoreLowering,
}

fn bootstrap_core_specs() -> &'static [CoreSpec] {
    const SPECS: &[CoreSpec] = &[
        CoreSpec {
            oid: OID_DONE,
            name: "DONE",
            kind: OidRecordKind::Sentinel,
            lowering: CoreLowering::Done,
        },
        CoreSpec {
            oid: 1,
            name: "NOP",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Nop,
        },
        CoreSpec {
            oid: 10,
            name: "LOAD_TRUE",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::ReturnBool(true),
        },
        CoreSpec {
            oid: 11,
            name: "LOAD_FALSE",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::ReturnBool(false),
        },
        CoreSpec {
            oid: 113,
            name: "I32_ADD",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Add),
        },
        CoreSpec {
            oid: 114,
            name: "I32_SUB",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Sub),
        },
        CoreSpec {
            oid: 115,
            name: "I32_MUL",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Mul),
        },
        CoreSpec {
            oid: 123,
            name: "I64_ADD",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Add),
        },
        CoreSpec {
            oid: 124,
            name: "I64_SUB",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Sub),
        },
        CoreSpec {
            oid: 125,
            name: "I64_MUL",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Mul),
        },
        CoreSpec {
            oid: 160,
            name: "U32_ADD",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Add),
        },
        CoreSpec {
            oid: 161,
            name: "U32_SUB",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Sub),
        },
        CoreSpec {
            oid: 162,
            name: "U32_MUL",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary32(BinaryOp::Mul),
        },
        CoreSpec {
            oid: 166,
            name: "U64_ADD",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Add),
        },
        CoreSpec {
            oid: 167,
            name: "U64_SUB",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Sub),
        },
        CoreSpec {
            oid: 168,
            name: "U64_MUL",
            kind: OidRecordKind::CoreOperation,
            lowering: CoreLowering::Binary64(BinaryOp::Mul),
        },
        CoreSpec {
            oid: OID_END_PACKAGE,
            name: "END_PACKAGE",
            kind: OidRecordKind::Sentinel,
            lowering: CoreLowering::Done,
        },
    ];
    SPECS
}

fn lower_core_spec(spec: &CoreSpec, target: &OidTarget) -> Result<OidRecord, OidError> {
    let machine_code = lower_core_machine_code(spec.lowering, target)?;
    let flags = OID_FLAG_CALLABLE_LEAF
        | OID_FLAG_BASELINE_CPU
        | if spec.kind == OidRecordKind::Sentinel {
            OID_FLAG_SENTINEL
        } else {
            OID_FLAG_CORE
        };
    Ok(OidRecord {
        format_version: OID_RECORD_FORMAT_VERSION,
        native_abi_version: OID_NATIVE_ABI_VERSION,
        oid: spec.oid,
        kind: spec.kind,
        flags,
        target: target.clone(),
        name: spec.name.to_string(),
        entry_offset: 0,
        alignment: if target.arch == "aarch64" { 4 } else { 16 },
        required_oids: Vec::new(),
        relocations: Vec::new(),
        diagnostics: Vec::new(),
        machine_code,
    })
}

fn ensure_supported_native_target(target: &OidTarget) -> Result<(), OidError> {
    if !target.little_endian || target.pointer_width != 64 {
        return Err(OidError::UnsupportedTarget(target.label()));
    }
    match target.arch.as_str() {
        "x86_64" | "aarch64" => Ok(()),
        _ => Err(OidError::UnsupportedTarget(target.label())),
    }
}

pub(crate) fn lower_native_bool_return(
    value: bool,
    target: &OidTarget,
) -> Result<Vec<u8>, OidError> {
    lower_core_machine_code(CoreLowering::ReturnBool(value), target)
}

fn lower_core_machine_code(
    lowering: CoreLowering,
    target: &OidTarget,
) -> Result<Vec<u8>, OidError> {
    ensure_supported_native_target(target)?;
    match target.arch.as_str() {
        "x86_64" => lower_x86_64(lowering, target.os == "windows"),
        "aarch64" => lower_aarch64(lowering),
        _ => Err(OidError::UnsupportedTarget(target.label())),
    }
}

/// OID native ABI v1 uses callable leaf fragments. Integer binary helpers use
/// the platform's first two integer argument registers and return through the
/// platform integer result register. Service-bin assembly may inline these in a
/// later optimization phase, but callable semantics are the v1 correctness base.
fn lower_x86_64(lowering: CoreLowering, windows: bool) -> Result<Vec<u8>, OidError> {
    let bytes = match lowering {
        CoreLowering::Done => vec![0xC3],
        CoreLowering::Nop => vec![0x90, 0xC3],
        CoreLowering::ReturnBool(false) => vec![0x31, 0xC0, 0xC3],
        CoreLowering::ReturnBool(true) => vec![0xB8, 1, 0, 0, 0, 0xC3],
        CoreLowering::Binary32(op) => {
            let mut out = if windows {
                vec![0x89, 0xC8]
            } else {
                vec![0x89, 0xF8]
            };
            match (op, windows) {
                (BinaryOp::Add, true) => out.extend_from_slice(&[0x01, 0xD0]),
                (BinaryOp::Add, false) => out.extend_from_slice(&[0x01, 0xF0]),
                (BinaryOp::Sub, true) => out.extend_from_slice(&[0x29, 0xD0]),
                (BinaryOp::Sub, false) => out.extend_from_slice(&[0x29, 0xF0]),
                (BinaryOp::Mul, true) => out.extend_from_slice(&[0x0F, 0xAF, 0xC2]),
                (BinaryOp::Mul, false) => out.extend_from_slice(&[0x0F, 0xAF, 0xC6]),
            }
            out.push(0xC3);
            out
        }
        CoreLowering::Binary64(op) => {
            let mut out = if windows {
                vec![0x48, 0x89, 0xC8]
            } else {
                vec![0x48, 0x89, 0xF8]
            };
            match (op, windows) {
                (BinaryOp::Add, true) => out.extend_from_slice(&[0x48, 0x01, 0xD0]),
                (BinaryOp::Add, false) => out.extend_from_slice(&[0x48, 0x01, 0xF0]),
                (BinaryOp::Sub, true) => out.extend_from_slice(&[0x48, 0x29, 0xD0]),
                (BinaryOp::Sub, false) => out.extend_from_slice(&[0x48, 0x29, 0xF0]),
                (BinaryOp::Mul, true) => out.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]),
                (BinaryOp::Mul, false) => out.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC6]),
            }
            out.push(0xC3);
            out
        }
    };
    Ok(bytes)
}

fn lower_aarch64(lowering: CoreLowering) -> Result<Vec<u8>, OidError> {
    fn push_word(out: &mut Vec<u8>, word: u32) {
        out.extend_from_slice(&word.to_le_bytes());
    }

    let mut out = Vec::new();
    match lowering {
        CoreLowering::Done => {
            push_word(&mut out, 0xD65F03C0);
        }
        CoreLowering::Nop => {
            push_word(&mut out, 0xD503201F);
            push_word(&mut out, 0xD65F03C0);
        }
        CoreLowering::ReturnBool(false) => {
            push_word(&mut out, 0xD2800000);
            push_word(&mut out, 0xD65F03C0);
        }
        CoreLowering::ReturnBool(true) => {
            push_word(&mut out, 0xD2800020);
            push_word(&mut out, 0xD65F03C0);
        }
        CoreLowering::Binary32(op) => {
            push_word(
                &mut out,
                match op {
                    BinaryOp::Add => 0x0B010000,
                    BinaryOp::Sub => 0x4B010000,
                    BinaryOp::Mul => 0x1B017C00,
                },
            );
            push_word(&mut out, 0xD65F03C0);
        }
        CoreLowering::Binary64(op) => {
            push_word(
                &mut out,
                match op {
                    BinaryOp::Add => 0x8B010000,
                    BinaryOp::Sub => 0xCB010000,
                    BinaryOp::Mul => 0x9B017C00,
                },
            );
            push_word(&mut out, 0xD65F03C0);
        }
    }
    Ok(out)
}

fn canonical_slot_map() -> Vec<OidSlotClass> {
    (0u32..=u16::MAX as u32)
        .map(|oid| canonical_slot_class(oid as u16))
        .collect()
}

fn canonical_slot_class(oid: u16) -> OidSlotClass {
    match oid {
        OID_DONE => OidSlotClass::Done,
        OID_RBE_CORE_START..=OID_RBE_CORE_END => OidSlotClass::RbeCore,
        OID_RBE_RESERVED_START..=OID_RBE_RESERVED_END => OidSlotClass::RbeReserved,
        OID_PACKAGE_START..=OID_PACKAGE_END => OidSlotClass::PackageDynamic,
        OID_END_PACKAGE => OidSlotClass::EndPackage,
        OID_REL_START..=OID_REL_END => OidSlotClass::RelDynamic,
    }
}

fn with_checksum(mut body: Vec<u8>) -> Vec<u8> {
    let digest = Sha256::digest(&body);
    body.extend_from_slice(&digest);
    body
}

fn verified_body<'a>(
    bytes: &'a [u8],
    expected_magic: [u8; 8],
    label: &str,
) -> Result<&'a [u8], OidError> {
    if bytes.len() < expected_magic.len() + CHECKSUM_BYTES {
        return Err(match label {
            "OID index" => OidError::InvalidIndex(format!("{label} is truncated")),
            _ => OidError::InvalidRecord(format!("{label} is truncated")),
        });
    }
    let body_len = bytes.len() - CHECKSUM_BYTES;
    let (body, checksum) = bytes.split_at(body_len);
    let digest = Sha256::digest(body);
    if digest[..] != checksum[..] {
        return Err(match label {
            "OID index" => OidError::InvalidIndex(format!("{label} checksum mismatch")),
            _ => OidError::InvalidRecord(format!("{label} checksum mismatch")),
        });
    }
    if body.get(..expected_magic.len()) != Some(expected_magic.as_slice()) {
        return Err(match label {
            "OID index" => OidError::InvalidIndex(format!("{label} magic mismatch")),
            _ => OidError::InvalidRecord(format!("{label} magic mismatch")),
        });
    }
    Ok(body)
}

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn i8(&mut self, value: i8) {
        self.bytes.push(value as u8);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) -> Result<(), OidError> {
        let len = u32::try_from(value.len())
            .map_err(|_| OidError::Invariant("OID string exceeds u32 length".into()))?;
        self.u32(len);
        self.bytes(value.as_bytes());
        Ok(())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn finish(&self, label: &str) -> Result<(), OidError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            let message = format!(
                "{label} has {} trailing byte(s)",
                self.bytes.len() - self.offset
            );
            if label == "OID index" {
                Err(OidError::InvalidIndex(message))
            } else {
                Err(OidError::InvalidRecord(message))
            }
        }
    }

    fn expect_magic(&mut self, expected: [u8; 8], label: &str) -> Result<(), OidError> {
        let actual = self.take(expected.len())?;
        if actual == expected.as_slice() {
            Ok(())
        } else if label == "OID index" {
            Err(OidError::InvalidIndex(format!("{label} magic mismatch")))
        } else {
            Err(OidError::InvalidRecord(format!("{label} magic mismatch")))
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], OidError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| OidError::InvalidRecord("OID decode offset overflow".into()))?;
        let slice = self.bytes.get(self.offset..end).ok_or_else(|| {
            OidError::InvalidRecord(format!(
                "OID binary is truncated at byte {} while reading {len} byte(s)",
                self.offset
            ))
        })?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, OidError> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8, OidError> {
        Ok(self.u8()? as i8)
    }

    fn u16(&mut self) -> Result<u16, OidError> {
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, OidError> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, OidError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, OidError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(i64::from_le_bytes(bytes))
    }

    fn string(&mut self) -> Result<String, OidError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            OidError::InvalidRecord("OID binary contains invalid UTF-8 string metadata".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rbe-oid-test-{name}-{}-{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_target(arch: &str, os: &str) -> OidTarget {
        OidTarget {
            os: os.into(),
            arch: arch.into(),
            abi: "test".into(),
            pointer_width: 64,
            little_endian: true,
        }
    }

    #[test]
    fn index_contains_the_complete_fixed_oid_address_space() {
        let index = OidIndex::fresh();
        assert_eq!(index.slots.len(), 65_536);
        assert_eq!(index.slot_class(0), OidSlotClass::Done);
        assert_eq!(index.slot_class(1), OidSlotClass::RbeCore);
        assert_eq!(index.slot_class(5_026), OidSlotClass::RbeCore);
        assert_eq!(index.slot_class(5_027), OidSlotClass::RbeReserved);
        assert_eq!(index.slot_class(20_085), OidSlotClass::RbeReserved);
        assert_eq!(index.slot_class(20_086), OidSlotClass::PackageDynamic);
        assert_eq!(index.slot_class(30_456), OidSlotClass::PackageDynamic);
        assert_eq!(index.slot_class(30_457), OidSlotClass::EndPackage);
        assert_eq!(index.slot_class(30_458), OidSlotClass::RelDynamic);
        assert_eq!(index.slot_class(65_535), OidSlotClass::RelDynamic);
    }

    #[test]
    fn index_round_trip_preserves_package_ownership_and_rel_bindings() {
        let mut index = OidIndex::fresh();
        let owner = index
            .assign_package_exports(
                "mail",
                "3.0.0",
                "a".repeat(64),
                vec![
                    "lib_mail".to_string(),
                    "lib_mail_send".to_string(),
                    "lib_mail_receive".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(owner.exports["lib_mail"], OID_PACKAGE_START);
        assert_eq!(
            index.assign_rel_binding("module.users.findUser").unwrap(),
            OID_REL_START
        );

        let bytes = index.to_bytes().unwrap();
        let decoded = OidIndex::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, index);
    }

    #[test]
    fn removed_package_oids_return_to_the_reusable_pool() {
        let mut index = OidIndex::fresh();
        index
            .assign_package_exports(
                "mail",
                "1.0.0",
                "a".repeat(64),
                vec!["lib_mail".into(), "lib_mail_send".into()],
            )
            .unwrap();
        assert_eq!(
            index.first_free_package_oid().unwrap(),
            OID_PACKAGE_START + 2
        );
        index.remove_package("mail");
        assert_eq!(index.first_free_package_oid().unwrap(), OID_PACKAGE_START);

        let replacement = index
            .assign_package_exports(
                "archive",
                "1.0.0",
                "b".repeat(64),
                vec!["lib_archive".into()],
            )
            .unwrap();
        assert_eq!(replacement.exports["lib_archive"], OID_PACKAGE_START);
    }

    #[test]
    fn record_round_trip_preserves_diagnostics_and_relocations() {
        let target = OidTarget::current();
        let record = OidRecord {
            format_version: OID_RECORD_FORMAT_VERSION,
            native_abi_version: OID_NATIVE_ABI_VERSION,
            oid: OID_PACKAGE_START,
            kind: OidRecordKind::PackageOperation,
            flags: 0,
            target,
            name: "lib_mail_send".into(),
            entry_offset: 0,
            alignment: 16,
            required_oids: vec![1, 123],
            relocations: vec![OidRelocation {
                kind: OidRelocationKind::CallTarget,
                offset: 1,
                target_oid: 123,
                addend: 0,
            }],
            diagnostics: vec![OidDiagnostic {
                severity: OidDiagnosticSeverity::Warning,
                code: "RELC2000".into(),
            }],
            machine_code: vec![0x90, 0xC3],
        };
        let bytes = record.to_bytes().unwrap();
        let decoded = OidRecord::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.diagnostics[0].render_control(), "-2-RELC2000");
    }

    #[test]
    fn record_checksum_rejects_corruption() {
        let target = fake_target("x86_64", "linux");
        let record = OidRecord {
            format_version: OID_RECORD_FORMAT_VERSION,
            native_abi_version: OID_NATIVE_ABI_VERSION,
            oid: 1,
            kind: OidRecordKind::CoreOperation,
            flags: OID_FLAG_CORE,
            target,
            name: "NOP".into(),
            entry_offset: 0,
            alignment: 16,
            required_oids: Vec::new(),
            relocations: Vec::new(),
            diagnostics: Vec::new(),
            machine_code: vec![0x90, 0xC3],
        };
        let mut bytes = record.to_bytes().unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x5A;
        let error = OidRecord::from_bytes(&bytes).unwrap_err();
        assert_eq!(error.code(), "RELC9001");
    }

    #[test]
    fn x86_64_core_lowering_uses_platform_argument_registers() {
        let linux = fake_target("x86_64", "linux");
        let windows = fake_target("x86_64", "windows");
        let add64 = CoreLowering::Binary64(BinaryOp::Add);
        assert_eq!(
            lower_core_machine_code(add64, &linux).unwrap(),
            vec![0x48, 0x89, 0xF8, 0x48, 0x01, 0xF0, 0xC3]
        );
        assert_eq!(
            lower_core_machine_code(add64, &windows).unwrap(),
            vec![0x48, 0x89, 0xC8, 0x48, 0x01, 0xD0, 0xC3]
        );
    }

    #[test]
    fn aarch64_core_lowering_emits_baseline_leaf_code() {
        let target = fake_target("aarch64", "linux");
        let add64 =
            lower_core_machine_code(CoreLowering::Binary64(BinaryOp::Add), &target).unwrap();
        assert_eq!(
            add64,
            [0x8B010000u32.to_le_bytes(), 0xD65F03C0u32.to_le_bytes()].concat()
        );
        let done = lower_core_machine_code(CoreLowering::Done, &target).unwrap();
        assert_eq!(done, 0xD65F03C0u32.to_le_bytes());
    }

    #[test]
    fn unsupported_target_fails_with_existing_error_book_code() {
        let target = fake_target("mips64", "linux");
        let error = lower_core_machine_code(CoreLowering::Done, &target).unwrap_err();
        assert_eq!(error.code(), "RELC3001");
    }

    #[test]
    fn cache_materialization_is_sparse_and_reuses_identical_records() {
        let project = temp_dir("sparse");
        let first = prepare_service_oid_cache(&project).unwrap();
        let second = prepare_service_oid_cache(&project).unwrap();
        assert_eq!(first.total, bootstrap_core_specs().len());
        assert_eq!(first.written, first.total);
        assert_eq!(second.written, 0);
        assert_eq!(second.reused, second.total);

        let cache = OidCache::open_or_rebuild(&project).unwrap();
        assert!(cache.root().join("index").is_file());
        assert!(cache.record_path(123).is_file());
        assert!(!cache.record_path(5_026).exists());

        let file_count = fs::read_dir(cache.root()).unwrap().count();
        assert!(
            file_count < 100,
            "cache must stay sparse, got {file_count} files"
        );
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn target_mismatch_rebuilds_the_disposable_cache() {
        let project = temp_dir("target-rebuild");
        let io = AtomicIo::new();
        let mut cache = OidCache::open_or_rebuild_with_io(&project, io.clone()).unwrap();
        fs::write(cache.root().join("321"), b"stale").unwrap();

        let mut wrong = cache.index().clone();
        wrong.target.arch = "definitely-not-current".into();
        io.write_atomic(&cache.root().join("index"), &wrong.to_bytes().unwrap())
            .unwrap();

        cache = OidCache::open_or_rebuild_with_io(&project, io).unwrap();
        assert_eq!(cache.index().target, OidTarget::current());
        assert!(!cache.root().join("321").exists());
        let _ = fs::remove_dir_all(project);
    }
}
