//! Container Task Image (CTI) binary contract and execution-local Slot ABI.
//!
//! RELC owns Task/OID meaning. Container consumes this format but never allocates
//! a competing OID namespace. The contract deliberately has no RELC/Wasmtime
//! dependency so engine and Container can share the exact same bytes.

use std::fmt;

pub const CTI_MAGIC: [u8; 8] = *b"RBECTI01";
pub const CTI_FORMAT_VERSION: u16 = 1;
pub const CTI_SLOT_ABI_VERSION: u16 = 1;
pub const CTI_GRAPH_ABI_VERSION: u16 = 1;
pub const CTI_LOG_ABI_VERSION: u16 = 1;
pub const CTI_HEADER_BYTES: usize = 132;
pub const CTI_SECTION_HEADER_BYTES: usize = 44;
pub const CTI_MAX_SECTIONS: usize = 64;
pub const CTI_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const CTI_SECTION_REQUIRED: u16 = 1;

pub const CTI_SLOT_BYTES: usize = 16;
pub const CTI_MAX_SLOTS: usize = 1024;
pub const CTI_DEFAULT_ARENA_BYTES: usize = 2 * 1024 * 1024;
pub const CTI_MAX_ARENA_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum CtiSectionKind {
    OidTable = 1,
    Graph = 2,
    SlotLayout = 3,
    Capabilities = 4,
    Code = 5,
    ResourcePolicy = 6,
    LogEvents = 7,
    SourceFiles = 8,
    SourceMap = 9,
    ErrorSites = 10,
    Dependencies = 11,
    Checksums = 12,
    StaticData = 13,
    SharedBlobs = 14,
    DebugNames = 15,
    ProfileHints = 16,
}

impl CtiSectionKind {
    pub const REQUIRED: [Self; 12] = [
        Self::OidTable,
        Self::Graph,
        Self::SlotLayout,
        Self::Capabilities,
        Self::Code,
        Self::ResourcePolicy,
        Self::LogEvents,
        Self::SourceFiles,
        Self::SourceMap,
        Self::ErrorSites,
        Self::Dependencies,
        Self::Checksums,
    ];

    pub const fn code(self) -> u16 {
        self as u16
    }

    pub const fn required_by_v1(self) -> bool {
        (self as u16) <= Self::Checksums as u16
    }

    pub const fn from_code(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::OidTable),
            2 => Some(Self::Graph),
            3 => Some(Self::SlotLayout),
            4 => Some(Self::Capabilities),
            5 => Some(Self::Code),
            6 => Some(Self::ResourcePolicy),
            7 => Some(Self::LogEvents),
            8 => Some(Self::SourceFiles),
            9 => Some(Self::SourceMap),
            10 => Some(Self::ErrorSites),
            11 => Some(Self::Dependencies),
            12 => Some(Self::Checksums),
            13 => Some(Self::StaticData),
            14 => Some(Self::SharedBlobs),
            15 => Some(Self::DebugNames),
            16 => Some(Self::ProfileHints),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtiSection {
    /// Raw numeric kind is retained so an older reader can skip a future
    /// optional section without reinterpreting it.
    pub kind: u16,
    pub flags: u16,
    pub data: Vec<u8>,
}

impl CtiSection {
    pub fn required(kind: CtiSectionKind, data: Vec<u8>) -> Self {
        Self {
            kind: kind.code(),
            flags: CTI_SECTION_REQUIRED,
            data,
        }
    }

    pub fn optional(kind: CtiSectionKind, data: Vec<u8>) -> Self {
        Self {
            kind: kind.code(),
            flags: 0,
            data,
        }
    }

    pub const fn is_required(&self) -> bool {
        self.flags & CTI_SECTION_REQUIRED != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtiHeaderV1 {
    pub flags: u32,
    pub task_oid: u16,
    pub capability_abi: u16,
    pub entry_node: u32,
    pub target_id: u32,
    pub runtime_image_sha256: [u8; 32],
    pub task_semantic_sha256: [u8; 32],
}

impl CtiHeaderV1 {
    pub fn new(
        task_oid: u16,
        capability_abi: u16,
        runtime_image_sha256: [u8; 32],
        task_semantic_sha256: [u8; 32],
    ) -> Self {
        Self {
            flags: 0,
            task_oid,
            capability_abi,
            entry_node: 0,
            target_id: 0,
            runtime_image_sha256,
            task_semantic_sha256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskImage {
    pub header: CtiHeaderV1,
    pub sections: Vec<CtiSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtiFormatError(pub String);

impl fmt::Display for CtiFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CtiFormatError {}

type Result<T> = std::result::Result<T, CtiFormatError>;

fn error(message: impl Into<String>) -> CtiFormatError {
    CtiFormatError(message.into())
}

impl ContainerTaskImage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.sections.is_empty() || self.sections.len() > CTI_MAX_SECTIONS {
            return Err(error(format!(
                "CTI section count {} is outside 1..={CTI_MAX_SECTIONS}",
                self.sections.len()
            )));
        }
        let mut sections = self.sections.clone();
        sections.sort_by_key(|section| section.kind);
        validate_section_set(&sections)?;

        let table_bytes = sections
            .len()
            .checked_mul(CTI_SECTION_HEADER_BYTES)
            .ok_or_else(|| error("CTI size arithmetic overflow"))?;
        let data_start = CTI_HEADER_BYTES
            .checked_add(table_bytes)
            .ok_or_else(|| error("CTI size arithmetic overflow"))?;
        let mut next_offset = data_start;
        let mut table = Vec::with_capacity(table_bytes);
        let mut payload = Vec::new();

        for section in &sections {
            if section.flags & !CTI_SECTION_REQUIRED != 0 {
                return Err(error(format!(
                    "CTI section {} uses unknown flags 0x{:04x}",
                    section.kind, section.flags
                )));
            }
            let end = next_offset
                .checked_add(section.data.len())
                .ok_or_else(|| error("CTI size arithmetic overflow"))?;
            if end > CTI_MAX_BYTES || end > u32::MAX as usize {
                return Err(error(format!("CTI image is too large: {end} bytes")));
            }
            push_u16(&mut table, section.kind);
            push_u16(&mut table, section.flags);
            push_u32(
                &mut table,
                u32::try_from(next_offset).map_err(|_| error("CTI offset does not fit u32"))?,
            );
            push_u32(
                &mut table,
                u32::try_from(section.data.len())
                    .map_err(|_| error("CTI section length does not fit u32"))?,
            );
            table.extend_from_slice(&cti_sha256(&section.data));
            payload.extend_from_slice(&section.data);
            next_offset = end;
        }

        let mut body = table;
        body.extend_from_slice(&payload);
        let total = CTI_HEADER_BYTES
            .checked_add(body.len())
            .ok_or_else(|| error("CTI size arithmetic overflow"))?;
        if total > CTI_MAX_BYTES {
            return Err(error(format!("CTI image is too large: {total} bytes")));
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&CTI_MAGIC);
        push_u16(&mut out, CTI_FORMAT_VERSION);
        push_u16(&mut out, CTI_HEADER_BYTES as u16);
        push_u16(&mut out, CTI_SLOT_ABI_VERSION);
        push_u16(&mut out, CTI_GRAPH_ABI_VERSION);
        push_u16(&mut out, CTI_LOG_ABI_VERSION);
        push_u16(&mut out, self.header.capability_abi);
        push_u32(&mut out, self.header.flags);
        push_u16(&mut out, self.header.task_oid);
        push_u16(&mut out, sections.len() as u16);
        push_u32(&mut out, self.header.entry_node);
        push_u32(&mut out, self.header.target_id);
        out.extend_from_slice(&self.header.runtime_image_sha256);
        out.extend_from_slice(&self.header.task_semantic_sha256);
        out.extend_from_slice(&cti_sha256(&body));
        debug_assert_eq!(out.len(), CTI_HEADER_BYTES);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > CTI_MAX_BYTES {
            return Err(error("CTI exceeds v1 size ceiling"));
        }
        if bytes.len() < CTI_HEADER_BYTES {
            return Err(error("CTI is shorter than its fixed v1 header"));
        }
        let mut reader = Reader::new(&bytes[..CTI_HEADER_BYTES]);
        if reader.array::<8>()? != CTI_MAGIC {
            return Err(error("CTI magic does not match RBECTI01"));
        }
        expect_version("format", reader.u16()?, CTI_FORMAT_VERSION)?;
        let header_bytes = reader.u16()?;
        if usize::from(header_bytes) != CTI_HEADER_BYTES {
            return Err(error(format!("unsupported CTI header size {header_bytes}")));
        }
        expect_version("slot ABI", reader.u16()?, CTI_SLOT_ABI_VERSION)?;
        expect_version("graph ABI", reader.u16()?, CTI_GRAPH_ABI_VERSION)?;
        expect_version("log ABI", reader.u16()?, CTI_LOG_ABI_VERSION)?;
        let capability_abi = reader.u16()?;
        let header_flags = reader.u32()?;
        let task_oid = reader.u16()?;
        let section_count = usize::from(reader.u16()?);
        if section_count == 0 || section_count > CTI_MAX_SECTIONS {
            return Err(error(format!("invalid CTI section count {section_count}")));
        }
        let entry_node = reader.u32()?;
        let target_id = reader.u32()?;
        let runtime_image_sha256 = reader.array::<32>()?;
        let task_semantic_sha256 = reader.array::<32>()?;
        let payload_sha256 = reader.array::<32>()?;
        reader.finish()?;

        if cti_sha256(&bytes[CTI_HEADER_BYTES..]) != payload_sha256 {
            return Err(error("CTI payload SHA-256 mismatch"));
        }
        let table_bytes = section_count
            .checked_mul(CTI_SECTION_HEADER_BYTES)
            .ok_or_else(|| error("CTI section-table size overflow"))?;
        let data_start = CTI_HEADER_BYTES
            .checked_add(table_bytes)
            .ok_or_else(|| error("CTI section-table size overflow"))?;
        if data_start > bytes.len() {
            return Err(error("CTI section table is truncated"));
        }

        let mut table = Reader::new(&bytes[CTI_HEADER_BYTES..data_start]);
        let mut metadata = Vec::with_capacity(section_count);
        let mut previous_kind = None;
        for _ in 0..section_count {
            let kind = table.u16()?;
            let flags = table.u16()?;
            if flags & !CTI_SECTION_REQUIRED != 0 {
                return Err(error(format!("CTI section {kind} uses unknown flags")));
            }
            if previous_kind.is_some_and(|previous| kind <= previous) {
                return Err(error("CTI section table is not in canonical numeric order"));
            }
            previous_kind = Some(kind);
            if CtiSectionKind::from_code(kind).is_none() && flags & CTI_SECTION_REQUIRED != 0 {
                return Err(error(format!("unknown required CTI section {kind}")));
            }
            metadata.push((
                kind,
                flags,
                table.u32()? as usize,
                table.u32()? as usize,
                table.array::<32>()?,
            ));
        }
        table.finish()?;

        let mut expected_offset = data_start;
        let mut sections = Vec::with_capacity(section_count);
        for (kind, flags, offset, length, digest) in metadata {
            if offset != expected_offset {
                return Err(error(format!(
                    "CTI section {kind} begins at {offset}, expected {expected_offset}"
                )));
            }
            let end = offset
                .checked_add(length)
                .ok_or_else(|| error("CTI section range overflow"))?;
            let data = bytes
                .get(offset..end)
                .ok_or_else(|| error(format!("CTI section {kind} is out of bounds")))?;
            if cti_sha256(data) != digest {
                return Err(error(format!("CTI section {kind} SHA-256 mismatch")));
            }
            sections.push(CtiSection {
                kind,
                flags,
                data: data.to_vec(),
            });
            expected_offset = end;
        }
        if expected_offset != bytes.len() {
            return Err(error("CTI contains trailing bytes"));
        }
        validate_section_set(&sections)?;

        Ok(Self {
            header: CtiHeaderV1 {
                flags: header_flags,
                task_oid,
                capability_abi,
                entry_node,
                target_id,
                runtime_image_sha256,
                task_semantic_sha256,
            },
            sections,
        })
    }
}

fn validate_section_set(sections: &[CtiSection]) -> Result<()> {
    let mut previous = None;
    for section in sections {
        if previous == Some(section.kind) {
            return Err(error(format!("duplicate CTI section {}", section.kind)));
        }
        previous = Some(section.kind);
        match CtiSectionKind::from_code(section.kind) {
            Some(kind) if kind.required_by_v1() && !section.is_required() => {
                return Err(error(format!(
                    "CTI v1 section {} must be marked required",
                    section.kind
                )));
            }
            None if section.is_required() => {
                return Err(error(format!("unknown required CTI section {}", section.kind)));
            }
            _ => {}
        }
    }
    for kind in CtiSectionKind::REQUIRED {
        if !sections.iter().any(|section| section.kind == kind.code()) {
            return Err(error(format!("required CTI section {} is missing", kind.code())));
        }
    }
    Ok(())
}

fn expect_version(label: &str, observed: u16, expected: u16) -> Result<()> {
    if observed == expected {
        Ok(())
    } else {
        Err(error(format!(
            "unsupported CTI {label} {observed}; expected {expected}"
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotType {
    Empty = 0,
    Bool = 1,
    I64 = 2,
    U64 = 3,
    F64 = 4,
    Bytes = 5,
    Utf8 = 6,
    Object = 7,
    Handle = 8,
}

impl SlotType {
    pub const fn from_tag(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Empty),
            1 => Some(Self::Bool),
            2 => Some(Self::I64),
            3 => Some(Self::U64),
            4 => Some(Self::F64),
            5 => Some(Self::Bytes),
            6 => Some(Self::Utf8),
            7 => Some(Self::Object),
            8 => Some(Self::Handle),
            _ => None,
        }
    }

    pub const fn arena_backed(self) -> bool {
        matches!(self, Self::Bytes | Self::Utf8 | Self::Object)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HandleKind {
    Generic = 1,
    QuickDbTransaction = 2,
    StorageStream = 3,
    ServiceSession = 4,
    NetworkStream = 5,
    VaultLease = 6,
}

impl HandleKind {
    pub const fn from_code(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Generic),
            2 => Some(Self::QuickDbTransaction),
            3 => Some(Self::StorageStream),
            4 => Some(Self::ServiceSession),
            5 => Some(Self::NetworkStream),
            6 => Some(Self::VaultLease),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaRef {
    pub offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpaqueHandle {
    pub kind: HandleKind,
    pub id: u64,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub tag: SlotType,
    /// Handle slots use flags for HandleKind. Other v1 types require flags=0.
    pub flags: u8,
    pub a: u64,
    pub b: u32,
}

impl Default for Slot {
    fn default() -> Self {
        Self::empty()
    }
}

impl Slot {
    pub const fn empty() -> Self {
        Self {
            tag: SlotType::Empty,
            flags: 0,
            a: 0,
            b: 0,
        }
    }

    pub const fn boolean(value: bool) -> Self {
        Self {
            tag: SlotType::Bool,
            flags: 0,
            a: value as u64,
            b: 0,
        }
    }

    pub const fn i64(value: i64) -> Self {
        Self {
            tag: SlotType::I64,
            flags: 0,
            a: value as u64,
            b: 0,
        }
    }

    pub const fn u64(value: u64) -> Self {
        Self {
            tag: SlotType::U64,
            flags: 0,
            a: value,
            b: 0,
        }
    }

    pub fn f64(value: f64) -> Self {
        Self {
            tag: SlotType::F64,
            flags: 0,
            a: value.to_bits(),
            b: 0,
        }
    }

    pub fn arena(tag: SlotType, reference: ArenaRef) -> Result<Self> {
        if !tag.arena_backed() {
            return Err(error("arena Slot must use Bytes, Utf8 or Object"));
        }
        Ok(Self {
            tag,
            flags: 0,
            a: reference.offset,
            b: reference.length,
        })
    }

    pub const fn handle(handle: OpaqueHandle) -> Self {
        Self {
            tag: SlotType::Handle,
            flags: handle.kind as u8,
            a: handle.id,
            b: handle.generation,
        }
    }

    pub fn validate(self) -> Result<()> {
        match self.tag {
            SlotType::Empty if self.flags != 0 || self.a != 0 || self.b != 0 => {
                Err(error("Empty Slot must contain zero payload"))
            }
            SlotType::Bool if self.flags != 0 || self.b != 0 || self.a > 1 => {
                Err(error("Bool Slot must contain canonical 0/1 payload"))
            }
            SlotType::I64 | SlotType::U64 | SlotType::F64
                if self.flags != 0 || self.b != 0 =>
            {
                Err(error("numeric Slot contains non-zero flags/reserved payload"))
            }
            SlotType::Bytes | SlotType::Utf8 | SlotType::Object if self.flags != 0 => {
                Err(error("arena-backed Slot contains unknown flags"))
            }
            SlotType::Handle if HandleKind::from_code(self.flags).is_none() => {
                Err(error("Handle Slot contains unknown handle kind"))
            }
            _ => Ok(()),
        }
    }

    pub fn encode(self) -> Result<[u8; CTI_SLOT_BYTES]> {
        self.validate()?;
        let mut out = [0u8; CTI_SLOT_BYTES];
        out[0] = self.tag as u8;
        out[1] = self.flags;
        out[4..12].copy_from_slice(&self.a.to_be_bytes());
        out[12..16].copy_from_slice(&self.b.to_be_bytes());
        Ok(out)
    }

    pub fn decode(bytes: [u8; CTI_SLOT_BYTES]) -> Result<Self> {
        if bytes[2] != 0 || bytes[3] != 0 {
            return Err(error("Slot reserved bytes must be zero"));
        }
        let tag = SlotType::from_tag(bytes[0])
            .ok_or_else(|| error(format!("unknown Slot tag {}", bytes[0])))?;
        let mut a_bytes = [0u8; 8];
        a_bytes.copy_from_slice(&bytes[4..12]);
        let mut b_bytes = [0u8; 4];
        b_bytes.copy_from_slice(&bytes[12..16]);
        let slot = Self {
            tag,
            flags: bytes[1],
            a: u64::from_be_bytes(a_bytes),
            b: u32::from_be_bytes(b_bytes),
        };
        slot.validate()?;
        Ok(slot)
    }

    pub const fn arena_ref(self) -> Option<ArenaRef> {
        if self.tag.arena_backed() {
            Some(ArenaRef {
                offset: self.a,
                length: self.b,
            })
        } else {
            None
        }
    }

    pub fn opaque_handle(self) -> Option<OpaqueHandle> {
        if self.tag != SlotType::Handle {
            return None;
        }
        Some(OpaqueHandle {
            kind: HandleKind::from_code(self.flags)?,
            id: self.a,
            generation: self.b,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SlotTable {
    slots: Vec<Slot>,
}

impl SlotTable {
    pub fn new(count: usize) -> Result<Self> {
        if count > CTI_MAX_SLOTS {
            return Err(error(format!("Slot count {count} exceeds {CTI_MAX_SLOTS}")));
        }
        Ok(Self {
            slots: vec![Slot::empty(); count],
        })
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn get(&self, index: usize) -> Result<Slot> {
        self.slots
            .get(index)
            .copied()
            .ok_or_else(|| error(format!("Slot index {index} is out of bounds")))
    }

    pub fn set(&mut self, index: usize, slot: Slot) -> Result<()> {
        slot.validate()?;
        let target = self
            .slots
            .get_mut(index)
            .ok_or_else(|| error(format!("Slot index {index} is out of bounds")))?;
        *target = slot;
        Ok(())
    }

    pub fn as_slice(&self) -> &[Slot] {
        &self.slots
    }
}

#[derive(Debug, Clone)]
pub struct TaskArena {
    bytes: Vec<u8>,
    limit: usize,
}

impl Default for TaskArena {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            limit: CTI_DEFAULT_ARENA_BYTES,
        }
    }
}

impl TaskArena {
    pub fn with_limit(limit: usize) -> Result<Self> {
        if limit == 0 || limit > CTI_MAX_ARENA_BYTES {
            return Err(error(format!("invalid Task Arena limit {limit}")));
        }
        Ok(Self {
            bytes: Vec::new(),
            limit,
        })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<ArenaRef> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| error("Task Arena size overflow"))?;
        if end > self.limit {
            return Err(error(format!(
                "Task Arena request needs {end} bytes; limit is {}",
                self.limit
            )));
        }
        let offset = u64::try_from(self.bytes.len())
            .map_err(|_| error("Task Arena offset does not fit u64"))?;
        let length = u32::try_from(bytes.len())
            .map_err(|_| error("Task Arena allocation does not fit u32"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(ArenaRef { offset, length })
    }

    pub fn get(&self, reference: ArenaRef) -> Result<&[u8]> {
        let offset = usize::try_from(reference.offset)
            .map_err(|_| error("Task Arena offset does not fit usize"))?;
        let end = offset
            .checked_add(reference.length as usize)
            .ok_or_else(|| error("Task Arena range overflow"))?;
        self.bytes
            .get(offset..end)
            .ok_or_else(|| error("Task Arena reference is out of bounds"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum CtiNodeKind {
    WasmBlock = 1,
    ServiceCall = 2,
    QuickDb = 3,
    CapabilityCall = 4,
    Condition = 5,
    Return = 6,
    Fail = 7,
    Cleanup = 8,
}

impl CtiNodeKind {
    pub const fn from_code(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::WasmBlock),
            2 => Some(Self::ServiceCall),
            3 => Some(Self::QuickDb),
            4 => Some(Self::CapabilityCall),
            5 => Some(Self::Condition),
            6 => Some(Self::Return),
            7 => Some(Self::Fail),
            8 => Some(Self::Cleanup),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CtiEventClass {
    Task = 1,
    Function = 2,
    Service = 3,
    QuickDb = 4,
    Capability = 5,
    Wasm = 6,
    Memory = 7,
    Network = 8,
    Error = 9,
    Warning = 10,
    Debug = 11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum CtiLogLevel {
    Off = 0,
    Error = 1,
    Task = 2,
    Normal = 3,
    Trace = 4,
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| error("CTI reader offset overflow"))?;
        let source = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| error("CTI is truncated"))?;
        let mut out = [0u8; N];
        out.copy_from_slice(source);
        self.offset = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn finish(self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(error("CTI reader left trailing bytes"))
        }
    }
}

/// Dependency-free SHA-256 keeps the shared contract free of workspace-specific
/// dependencies. It is used for CTI integrity, not as a substitute for Vault.
pub fn cti_sha256(input: &[u8]) -> [u8; 32] {
    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
        0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
        0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut state = H0;
    let whole = input.len() / 64 * 64;
    for chunk in input[..whole].chunks_exact(64) {
        sha256_compress(&mut state, chunk, &K);
    }
    let remainder = &input[whole..];
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let padded = if remainder.len() < 56 { 64 } else { 128 };
    tail[padded - 8..padded].copy_from_slice(&(input.len() as u64).saturating_mul(8).to_be_bytes());
    for chunk in tail[..padded].chunks_exact(64) {
        sha256_compress(&mut state, chunk, &K);
    }
    let mut out = [0u8; 32];
    for (index, word) in state.into_iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn sha256_compress(state: &mut [u32; 8], chunk: &[u8], constants: &[u32; 64]) {
    let mut schedule = [0u32; 64];
    for (index, word) in chunk.chunks_exact(4).take(16).enumerate() {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(word);
        schedule[index] = u32::from_be_bytes(bytes);
    }
    for index in 16..64 {
        let s0 = schedule[index - 15].rotate_right(7)
            ^ schedule[index - 15].rotate_right(18)
            ^ (schedule[index - 15] >> 3);
        let s1 = schedule[index - 2].rotate_right(17)
            ^ schedule[index - 2].rotate_right(19)
            ^ (schedule[index - 2] >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
    let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(constants[index])
            .wrapping_add(schedule[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_sections() -> Vec<CtiSection> {
        CtiSectionKind::REQUIRED
            .into_iter()
            .map(|kind| CtiSection::required(kind, Vec::new()))
            .collect()
    }

    fn hex(bytes: [u8; 32]) -> String {
        bytes.into_iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha256_matches_standard_vectors() {
        assert_eq!(
            hex(cti_sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cti_round_trip_is_deterministic() {
        let image = ContainerTaskImage {
            header: CtiHeaderV1::new(31_844, 1, [0x11; 32], [0x22; 32]),
            sections: required_sections(),
        };
        let first = image.encode().unwrap();
        let decoded = ContainerTaskImage::decode(&first).unwrap();
        assert_eq!(first, decoded.encode().unwrap());
        assert_eq!(decoded.header.task_oid, 31_844);
    }

    #[test]
    fn corrupt_payload_fails_closed() {
        let image = ContainerTaskImage {
            header: CtiHeaderV1::new(31_844, 1, [1; 32], [2; 32]),
            sections: required_sections(),
        };
        let mut bytes = image.encode().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(ContainerTaskImage::decode(&bytes).is_err());
    }

    #[test]
    fn slot_arena_and_handle_round_trip_without_host_pointers() {
        let mut arena = TaskArena::with_limit(64).unwrap();
        let reference = arena.push(b"hello").unwrap();
        let slot = Slot::arena(SlotType::Utf8, reference).unwrap();
        assert_eq!(arena.get(slot.arena_ref().unwrap()).unwrap(), b"hello");
        assert_eq!(Slot::decode(slot.encode().unwrap()).unwrap(), slot);

        let handle = OpaqueHandle {
            kind: HandleKind::ServiceSession,
            id: 44,
            generation: 7,
        };
        let slot = Slot::handle(handle);
        assert_eq!(slot.opaque_handle(), Some(handle));
        assert_eq!(Slot::decode(slot.encode().unwrap()).unwrap(), slot);
    }
}
