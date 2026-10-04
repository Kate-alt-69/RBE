//! Immutable Container Task Image loading and RuntimeImage + TaskOID lookup.
//!
//! Phase 4 stops at validated addressing. This module deliberately does not
//! schedule CTI graph nodes or execute WASM/Service/QuickDB work; it only turns
//! Phase 3 cache entries into immutable, hash-verified Task images that the
//! Controller can address by `(RuntimeImage, TaskOID)`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use ipc_protocol::{
    cti_sha256, ContainerTaskImage, CAPABILITY_ABI_VERSION,
};

use crate::{
    ContainerTaskAssembler, ContainerTaskIndexEntry, CTI_COMPILER_ABI_VERSION,
};

const TASK_SLOT_COUNT: usize = u16::MAX as usize + 1;
const EMPTY_TASK_SLOT: u32 = u32::MAX;

#[derive(Debug)]
pub struct LoadedTaskImage {
    bytes: Arc<[u8]>,
    image: Arc<ContainerTaskImage>,
    cti_sha256: [u8; 32],
}

impl LoadedTaskImage {
    pub fn bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    pub fn image(&self) -> Arc<ContainerTaskImage> {
        Arc::clone(&self.image)
    }

    pub const fn cti_sha256(&self) -> [u8; 32] {
        self.cti_sha256
    }

    pub const fn task_oid(&self) -> u16 {
        self.image.header.task_oid
    }

    pub const fn runtime_image_sha256(&self) -> [u8; 32] {
        self.image.header.runtime_image_sha256
    }

    pub const fn task_semantic_sha256(&self) -> [u8; 32] {
        self.image.header.task_semantic_sha256
    }

    pub const fn target_id(&self) -> u32 {
        self.image.header.target_id
    }
}

#[derive(Debug)]
pub struct RuntimeImageTaskTable {
    runtime_image_sha256: [u8; 32],
    /// Direct TaskOID -> dense image index. `u32::MAX` means no Task at this OID.
    /// The fixed 65,536-entry table keeps the hot lookup independent of maps,
    /// source strings, service names, or artifact identifiers.
    slots: Box<[u32]>,
    images: Vec<Arc<LoadedTaskImage>>,
}

impl RuntimeImageTaskTable {
    fn new(
        runtime_image_sha256: [u8; 32],
        mut images: Vec<Arc<LoadedTaskImage>>,
    ) -> Result<Self, ContainerTaskLoadError> {
        images.sort_by_key(|image| image.task_oid());
        if images
            .windows(2)
            .any(|pair| pair[0].task_oid() == pair[1].task_oid())
        {
            return Err(ContainerTaskLoadError::InvalidIndex(
                "Runtime Image contains duplicate TaskOID entries".into(),
            ));
        }

        let mut slots = vec![EMPTY_TASK_SLOT; TASK_SLOT_COUNT].into_boxed_slice();
        for (dense_index, image) in images.iter().enumerate() {
            if image.runtime_image_sha256() != runtime_image_sha256 {
                return Err(ContainerTaskLoadError::IdentityMismatch(format!(
                    "TaskOID {} belongs to a different Runtime Image",
                    image.task_oid()
                )));
            }
            let dense_index = u32::try_from(dense_index).map_err(|_| {
                ContainerTaskLoadError::InvalidIndex(
                    "Runtime Image Task table exceeds u32 address space".into(),
                )
            })?;
            slots[usize::from(image.task_oid())] = dense_index;
        }

        Ok(Self {
            runtime_image_sha256,
            slots,
            images,
        })
    }

    pub const fn runtime_image_sha256(&self) -> [u8; 32] {
        self.runtime_image_sha256
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn lookup(&self, task_oid: u16) -> Option<Arc<LoadedTaskImage>> {
        let dense_index = self.slots[usize::from(task_oid)];
        if dense_index == EMPTY_TASK_SLOT {
            return None;
        }
        self.images.get(dense_index as usize).cloned()
    }

    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

#[derive(Clone)]
pub struct ContainerTaskLoader {
    cache_root: PathBuf,
    io: atomic_io::AtomicIo,
}

impl ContainerTaskLoader {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
            io: atomic_io::AtomicIo::new(),
        }
    }

    pub fn load_runtime_image(
        &self,
        runtime_image_sha256: [u8; 32],
    ) -> Result<Arc<RuntimeImageTaskTable>, ContainerTaskLoadError> {
        let assembler = ContainerTaskAssembler::new(&self.cache_root);
        let index = assembler
            .load_index()
            .map_err(ContainerTaskLoadError::InvalidIndex)?;

        let entries = index
            .iter()
            .filter(|(key, _)| key.runtime_image_sha256 == runtime_image_sha256)
            .map(|(key, entry)| (*key, entry.clone()))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Err(ContainerTaskLoadError::RuntimeImageNotFound(hex::encode(
                runtime_image_sha256,
            )));
        }

        let mut loaded = Vec::with_capacity(entries.len());
        for (key, entry) in entries {
            loaded.push(self.load_one(&assembler, key.task_oid, runtime_image_sha256, &entry)?);
        }
        Ok(Arc::new(RuntimeImageTaskTable::new(
            runtime_image_sha256,
            loaded,
        )?))
    }

    pub fn load_runtime_image_hex(
        &self,
        runtime_image: &str,
    ) -> Result<Arc<RuntimeImageTaskTable>, ContainerTaskLoadError> {
        self.load_runtime_image(decode_sha256(runtime_image)?)
    }

    fn load_one(
        &self,
        assembler: &ContainerTaskAssembler,
        task_oid: u16,
        runtime_image_sha256: [u8; 32],
        entry: &ContainerTaskIndexEntry,
    ) -> Result<Arc<LoadedTaskImage>, ContainerTaskLoadError> {
        if entry.compiler_abi != CTI_COMPILER_ABI_VERSION {
            return Err(ContainerTaskLoadError::AbiMismatch(format!(
                "TaskOID {task_oid} compiler ABI {} does not match {}",
                entry.compiler_abi, CTI_COMPILER_ABI_VERSION
            )));
        }

        let path = assembler.blob_path(&entry.cti_sha256);
        let bytes = self
            .io
            .read(&path)
            .map_err(|error| ContainerTaskLoadError::Io(format!(
                "failed to read {}: {error}",
                path.display()
            )))?;
        let observed_sha256 = cti_sha256(&bytes);
        if observed_sha256 != entry.cti_sha256 {
            return Err(ContainerTaskLoadError::HashMismatch(format!(
                "TaskOID {task_oid} CTI SHA-256 does not match the Container Task index"
            )));
        }

        let image = ContainerTaskImage::decode(&bytes).map_err(|error| {
            ContainerTaskLoadError::MalformedCti(format!("TaskOID {task_oid}: {error}"))
        })?;
        if image.header.runtime_image_sha256 != runtime_image_sha256 {
            return Err(ContainerTaskLoadError::IdentityMismatch(format!(
                "TaskOID {task_oid} CTI Runtime Image does not match its index key"
            )));
        }
        if image.header.task_oid != task_oid {
            return Err(ContainerTaskLoadError::IdentityMismatch(format!(
                "index TaskOID {task_oid} points at CTI TaskOID {}",
                image.header.task_oid
            )));
        }
        if image.header.task_semantic_sha256 != entry.task_semantic_sha256 {
            return Err(ContainerTaskLoadError::IdentityMismatch(format!(
                "TaskOID {task_oid} semantic SHA-256 does not match the index"
            )));
        }
        if image.header.target_id != entry.target_id {
            return Err(ContainerTaskLoadError::IdentityMismatch(format!(
                "TaskOID {task_oid} target identity does not match the index"
            )));
        }
        if image.header.capability_abi != CAPABILITY_ABI_VERSION {
            return Err(ContainerTaskLoadError::AbiMismatch(format!(
                "TaskOID {task_oid} capability ABI {} does not match Controller ABI {CAPABILITY_ABI_VERSION}",
                image.header.capability_abi
            )));
        }

        Ok(Arc::new(LoadedTaskImage {
            bytes: Arc::from(bytes),
            image: Arc::new(image),
            cti_sha256: observed_sha256,
        }))
    }
}

#[derive(Clone, Default)]
pub struct RuntimeImageTaskRegistry {
    tables: Arc<RwLock<BTreeMap<[u8; 32], Arc<RuntimeImageTaskTable>>>>,
}

impl RuntimeImageTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the complete immutable table first, then replace the active table
    /// in one short write-lock operation. Existing callers retain their Arcs,
    /// so a hot cutover never invalidates an in-flight old Task image.
    pub fn install_from_cache(
        &self,
        loader: &ContainerTaskLoader,
        runtime_image_sha256: [u8; 32],
    ) -> Result<Arc<RuntimeImageTaskTable>, ContainerTaskLoadError> {
        let table = loader.load_runtime_image(runtime_image_sha256)?;
        self.install_table(Arc::clone(&table));
        Ok(table)
    }

    pub fn install_table(
        &self,
        table: Arc<RuntimeImageTaskTable>,
    ) -> Option<Arc<RuntimeImageTaskTable>> {
        let key = table.runtime_image_sha256();
        write_unpoisoned(&self.tables).insert(key, table)
    }

    pub fn lookup(
        &self,
        runtime_image_sha256: &[u8; 32],
        task_oid: u16,
    ) -> Option<Arc<LoadedTaskImage>> {
        read_unpoisoned(&self.tables)
            .get(runtime_image_sha256)
            .and_then(|table| table.lookup(task_oid))
    }

    pub fn lookup_hex(
        &self,
        runtime_image: &str,
        task_oid: u16,
    ) -> Result<Option<Arc<LoadedTaskImage>>, ContainerTaskLoadError> {
        let runtime_image_sha256 = decode_sha256(runtime_image)?;
        Ok(self.lookup(&runtime_image_sha256, task_oid))
    }

    pub fn remove(
        &self,
        runtime_image_sha256: &[u8; 32],
    ) -> Option<Arc<RuntimeImageTaskTable>> {
        write_unpoisoned(&self.tables).remove(runtime_image_sha256)
    }
}

fn read_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn decode_sha256(value: &str) -> Result<[u8; 32], ContainerTaskLoadError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ContainerTaskLoadError::InvalidRuntimeImage(
            "Runtime Image identity must be 64 hexadecimal characters".into(),
        ));
    }
    let bytes = hex::decode(value).map_err(|error| {
        ContainerTaskLoadError::InvalidRuntimeImage(format!(
            "failed to decode Runtime Image SHA-256: {error}"
        ))
    })?;
    bytes.try_into().map_err(|_| {
        ContainerTaskLoadError::InvalidRuntimeImage(
            "Runtime Image identity must contain exactly 32 bytes".into(),
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerTaskLoadError {
    InvalidRuntimeImage(String),
    RuntimeImageNotFound(String),
    InvalidIndex(String),
    Io(String),
    HashMismatch(String),
    MalformedCti(String),
    IdentityMismatch(String),
    AbiMismatch(String),
}

impl fmt::Display for ContainerTaskLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRuntimeImage(message) => write!(formatter, "invalid Runtime Image: {message}"),
            Self::RuntimeImageNotFound(image) => {
                write!(formatter, "Runtime Image {image} has no Container Task index entries")
            }
            Self::InvalidIndex(message) => write!(formatter, "invalid Container Task index: {message}"),
            Self::Io(message) => formatter.write_str(message),
            Self::HashMismatch(message) => write!(formatter, "CTI hash mismatch: {message}"),
            Self::MalformedCti(message) => write!(formatter, "malformed CTI: {message}"),
            Self::IdentityMismatch(message) => write!(formatter, "CTI identity mismatch: {message}"),
            Self::AbiMismatch(message) => write!(formatter, "CTI ABI mismatch: {message}"),
        }
    }
}

impl std::error::Error for ContainerTaskLoadError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ipc_protocol::{CtiSection, CtiSectionKind, CTI_SECTION_REQUIRED};

    use super::*;
    use crate::{ContainerTaskAssemblyInput, ContainerTaskIndexEntry};

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-cti-loader-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn required_sections(marker: u8) -> Vec<CtiSection> {
        CtiSectionKind::REQUIRED
            .into_iter()
            .map(|kind| CtiSection {
                kind: kind.code(),
                flags: CTI_SECTION_REQUIRED,
                data: vec![marker, kind.code() as u8],
            })
            .collect()
    }

    fn input(
        runtime_image_sha256: [u8; 32],
        task_oid: u16,
        semantic: u8,
        capability_abi: u16,
    ) -> ContainerTaskAssemblyInput {
        ContainerTaskAssemblyInput {
            task_oid,
            capability_abi,
            runtime_image_sha256,
            task_semantic_sha256: [semantic; 32],
            entry_node: 0,
            target_id: 77,
            flags: 0,
            sections: required_sections(semantic),
        }
    }

    fn commit(
        assembler: &ContainerTaskAssembler,
        input: ContainerTaskAssemblyInput,
    ) -> crate::ContainerTaskCacheCommit {
        let mut index = assembler.load_index().unwrap();
        assembler.assemble_and_commit(&mut index, input).unwrap()
    }

    #[test]
    fn loader_builds_fixed_oid_table_and_resolves_task() {
        let root = temp_root("lookup");
        let runtime_image = [7; 32];
        let assembler = ContainerTaskAssembler::new(&root);
        commit(
            &assembler,
            input(runtime_image, 31_844, 1, CAPABILITY_ABI_VERSION),
        );

        let table = ContainerTaskLoader::new(&root)
            .load_runtime_image(runtime_image)
            .unwrap();
        assert_eq!(table.slot_count(), TASK_SLOT_COUNT);
        assert_eq!(table.len(), 1);
        assert_eq!(table.lookup(31_844).unwrap().task_oid(), 31_844);
        assert!(table.lookup(31_845).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_cti_blob_is_rejected_before_decode() {
        let root = temp_root("corrupt");
        let runtime_image = [8; 32];
        let assembler = ContainerTaskAssembler::new(&root);
        let committed = commit(
            &assembler,
            input(runtime_image, 31_844, 2, CAPABILITY_ABI_VERSION),
        );
        fs::write(&committed.blob_path, b"not-a-cti").unwrap();

        let error = ContainerTaskLoader::new(&root)
            .load_runtime_image(runtime_image)
            .unwrap_err();
        assert!(matches!(error, ContainerTaskLoadError::HashMismatch(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn index_to_header_semantic_mismatch_is_rejected() {
        let root = temp_root("semantic-mismatch");
        let runtime_image = [9; 32];
        let assembler = ContainerTaskAssembler::new(&root);
        commit(
            &assembler,
            input(runtime_image, 31_844, 3, CAPABILITY_ABI_VERSION),
        );
        let mut index = assembler.load_index().unwrap();
        let (key, original) = index
            .iter()
            .next()
            .map(|(key, entry)| (*key, entry.clone()))
            .unwrap();
        index.insert(
            key,
            ContainerTaskIndexEntry {
                task_semantic_sha256: [99; 32],
                ..original
            },
        );
        assembler.replace_index(&index).unwrap();

        let error = ContainerTaskLoader::new(&root)
            .load_runtime_image(runtime_image)
            .unwrap_err();
        assert!(matches!(
            error,
            ContainerTaskLoadError::IdentityMismatch(_)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_capability_abi_is_rejected() {
        let root = temp_root("abi");
        let runtime_image = [10; 32];
        let assembler = ContainerTaskAssembler::new(&root);
        commit(
            &assembler,
            input(
                runtime_image,
                31_844,
                4,
                CAPABILITY_ABI_VERSION.saturating_add(1),
            ),
        );

        let error = ContainerTaskLoader::new(&root)
            .load_runtime_image(runtime_image)
            .unwrap_err();
        assert!(matches!(error, ContainerTaskLoadError::AbiMismatch(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hot_cutover_retains_old_loaded_task_arc() {
        let root = temp_root("cutover");
        let runtime_image = [11; 32];
        let assembler = ContainerTaskAssembler::new(&root);
        commit(
            &assembler,
            input(runtime_image, 31_844, 5, CAPABILITY_ABI_VERSION),
        );
        let loader = ContainerTaskLoader::new(&root);
        let registry = RuntimeImageTaskRegistry::new();
        registry
            .install_from_cache(&loader, runtime_image)
            .unwrap();
        let old_task = registry.lookup(&runtime_image, 31_844).unwrap();
        let old_hash = old_task.cti_sha256();

        commit(
            &assembler,
            input(runtime_image, 31_844, 6, CAPABILITY_ABI_VERSION),
        );
        registry
            .install_from_cache(&loader, runtime_image)
            .unwrap();
        let new_task = registry.lookup(&runtime_image, 31_844).unwrap();

        assert_ne!(old_hash, new_task.cti_sha256());
        assert_eq!(old_task.cti_sha256(), old_hash);
        assert_eq!(old_task.task_semantic_sha256(), [5; 32]);
        assert_eq!(new_task.task_semantic_sha256(), [6; 32]);
        let _ = fs::remove_dir_all(root);
    }
}
