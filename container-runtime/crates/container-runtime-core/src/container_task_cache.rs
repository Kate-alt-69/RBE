//! CTI assembly, content-addressed cache, and RuntimeImage + TaskOID index.
//!
//! This module owns compiler-side cache state only. RELC remains authoritative
//! for Task/OID meaning and supplies the already-compiled section payloads.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ipc_protocol::container_task_image::{cti_sha256, ContainerTaskImage, CtiHeaderV1, CtiSection};

pub const CTI_COMPILER_ABI_VERSION: u16 = 1;
pub const CONTAINER_TASK_INDEX_MAGIC: [u8; 8] = *b"RBECTIX1";
pub const CONTAINER_TASK_INDEX_VERSION: u16 = 1;
const CONTAINER_TASK_INDEX_HEADER_BYTES: usize = 16;
const CONTAINER_TASK_INDEX_ENTRY_BYTES: usize = 104;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskAssemblyInput {
    pub task_oid: u16,
    pub capability_abi: u16,
    pub runtime_image_sha256: [u8; 32],
    pub task_semantic_sha256: [u8; 32],
    pub entry_node: u32,
    pub target_id: u32,
    pub flags: u32,
    pub sections: Vec<CtiSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledContainerTask {
    pub bytes: Vec<u8>,
    pub cti_sha256: [u8; 32],
    pub task_semantic_sha256: [u8; 32],
    pub task_oid: u16,
    pub runtime_image_sha256: [u8; 32],
    pub target_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContainerTaskIndexKey {
    pub runtime_image_sha256: [u8; 32],
    pub task_oid: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskIndexEntry {
    pub cti_sha256: [u8; 32],
    pub task_semantic_sha256: [u8; 32],
    pub compiler_abi: u16,
    pub target_id: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerTaskIndex {
    entries: BTreeMap<ContainerTaskIndexKey, ContainerTaskIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTaskCacheCommit {
    pub blob_path: PathBuf,
    pub cti_sha256: [u8; 32],
    pub index_changed: bool,
}

#[derive(Clone)]
pub struct ContainerTaskAssembler {
    cache_root: PathBuf,
    io: atomic_io::AtomicIo,
}

impl ContainerTaskAssembler {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
            io: atomic_io::AtomicIo::new(),
        }
    }

    pub fn container_cache_dir(&self) -> PathBuf {
        self.cache_root.join("compiler").join("container")
    }

    pub fn index_path(&self) -> PathBuf {
        self.container_cache_dir().join("index")
    }

    pub fn blob_path(&self, digest: &[u8; 32]) -> PathBuf {
        self.container_cache_dir()
            .join(format!("{}.bin", hex::encode(digest)))
    }

    pub fn assemble(
        &self,
        input: ContainerTaskAssemblyInput,
    ) -> Result<AssembledContainerTask, String> {
        let mut header = CtiHeaderV1::new(
            input.task_oid,
            input.capability_abi,
            input.runtime_image_sha256,
            input.task_semantic_sha256,
        );
        header.flags = input.flags;
        header.entry_node = input.entry_node;
        header.target_id = input.target_id;
        let image = ContainerTaskImage {
            header,
            sections: input.sections,
        };
        let bytes = image
            .encode()
            .map_err(|error| format!("failed to encode CTI: {error}"))?;
        let cti_sha256 = cti_sha256(&bytes);
        Ok(AssembledContainerTask {
            bytes,
            cti_sha256,
            task_semantic_sha256: input.task_semantic_sha256,
            task_oid: input.task_oid,
            runtime_image_sha256: input.runtime_image_sha256,
            target_id: input.target_id,
        })
    }

    pub fn load_index(&self) -> Result<ContainerTaskIndex, String> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(ContainerTaskIndex::default());
        }
        let bytes = self
            .io
            .read(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        ContainerTaskIndex::decode(&bytes)
    }

    pub fn commit(
        &self,
        index: &mut ContainerTaskIndex,
        assembled: &AssembledContainerTask,
    ) -> Result<ContainerTaskCacheCommit, String> {
        fs::create_dir_all(self.container_cache_dir()).map_err(|error| {
            format!(
                "failed to create {}: {error}",
                self.container_cache_dir().display()
            )
        })?;

        let blob_path = self.blob_path(&assembled.cti_sha256);
        let blob_is_valid = match self.io.read(&blob_path) {
            Ok(existing) => {
                cti_sha256(&existing) == assembled.cti_sha256 && existing == assembled.bytes
            }
            Err(_) => false,
        };
        if !blob_is_valid {
            self.io
                .write_atomic(&blob_path, &assembled.bytes)
                .map_err(|error| format!("failed to write {}: {error}", blob_path.display()))?;
        }

        let key = ContainerTaskIndexKey {
            runtime_image_sha256: assembled.runtime_image_sha256,
            task_oid: assembled.task_oid,
        };
        let entry = ContainerTaskIndexEntry {
            cti_sha256: assembled.cti_sha256,
            task_semantic_sha256: assembled.task_semantic_sha256,
            compiler_abi: CTI_COMPILER_ABI_VERSION,
            target_id: assembled.target_id,
        };
        let index_changed = index.entries.get(&key) != Some(&entry);
        if index_changed {
            index.entries.insert(key, entry);
            self.write_index(index)?;
        }

        Ok(ContainerTaskCacheCommit {
            blob_path,
            cti_sha256: assembled.cti_sha256,
            index_changed,
        })
    }

    pub fn assemble_and_commit(
        &self,
        index: &mut ContainerTaskIndex,
        input: ContainerTaskAssemblyInput,
    ) -> Result<ContainerTaskCacheCommit, String> {
        let assembled = self.assemble(input)?;
        self.commit(index, &assembled)
    }

    pub fn invalidate(
        &self,
        index: &mut ContainerTaskIndex,
        runtime_image_sha256: [u8; 32],
        task_oid: u16,
    ) -> Result<bool, String> {
        let removed = index
            .entries
            .remove(&ContainerTaskIndexKey {
                runtime_image_sha256,
                task_oid,
            })
            .is_some();
        if removed {
            self.write_index(index)?;
        }
        Ok(removed)
    }

    pub fn replace_index(&self, index: &ContainerTaskIndex) -> Result<(), String> {
        fs::create_dir_all(self.container_cache_dir()).map_err(|error| {
            format!(
                "failed to create {}: {error}",
                self.container_cache_dir().display()
            )
        })?;
        self.write_index(index)
    }

    fn write_index(&self, index: &ContainerTaskIndex) -> Result<(), String> {
        let bytes = index.encode()?;
        let path = self.index_path();
        self.io
            .write_atomic(&path, &bytes)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))
    }
}

impl ContainerTaskIndex {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(
        &self,
        runtime_image_sha256: &[u8; 32],
        task_oid: u16,
    ) -> Option<&ContainerTaskIndexEntry> {
        self.entries.get(&ContainerTaskIndexKey {
            runtime_image_sha256: *runtime_image_sha256,
            task_oid,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ContainerTaskIndexKey, &ContainerTaskIndexEntry)> {
        self.entries.iter()
    }

    pub fn insert(
        &mut self,
        key: ContainerTaskIndexKey,
        entry: ContainerTaskIndexEntry,
    ) -> Option<ContainerTaskIndexEntry> {
        self.entries.insert(key, entry)
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let count = u32::try_from(self.entries.len())
            .map_err(|_| "Container Task index has too many entries".to_string())?;
        let body_bytes = self
            .entries
            .len()
            .checked_mul(CONTAINER_TASK_INDEX_ENTRY_BYTES)
            .ok_or_else(|| "Container Task index size overflow".to_string())?;
        let capacity = CONTAINER_TASK_INDEX_HEADER_BYTES
            .checked_add(body_bytes)
            .ok_or_else(|| "Container Task index size overflow".to_string())?;

        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&CONTAINER_TASK_INDEX_MAGIC);
        push_u16(&mut out, CONTAINER_TASK_INDEX_VERSION);
        push_u16(&mut out, 0);
        push_u32(&mut out, count);
        for (key, entry) in &self.entries {
            out.extend_from_slice(&key.runtime_image_sha256);
            push_u16(&mut out, key.task_oid);
            push_u16(&mut out, entry.compiler_abi);
            push_u32(&mut out, entry.target_id);
            out.extend_from_slice(&entry.task_semantic_sha256);
            out.extend_from_slice(&entry.cti_sha256);
        }
        debug_assert_eq!(out.len(), capacity);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < CONTAINER_TASK_INDEX_HEADER_BYTES {
            return Err("Container Task index is truncated".to_string());
        }
        if bytes[..8] != CONTAINER_TASK_INDEX_MAGIC {
            return Err("Container Task index magic mismatch".to_string());
        }
        let version = read_u16(bytes, 8)?;
        if version != CONTAINER_TASK_INDEX_VERSION {
            return Err(format!(
                "unsupported Container Task index version {version}; expected {CONTAINER_TASK_INDEX_VERSION}"
            ));
        }
        if read_u16(bytes, 10)? != 0 {
            return Err("Container Task index reserved header bits are non-zero".to_string());
        }
        let count = usize::try_from(read_u32(bytes, 12)?)
            .map_err(|_| "Container Task index count does not fit usize".to_string())?;
        let expected = CONTAINER_TASK_INDEX_HEADER_BYTES
            .checked_add(
                count
                    .checked_mul(CONTAINER_TASK_INDEX_ENTRY_BYTES)
                    .ok_or_else(|| "Container Task index size overflow".to_string())?,
            )
            .ok_or_else(|| "Container Task index size overflow".to_string())?;
        if bytes.len() != expected {
            return Err(format!(
                "Container Task index length mismatch: got {}, expected {expected}",
                bytes.len()
            ));
        }

        let mut entries = BTreeMap::new();
        let mut offset = CONTAINER_TASK_INDEX_HEADER_BYTES;
        for _ in 0..count {
            let runtime_image_sha256 = read_array::<32>(bytes, offset)?;
            offset += 32;
            let task_oid = read_u16(bytes, offset)?;
            offset += 2;
            let compiler_abi = read_u16(bytes, offset)?;
            offset += 2;
            let target_id = read_u32(bytes, offset)?;
            offset += 4;
            let task_semantic_sha256 = read_array::<32>(bytes, offset)?;
            offset += 32;
            let cti_sha256 = read_array::<32>(bytes, offset)?;
            offset += 32;

            let key = ContainerTaskIndexKey {
                runtime_image_sha256,
                task_oid,
            };
            let entry = ContainerTaskIndexEntry {
                cti_sha256,
                task_semantic_sha256,
                compiler_abi,
                target_id,
            };
            if entries.insert(key, entry).is_some() {
                return Err(
                    "Container Task index contains a duplicate RuntimeImage + TaskOID binding"
                        .to_string(),
                );
            }
        }
        Ok(Self { entries })
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_be_bytes(read_array::<2>(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_be_bytes(read_array::<4>(bytes, offset)?))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| "Container Task index range overflow".to_string())?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "Container Task index is truncated".to_string())?;
    slice
        .try_into()
        .map_err(|_| "Container Task index fixed-width decode failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc_protocol::container_task_image::{CtiSectionKind, CTI_SECTION_REQUIRED};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sample_sections(dependency_byte: u8) -> Vec<CtiSection> {
        CtiSectionKind::REQUIRED
            .into_iter()
            .map(|kind| CtiSection {
                kind: kind.code(),
                flags: CTI_SECTION_REQUIRED,
                data: if kind == CtiSectionKind::Dependencies {
                    vec![dependency_byte]
                } else {
                    vec![kind.code() as u8]
                },
            })
            .collect()
    }

    fn sample_input(semantic: u8, dependency: u8) -> ContainerTaskAssemblyInput {
        ContainerTaskAssemblyInput {
            task_oid: 31_844,
            capability_abi: 1,
            runtime_image_sha256: [7; 32],
            task_semantic_sha256: [semantic; 32],
            entry_node: 0,
            target_id: 9,
            flags: 0,
            sections: sample_sections(dependency),
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("rbe-cti-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn same_semantic_task_produces_same_bytes_and_hash() {
        let assembler = ContainerTaskAssembler::new(temp_root("deterministic"));
        let left = assembler.assemble(sample_input(1, 1)).unwrap();
        let right = assembler.assemble(sample_input(1, 1)).unwrap();
        assert_eq!(left.bytes, right.bytes);
        assert_eq!(left.cti_sha256, right.cti_sha256);
    }

    #[test]
    fn changed_dependency_changes_cti_identity() {
        let assembler = ContainerTaskAssembler::new(temp_root("dependency"));
        let left = assembler.assemble(sample_input(1, 1)).unwrap();
        let right = assembler.assemble(sample_input(2, 2)).unwrap();
        assert_ne!(left.cti_sha256, right.cti_sha256);
        assert_ne!(left.bytes, right.bytes);
    }

    #[test]
    fn index_round_trip_is_deterministic() {
        let mut index = ContainerTaskIndex::default();
        index.insert(
            ContainerTaskIndexKey {
                runtime_image_sha256: [2; 32],
                task_oid: 31_845,
            },
            ContainerTaskIndexEntry {
                cti_sha256: [4; 32],
                task_semantic_sha256: [3; 32],
                compiler_abi: CTI_COMPILER_ABI_VERSION,
                target_id: 8,
            },
        );
        index.insert(
            ContainerTaskIndexKey {
                runtime_image_sha256: [1; 32],
                task_oid: 31_844,
            },
            ContainerTaskIndexEntry {
                cti_sha256: [6; 32],
                task_semantic_sha256: [5; 32],
                compiler_abi: CTI_COMPILER_ABI_VERSION,
                target_id: 7,
            },
        );
        let encoded = index.encode().unwrap();
        let decoded = ContainerTaskIndex::decode(&encoded).unwrap();
        assert_eq!(decoded, index);
        assert_eq!(decoded.encode().unwrap(), encoded);
    }

    #[test]
    fn cache_can_be_deleted_and_rebuilt() {
        let root = temp_root("rebuild");
        let assembler = ContainerTaskAssembler::new(&root);
        let mut index = ContainerTaskIndex::default();
        let first = assembler
            .assemble_and_commit(&mut index, sample_input(1, 1))
            .unwrap();
        assert!(first.blob_path.is_file());
        assert!(assembler.index_path().is_file());

        fs::remove_dir_all(&root).unwrap();

        let assembler = ContainerTaskAssembler::new(&root);
        let mut rebuilt = ContainerTaskIndex::default();
        let second = assembler
            .assemble_and_commit(&mut rebuilt, sample_input(1, 1))
            .unwrap();
        assert_eq!(first.cti_sha256, second.cti_sha256);
        assert!(second.blob_path.is_file());
        assert_eq!(assembler.load_index().unwrap(), rebuilt);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remap_keeps_old_blob_and_updates_only_binding() {
        let root = temp_root("remap");
        let assembler = ContainerTaskAssembler::new(&root);
        let mut index = ContainerTaskIndex::default();
        let first = assembler
            .assemble_and_commit(&mut index, sample_input(1, 1))
            .unwrap();
        let second = assembler
            .assemble_and_commit(&mut index, sample_input(2, 2))
            .unwrap();

        assert_ne!(first.cti_sha256, second.cti_sha256);
        assert!(first.blob_path.is_file());
        assert!(second.blob_path.is_file());
        assert_eq!(
            index.get(&[7; 32], 31_844).unwrap().cti_sha256,
            second.cti_sha256
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_index_is_rejected() {
        let mut bytes = ContainerTaskIndex::default().encode().unwrap();
        bytes[0] ^= 0xff;
        assert!(ContainerTaskIndex::decode(&bytes).is_err());
    }
}
