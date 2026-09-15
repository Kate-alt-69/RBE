use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::format::{BlobBody, BlobKind, BlobManifest, FolderEntry};
use crate::store::CloudNodeStore;
use crate::sync::{SyncPlan, SyncPlanHeader};
use crate::transfer::{TransferChunk, TransferResource};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReceipt {
    pub committed: bool,
    pub duplicate: bool,
    pub video_reconstructed: bool,
    /// First byte the sender still needs to transmit for this resource.
    /// A completed resource reports `total_size` so a reconnect can skip it.
    pub next_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceIdentity {
    kind: BlobKind,
    resource: TransferResource,
    object_key: [u8; 32],
    content_sha256: [u8; 32],
    resource_sha256: [u8; 32],
    total_size: u64,
}

impl From<&TransferChunk> for ResourceIdentity {
    fn from(chunk: &TransferChunk) -> Self {
        Self {
            kind: chunk.kind,
            resource: chunk.resource,
            object_key: chunk.object_key,
            content_sha256: chunk.content_sha256,
            resource_sha256: chunk.resource_sha256,
            total_size: chunk.total_size,
        }
    }
}

#[derive(Debug)]
struct ActiveResource {
    identity: ResourceIdentity,
    path: PathBuf,
    written: u64,
}

/// Receives a complete authenticated Cloud Node snapshot into a private staging
/// tree. The live `storage/` tree is not touched until `complete()` proves the
/// staged snapshot has the exact negotiated sync root.
///
/// This is intentionally a full-snapshot recovery primitive. A root mismatch
/// must not merge remote data into an older local tree because stale local
/// objects would survive and the resulting root would no longer represent the
/// sending node.
pub struct CloudNodeRecoveryReceiver {
    session_root: PathBuf,
    staging_storage: PathBuf,
    phase: u8,
    active: Option<ActiveResource>,
    completed: bool,
}

impl CloudNodeRecoveryReceiver {
    pub fn open(
        store: &CloudNodeStore,
        owner_node_id: &str,
        expected: SyncPlanHeader,
    ) -> anyhow::Result<Self> {
        let summary = store.summary();
        let peer_root = summary
            .root
            .join("recovery-staging")
            .join(recovery_peer_key(owner_node_id));
        fs::create_dir_all(&peer_root)?;
        let recovery_name = recovery_plan_key(expected);
        prune_obsolete_peer_recoveries(&peer_root, &recovery_name)?;
        let session_root = peer_root.join(&recovery_name);
        let staging_storage = session_root.join("storage");
        fs::create_dir_all(&staging_storage)?;
        let phase = infer_recovery_phase(&staging_storage)?;
        Ok(Self {
            session_root,
            staging_storage,
            phase,
            active: None,
            completed: false,
        })
    }

    /// Removes an obsolete private staging tree. This is used only after
    /// the same authenticated peer negotiates a different snapshot root.
    pub fn discard(self) -> anyhow::Result<()> {
        if self.session_root.exists() {
            fs::remove_dir_all(&self.session_root)?;
        }
        Ok(())
    }

    pub fn accept_chunk(&mut self, chunk: &TransferChunk) -> anyhow::Result<RecoveryReceipt> {
        if self.completed {
            anyhow::bail!("Cloud Node recovery session is already complete");
        }
        chunk.validate()?;

        // A reauthenticated sender starts its ordered walk at the beginning.
        // Recognize already-committed staged resources before enforcing the
        // current phase so reconnects can cheaply replay/skip earlier objects.
        let target = self.resource_target(chunk);
        if target.is_file() && verify_resource(&target, chunk.resource_sha256, chunk.total_size)? {
            let part = transfer_part_path(&target)?;
            if part.exists() {
                fs::remove_file(part)?;
            }
            let video_reconstructed = self.after_resource_committed(chunk, &target)?;
            return Ok(RecoveryReceipt {
                committed: true,
                duplicate: true,
                video_reconstructed,
                next_offset: chunk.total_size,
            });
        }

        self.enforce_recovery_order(chunk)?;
        self.enforce_resource_dependency(chunk)?;
        let identity = ResourceIdentity::from(chunk);
        if self.active.is_none() {
            if chunk.offset != 0 {
                anyhow::bail!("Cloud Node recovery resource did not begin at offset zero");
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let part = transfer_part_path(&target)?;
            let written = match fs::metadata(&part) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= chunk.total_size => {
                    metadata.len()
                }
                Ok(_) => {
                    if part.exists() {
                        fs::remove_file(&part)?;
                    }
                    let file = File::create(&part)?;
                    file.sync_all()?;
                    0
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let file = File::create(&part)?;
                    file.sync_all()?;
                    0
                }
                Err(error) => return Err(error.into()),
            };
            self.active = Some(ActiveResource {
                identity: identity.clone(),
                path: part,
                written,
            });
        }

        let active = self
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Cloud Node recovery resource state disappeared"))?;
        if active.identity != identity {
            anyhow::bail!("Cloud Node recovery interleaved two resource streams");
        }

        let mut duplicate = write_or_replay_recovery_chunk(active, chunk)?;

        // A crash may happen after the full .transfer.part was fsynced
        // but before it was verified and renamed. A replay probe must finish
        // that commit before it can advertise total_size as durable.
        if active.written < chunk.total_size {
            return Ok(RecoveryReceipt {
                committed: false,
                duplicate,
                video_reconstructed: false,
                next_offset: active.written,
            });
        }
        if active.written != chunk.total_size {
            anyhow::bail!("Cloud Node recovery final chunk did not complete the resource");
        }
        if !verify_resource(&active.path, chunk.resource_sha256, chunk.total_size)? {
            reset_active_resource(active)?;
            anyhow::bail!("Cloud Node recovery resource hash mismatch; durable partial was reset");
        }
        validate_staged_resource(&active.path, chunk, &self.staging_storage)?;

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.exists() {
            if verify_resource(&target, chunk.resource_sha256, chunk.total_size)? {
                fs::remove_file(&active.path)?;
                duplicate = true;
            } else {
                fs::remove_file(&target)?;
                fs::rename(&active.path, &target)?;
                duplicate = false;
            }
        } else {
            fs::rename(&active.path, &target)?;
        }
        self.active = None;

        let video_reconstructed = self.after_resource_committed(chunk, &target)?;
        Ok(RecoveryReceipt {
            committed: true,
            duplicate,
            video_reconstructed,
            next_offset: chunk.total_size,
        })
    }

    /// Verifies the fully staged snapshot against the authenticated sender's
    /// negotiated root and only then swaps it into the live Cloud Node store.
    /// If verification fails after the swap, the previous live tree is restored.
    pub fn complete(
        &mut self,
        store: &CloudNodeStore,
        expected: SyncPlanHeader,
    ) -> anyhow::Result<SyncPlanHeader> {
        if self.completed {
            anyhow::bail!("Cloud Node recovery session is already complete");
        }
        if self.active.is_some() || contains_transfer_part(&self.staging_storage)? {
            anyhow::bail!("Cloud Node recovery cannot complete with a partial resource");
        }

        CloudNodeStore::verify_storage_path(&self.staging_storage)?;
        let staged = SyncPlan::scan_storage(&self.staging_storage)?.header()?;
        if staged != expected {
            anyhow::bail!(
                "Cloud Node staged recovery root mismatch: expected {}, got {}",
                hex::encode(expected.root_sha256),
                hex::encode(staged.root_sha256)
            );
        }

        let summary = store.summary();
        let live = summary.storage;
        let old = summary.root.join("recovery-previous-storage");
        if old.exists() {
            anyhow::bail!(
                "Cloud Node cannot start a storage swap while previous recovery storage exists"
            );
        }

        let had_live = live.exists();
        if had_live {
            fs::rename(&live, &old)?;
        }
        if let Err(error) = fs::rename(&self.staging_storage, &live) {
            if had_live && old.exists() {
                let _ = fs::rename(&old, &live);
            }
            return Err(error.into());
        }

        let actual = match store.verify().and_then(|_| store.sync_plan()?.header()) {
            Ok(actual) => actual,
            Err(error) => {
                self.rollback_swap(&live, &old, had_live);
                return Err(error);
            }
        };
        if actual != expected {
            self.rollback_swap(&live, &old, had_live);
            anyhow::bail!(
                "Cloud Node activated recovery root mismatch: expected {}, got {}",
                hex::encode(expected.root_sha256),
                hex::encode(actual.root_sha256)
            );
        }

        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        self.completed = true;
        if self.session_root.exists() {
            fs::remove_dir_all(&self.session_root)?;
        }
        Ok(actual)
    }

    fn rollback_swap(&mut self, live: &Path, old: &Path, had_live: bool) {
        if live.exists() {
            let _ = fs::rename(live, &self.staging_storage);
        }
        if had_live && old.exists() {
            let _ = fs::rename(old, live);
        }
    }

    fn enforce_recovery_order(&mut self, chunk: &TransferChunk) -> anyhow::Result<()> {
        let phase = match chunk.kind {
            BlobKind::Folder => 0,
            BlobKind::Video => 1,
            BlobKind::File => 2,
        };
        if phase < self.phase {
            anyhow::bail!("Cloud Node recovery object order regressed");
        }
        if phase > self.phase {
            if self.active.is_some() {
                anyhow::bail!("Cloud Node recovery changed phase during a partial resource");
            }
            self.phase = phase;
        }
        Ok(())
    }

    fn enforce_resource_dependency(&self, chunk: &TransferChunk) -> anyhow::Result<()> {
        if chunk.resource == TransferResource::Manifest {
            return Ok(());
        }
        let manifest = self
            .staging_storage
            .join(hex::encode(chunk.object_key))
            .join(chunk.kind.manifest_name());
        if !manifest.is_file() {
            anyhow::bail!("Cloud Node recovery payload arrived before its manifest");
        }
        Ok(())
    }

    fn resource_target(&self, chunk: &TransferChunk) -> PathBuf {
        let object = self.staging_storage.join(hex::encode(chunk.object_key));
        match chunk.resource {
            TransferResource::Manifest => object.join(chunk.kind.manifest_name()),
            TransferResource::FilePayload => object
                .join("versions")
                .join(hex::encode(chunk.content_sha256))
                .join("payload"),
            TransferResource::VideoChunk => object
                .join("chunks")
                .join(format!("{}.chunk", hex::encode(chunk.resource_sha256))),
        }
    }

    fn after_resource_committed(
        &self,
        chunk: &TransferChunk,
        target: &Path,
    ) -> anyhow::Result<bool> {
        if chunk.resource == TransferResource::Manifest && chunk.kind == BlobKind::Folder {
            let version = self
                .staging_storage
                .join(hex::encode(chunk.object_key))
                .join("versions")
                .join(hex::encode(chunk.content_sha256))
                .join(BlobKind::Folder.manifest_name());
            if let Some(parent) = version.parent() {
                fs::create_dir_all(parent)?;
            }
            if !version.exists() {
                copy_file_via_part(target, &version)?;
            }
        }

        if chunk.kind == BlobKind::Video
            && matches!(
                chunk.resource,
                TransferResource::Manifest | TransferResource::VideoChunk
            )
        {
            return self.reconstruct_video_payload(chunk.object_key, chunk.content_sha256);
        }
        Ok(false)
    }

    fn reconstruct_video_payload(
        &self,
        object_key: [u8; 32],
        content_sha256: [u8; 32],
    ) -> anyhow::Result<bool> {
        let object = self.staging_storage.join(hex::encode(object_key));
        let manifest_path = object.join(BlobKind::Video.manifest_name());
        if !manifest_path.is_file() {
            return Ok(false);
        }
        let manifest = BlobManifest::decode(&fs::read(&manifest_path)?)?;
        validate_manifest(&manifest)?;
        if manifest.kind != BlobKind::Video
            || manifest.object_key != object_key
            || manifest.content_sha256 != content_sha256
        {
            anyhow::bail!("Cloud Node recovery video manifest identity mismatch");
        }
        let BlobBody::Video { chunks, .. } = &manifest.body else {
            anyhow::bail!("Cloud Node recovery expected a video manifest");
        };
        for chunk in chunks {
            let path = object
                .join("chunks")
                .join(format!("{}.chunk", hex::encode(chunk.sha256)));
            if !path.is_file() {
                return Ok(false);
            }
            if !verify_resource(&path, chunk.sha256, u64::from(chunk.len))? {
                anyhow::bail!("Cloud Node recovery video chunk failed verification");
            }
        }

        let payload = object
            .join("versions")
            .join(hex::encode(content_sha256))
            .join("payload");
        if payload.is_file() && verify_resource(&payload, content_sha256, manifest.logical_size)? {
            return Ok(true);
        }
        if let Some(parent) = payload.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = part_path(&payload)?;
        if temp.exists() {
            fs::remove_file(&temp)?;
        }

        let result = (|| -> anyhow::Result<()> {
            let mut output = File::create(&temp)?;
            let mut digest = Sha256::new();
            let mut written = 0u64;
            let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
            for chunk in chunks {
                let path = object
                    .join("chunks")
                    .join(format!("{}.chunk", hex::encode(chunk.sha256)));
                let mut input = File::open(path)?;
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                    output.write_all(&buffer[..read])?;
                    written = written.saturating_add(read as u64);
                }
            }
            output.flush()?;
            output.sync_all()?;
            let actual: [u8; 32] = digest.finalize().into();
            if written != manifest.logical_size || actual != content_sha256 {
                anyhow::bail!("Cloud Node reconstructed video payload hash mismatch");
            }
            fs::rename(&temp, &payload)?;
            Ok(())
        })();
        if result.is_err() && temp.exists() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        Ok(true)
    }
}

fn recovery_peer_key(owner_node_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-RECOVERY-PEER/1\0");
    digest.update(owner_node_id.as_bytes());
    hex::encode(digest.finalize())
}

fn recovery_plan_key(expected: SyncPlanHeader) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-RECOVERY-PLAN/1\0");
    digest.update(expected.encode());
    hex::encode(digest.finalize())
}

fn prune_obsolete_peer_recoveries(peer_root: &Path, keep: &str) -> anyhow::Result<()> {
    for entry in fs::read_dir(peer_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy() == keep {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

fn recovery_phase(kind: BlobKind) -> u8 {
    match kind {
        BlobKind::Folder => 0,
        BlobKind::Video => 1,
        BlobKind::File => 2,
    }
}

fn infer_recovery_phase(staging_storage: &Path) -> anyhow::Result<u8> {
    let mut phase = 0u8;
    for entry in fs::read_dir(staging_storage)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for kind in [BlobKind::Folder, BlobKind::Video, BlobKind::File] {
            if entry.path().join(kind.manifest_name()).is_file() {
                phase = phase.max(recovery_phase(kind));
            }
        }
    }
    Ok(phase)
}

fn transfer_part_path(target: &Path) -> anyhow::Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node recovery target has no file name: {}",
                target.display()
            )
        })?
        .to_string_lossy();
    Ok(target.with_file_name(format!("{name}.transfer.part")))
}

fn contains_transfer_part(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if contains_transfer_part(&entry.path())? {
                return Ok(true);
            }
        } else if entry
            .file_name()
            .to_string_lossy()
            .ends_with(".transfer.part")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reset_active_resource(active: &mut ActiveResource) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&active.path)?;
    file.sync_all()?;
    active.written = 0;
    Ok(())
}

fn write_or_replay_recovery_chunk(
    active: &mut ActiveResource,
    chunk: &TransferChunk,
) -> anyhow::Result<bool> {
    let data_len = u64::try_from(chunk.data.len())
        .map_err(|_| anyhow::anyhow!("Cloud Node recovery chunk size exceeds u64"))?;
    let end = chunk
        .offset
        .checked_add(data_len)
        .ok_or_else(|| anyhow::anyhow!("Cloud Node recovery chunk range overflow"))?;
    if chunk.offset > active.written {
        anyhow::bail!("Cloud Node recovery chunk is not contiguous");
    }

    if chunk.offset < active.written {
        if end <= active.written {
            if verify_part_range(&active.path, chunk.offset, &chunk.data).is_ok() {
                return Ok(true);
            }
            reset_active_resource(active)?;
            if chunk.offset != 0 {
                anyhow::bail!("Cloud Node durable recovery prefix was corrupt and was reset");
            }
        } else {
            let overlap = usize::try_from(active.written - chunk.offset)
                .map_err(|_| anyhow::anyhow!("Cloud Node recovery overlap exceeds usize"))?;
            if verify_part_range(&active.path, chunk.offset, &chunk.data[..overlap]).is_err() {
                reset_active_resource(active)?;
                if chunk.offset != 0 {
                    anyhow::bail!("Cloud Node durable recovery overlap was corrupt and was reset");
                }
            } else {
                let mut file = OpenOptions::new().write(true).open(&active.path)?;
                file.set_len(chunk.offset)?;
                file.seek(SeekFrom::End(0))?;
                file.write_all(&chunk.data)?;
                file.sync_data()?;
                active.written = end;
                return Ok(false);
            }
        }
    }

    if chunk.offset != active.written {
        anyhow::bail!("Cloud Node recovery retry must restart from offset zero");
    }
    let mut file = OpenOptions::new().append(true).open(&active.path)?;
    file.write_all(&chunk.data)?;
    file.sync_data()?;
    active.written = end;
    Ok(false)
}

fn part_path(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node recovery target has no file name: {}",
                path.display()
            )
        })?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{name}.part.{}", std::process::id())))
}

fn copy_file_via_part(source: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = part_path(target)?;
    if part.exists() {
        fs::remove_file(&part)?;
    }
    fs::copy(source, &part)?;
    File::open(&part)?.sync_all()?;
    if target.exists() {
        fs::remove_file(target)?;
    }
    fs::rename(part, target)?;
    Ok(())
}

fn validate_staged_resource(
    path: &Path,
    chunk: &TransferChunk,
    staging_storage: &Path,
) -> anyhow::Result<()> {
    match chunk.resource {
        TransferResource::Manifest => {
            let manifest = BlobManifest::decode(&fs::read(path)?)?;
            validate_manifest(&manifest)?;
            if manifest.kind != chunk.kind
                || manifest.object_key != chunk.object_key
                || manifest.content_sha256 != chunk.content_sha256
            {
                anyhow::bail!("Cloud Node recovery manifest identity mismatch");
            }
        }
        TransferResource::FilePayload => {
            let manifest = load_staged_manifest(staging_storage, chunk)?;
            if manifest.kind != BlobKind::File
                || manifest.content_sha256 != chunk.content_sha256
                || manifest.logical_size != chunk.total_size
            {
                anyhow::bail!("Cloud Node recovery file payload does not match its manifest");
            }
        }
        TransferResource::VideoChunk => {
            let manifest = load_staged_manifest(staging_storage, chunk)?;
            let BlobBody::Video { chunks, .. } = manifest.body else {
                anyhow::bail!("Cloud Node recovery video chunk does not have a video manifest");
            };
            if !chunks.iter().any(|entry| {
                entry.sha256 == chunk.resource_sha256 && u64::from(entry.len) == chunk.total_size
            }) {
                anyhow::bail!("Cloud Node recovery video chunk is not declared by its manifest");
            }
        }
    }
    Ok(())
}

fn load_staged_manifest(
    staging_storage: &Path,
    chunk: &TransferChunk,
) -> anyhow::Result<BlobManifest> {
    let path = staging_storage
        .join(hex::encode(chunk.object_key))
        .join(chunk.kind.manifest_name());
    let manifest = BlobManifest::decode(&fs::read(path)?)?;
    validate_manifest(&manifest)?;
    if manifest.object_key != chunk.object_key {
        anyhow::bail!("Cloud Node recovery payload object key does not match its manifest");
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &BlobManifest) -> anyhow::Result<()> {
    validate_logical_path(&manifest.logical_path)?;
    if manifest.object_key != object_key(manifest.kind, &manifest.logical_path) {
        anyhow::bail!("Cloud Node recovery manifest object key is not canonical");
    }

    match &manifest.body {
        BlobBody::File { changes } => {
            for change in changes {
                change
                    .offset
                    .checked_add(change.old_len)
                    .ok_or_else(|| anyhow::anyhow!("Cloud Node file change range overflow"))?;
            }
        }
        BlobBody::Video {
            chunk_bytes,
            chunks,
        } => {
            if *chunk_bytes == 0 && !chunks.is_empty() {
                anyhow::bail!("Cloud Node video manifest uses zero-sized chunks");
            }
            let mut expected_offset = 0u64;
            for chunk in chunks {
                if chunk.offset != expected_offset || chunk.len == 0 || chunk.len > *chunk_bytes {
                    anyhow::bail!("Cloud Node video manifest chunk layout is invalid");
                }
                expected_offset = expected_offset
                    .checked_add(u64::from(chunk.len))
                    .ok_or_else(|| anyhow::anyhow!("Cloud Node video chunk range overflow"))?;
            }
            if expected_offset != manifest.logical_size {
                anyhow::bail!("Cloud Node video manifest size does not match its chunk layout");
            }
        }
        BlobBody::Folder { entries } => {
            let mut previous: Option<&str> = None;
            let mut logical_size = 0u64;
            for entry in entries {
                validate_folder_entry(entry)?;
                if previous.is_some_and(|path| path >= entry.path.as_str()) {
                    anyhow::bail!("Cloud Node folder manifest entries are not strictly ordered");
                }
                previous = Some(&entry.path);
                logical_size = logical_size
                    .checked_add(entry.size)
                    .ok_or_else(|| anyhow::anyhow!("Cloud Node folder logical size overflow"))?;
            }
            if logical_size != manifest.logical_size {
                anyhow::bail!("Cloud Node folder logical size does not match its entries");
            }
            if folder_digest(entries) != manifest.content_sha256 {
                anyhow::bail!("Cloud Node folder manifest content hash mismatch");
            }
        }
    }
    Ok(())
}

fn validate_folder_entry(entry: &FolderEntry) -> anyhow::Result<()> {
    validate_logical_path(&entry.path)?;
    if entry.object_key != object_key(entry.kind, &entry.path) {
        anyhow::bail!("Cloud Node folder entry object key is not canonical");
    }
    if entry.kind == BlobKind::Folder && (entry.content_sha256 != [0u8; 32] || entry.size != 0) {
        anyhow::bail!("Cloud Node folder entry directory metadata is invalid");
    }
    Ok(())
}

fn validate_logical_path(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains(':')
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("invalid canonical Cloud Node logical path {value:?}");
    }
    Ok(())
}

fn object_key(kind: BlobKind, logical_path: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-OBJECT/1\0");
    digest.update([kind as u8]);
    digest.update(logical_path.as_bytes());
    digest.finalize().into()
}

fn folder_digest(entries: &[FolderEntry]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-FOLDER/1\0");
    for entry in entries {
        digest.update([entry.kind as u8]);
        digest.update((entry.path.len() as u64).to_be_bytes());
        digest.update(entry.path.as_bytes());
        digest.update(entry.object_key);
        digest.update(entry.content_sha256);
        digest.update(entry.size.to_be_bytes());
    }
    digest.finalize().into()
}

fn verify_part_range(path: &Path, offset: u64, expected: &[u8]) -> anyhow::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut actual = vec![0u8; expected.len()];
    file.read_exact(&mut actual)?;
    if actual != expected {
        anyhow::bail!("Cloud Node recovery retry bytes do not match staged data");
    }
    Ok(())
}

fn verify_resource(
    path: &Path,
    expected_hash: [u8; 32],
    expected_size: u64,
) -> anyhow::Result<bool> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != expected_size {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(<[u8; 32]>::from(digest.finalize()) == expected_hash)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::CloudNodeSettings;
    use crate::sync::SyncObject;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-recovery-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn settings(root: &Path, id: &str) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": id,
                "storageRoot": root,
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            }
        }))
        .unwrap()
    }

    fn feed_resource(
        receiver: &mut CloudNodeRecoveryReceiver,
        object: &SyncObject,
        resource: TransferResource,
        path: &Path,
        resource_hash: [u8; 32],
    ) {
        let bytes = fs::read(path).unwrap();
        let chunk = TransferChunk::new(
            object.kind,
            resource,
            object.object_key,
            object.content_sha256,
            resource_hash,
            0,
            bytes.len() as u64,
            bytes,
        )
        .unwrap();
        receiver.accept_chunk(&chunk).unwrap();
    }

    #[test]
    fn recovery_swaps_only_after_exact_root_matches() {
        let source_root = test_root("source");
        let destination_root = test_root("destination");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = CloudNodeStore::open(&settings(&source_root, "source")).unwrap();
        let destination =
            CloudNodeStore::open(&settings(&destination_root, "destination")).unwrap();

        let tree = source_root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.txt"), b"folder topology").unwrap();
        source.snapshot_folder(&tree, "root").unwrap();

        let video = source_root.join("clip.mp4");
        fs::write(&video, vec![5u8; 1024 * 1024 + 7]).unwrap();
        source.store_video(&video, "video/clip.mp4").unwrap();

        let file = source_root.join("users.db");
        fs::write(&file, b"database bytes").unwrap();
        source.store_file(&file, "db/users.db").unwrap();

        let stale = destination_root.join("stale.db");
        fs::write(&stale, b"stale").unwrap();
        destination.store_file(&stale, "db/stale.db").unwrap();

        let plan = source.sync_plan().unwrap();
        let expected = plan.header().unwrap();
        let mut receiver =
            CloudNodeRecoveryReceiver::open(&destination, "source", expected).unwrap();
        for object in plan.ordered() {
            let manifest_bytes = fs::read(&object.manifest_path).unwrap();
            let manifest_hash: [u8; 32] = Sha256::digest(&manifest_bytes).into();
            feed_resource(
                &mut receiver,
                object,
                TransferResource::Manifest,
                &object.manifest_path,
                manifest_hash,
            );
            match object.kind {
                BlobKind::Folder => {}
                BlobKind::Video => {
                    for chunk_path in &object.chunk_paths {
                        let bytes = fs::read(chunk_path).unwrap();
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();
                        feed_resource(
                            &mut receiver,
                            object,
                            TransferResource::VideoChunk,
                            chunk_path,
                            hash,
                        );
                    }
                }
                BlobKind::File => feed_resource(
                    &mut receiver,
                    object,
                    TransferResource::FilePayload,
                    object.payload_path.as_deref().unwrap(),
                    object.content_sha256,
                ),
            }
        }

        assert_eq!(receiver.complete(&destination, expected).unwrap(), expected);
        assert_eq!(destination.sync_plan().unwrap().header().unwrap(), expected);
        assert_eq!(destination.sync_plan().unwrap().object_count(), 3);

        fs::remove_dir_all(source_root).unwrap();
        fs::remove_dir_all(destination_root).unwrap();
    }

    #[test]
    fn recovery_allows_completed_earlier_resource_replay() {
        let root = test_root("resume-replay");
        fs::create_dir_all(&root).unwrap();
        let store = CloudNodeStore::open(&settings(&root, "resume-replay")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [6u8; 32],
            folder_count: 1,
            video_count: 0,
            file_count: 1,
        };
        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "resume-peer", expected).unwrap();

        let folder_manifest = BlobManifest {
            kind: BlobKind::Folder,
            object_key: object_key(BlobKind::Folder, "root"),
            content_sha256: folder_digest(&[]),
            parent_content_sha256: None,
            logical_path: "root".into(),
            logical_size: 0,
            created_unix_ms: 1,
            body: BlobBody::Folder {
                entries: Vec::new(),
            },
        };
        let folder_bytes = folder_manifest.encode().unwrap();
        let folder_hash: [u8; 32] = Sha256::digest(&folder_bytes).into();
        let folder_chunk = TransferChunk::new(
            BlobKind::Folder,
            TransferResource::Manifest,
            folder_manifest.object_key,
            folder_manifest.content_sha256,
            folder_hash,
            0,
            folder_bytes.len() as u64,
            folder_bytes,
        )
        .unwrap();
        receiver.accept_chunk(&folder_chunk).unwrap();

        let file_manifest = BlobManifest {
            kind: BlobKind::File,
            object_key: object_key(BlobKind::File, "db/a.db"),
            content_sha256: Sha256::digest(b"a").into(),
            parent_content_sha256: None,
            logical_path: "db/a.db".into(),
            logical_size: 1,
            created_unix_ms: 1,
            body: BlobBody::File {
                changes: Vec::new(),
            },
        };
        let file_bytes = file_manifest.encode().unwrap();
        let file_hash: [u8; 32] = Sha256::digest(&file_bytes).into();
        let file_chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::Manifest,
            file_manifest.object_key,
            file_manifest.content_sha256,
            file_hash,
            0,
            file_bytes.len() as u64,
            file_bytes,
        )
        .unwrap();
        receiver.accept_chunk(&file_chunk).unwrap();

        let replay = receiver.accept_chunk(&folder_chunk).unwrap();
        assert!(replay.committed);
        assert!(replay.duplicate);
        assert_eq!(replay.next_offset, folder_chunk.total_size);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_resumes_partial_resource_after_receiver_restart() {
        let root = test_root("process-restart");
        fs::create_dir_all(&root).unwrap();
        let store = CloudNodeStore::open(&settings(&root, "process-restart")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [8u8; 32],
            folder_count: 0,
            video_count: 0,
            file_count: 1,
        };
        let payload = b"0123456789abcdef";
        let content_sha256: [u8; 32] = Sha256::digest(payload).into();
        let manifest = BlobManifest {
            kind: BlobKind::File,
            object_key: object_key(BlobKind::File, "db/restart.db"),
            content_sha256,
            parent_content_sha256: None,
            logical_path: "db/restart.db".into(),
            logical_size: payload.len() as u64,
            created_unix_ms: 1,
            body: BlobBody::File {
                changes: Vec::new(),
            },
        };
        let manifest_bytes = manifest.encode().unwrap();
        let manifest_hash: [u8; 32] = Sha256::digest(&manifest_bytes).into();
        let manifest_chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::Manifest,
            manifest.object_key,
            manifest.content_sha256,
            manifest_hash,
            0,
            manifest_bytes.len() as u64,
            manifest_bytes,
        )
        .unwrap();

        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "restart-peer", expected).unwrap();
        receiver.accept_chunk(&manifest_chunk).unwrap();
        let first = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            manifest.object_key,
            manifest.content_sha256,
            content_sha256,
            0,
            payload.len() as u64,
            payload[..8].to_vec(),
        )
        .unwrap();
        let receipt = receiver.accept_chunk(&first).unwrap();
        assert!(!receipt.committed);
        assert_eq!(receipt.next_offset, 8);
        drop(receiver);

        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "restart-peer", expected).unwrap();
        let replay = receiver.accept_chunk(&first).unwrap();
        assert!(replay.duplicate);
        assert_eq!(replay.next_offset, 8);

        let second = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            manifest.object_key,
            manifest.content_sha256,
            content_sha256,
            8,
            payload.len() as u64,
            payload[8..].to_vec(),
        )
        .unwrap();
        let receipt = receiver.accept_chunk(&second).unwrap();
        assert!(receipt.committed);
        assert_eq!(receipt.next_offset, payload.len() as u64);

        let target = receiver
            .staging_storage
            .join(hex::encode(manifest.object_key))
            .join("versions")
            .join(hex::encode(content_sha256))
            .join("payload");
        assert_eq!(fs::read(target).unwrap(), payload);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_commits_fully_durable_part_after_restart_probe() {
        let root = test_root("full-part-restart");
        fs::create_dir_all(&root).unwrap();
        let store = CloudNodeStore::open(&settings(&root, "full-part-restart")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [9u8; 32],
            folder_count: 0,
            video_count: 0,
            file_count: 1,
        };
        let payload = vec![3u8; 32];
        let content_sha256: [u8; 32] = Sha256::digest(&payload).into();
        let manifest = BlobManifest {
            kind: BlobKind::File,
            object_key: object_key(BlobKind::File, "db/full.db"),
            content_sha256,
            parent_content_sha256: None,
            logical_path: "db/full.db".into(),
            logical_size: payload.len() as u64,
            created_unix_ms: 1,
            body: BlobBody::File {
                changes: Vec::new(),
            },
        };
        let manifest_bytes = manifest.encode().unwrap();
        let manifest_hash: [u8; 32] = Sha256::digest(&manifest_bytes).into();
        let manifest_chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::Manifest,
            manifest.object_key,
            manifest.content_sha256,
            manifest_hash,
            0,
            manifest_bytes.len() as u64,
            manifest_bytes,
        )
        .unwrap();

        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "full-part-peer", expected).unwrap();
        receiver.accept_chunk(&manifest_chunk).unwrap();
        let whole = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            manifest.object_key,
            manifest.content_sha256,
            content_sha256,
            0,
            payload.len() as u64,
            payload.clone(),
        )
        .unwrap();
        let target = receiver.resource_target(&whole);
        let part = transfer_part_path(&target).unwrap();
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(&part, &payload).unwrap();
        File::open(&part).unwrap().sync_all().unwrap();
        drop(receiver);

        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "full-part-peer", expected).unwrap();
        let probe = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            manifest.object_key,
            manifest.content_sha256,
            content_sha256,
            0,
            payload.len() as u64,
            payload[..8].to_vec(),
        )
        .unwrap();
        let receipt = receiver.accept_chunk(&probe).unwrap();
        assert!(receipt.committed);
        assert_eq!(receipt.next_offset, payload.len() as u64);
        assert_eq!(fs::read(&target).unwrap(), payload);
        assert!(!part.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_phase_regression() {
        let root = test_root("order");
        fs::create_dir_all(&root).unwrap();
        let store = CloudNodeStore::open(&settings(&root, "order")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [7u8; 32],
            folder_count: 1,
            video_count: 0,
            file_count: 1,
        };
        let mut receiver = CloudNodeRecoveryReceiver::open(&store, "order-peer", expected).unwrap();

        let file_manifest = BlobManifest {
            kind: BlobKind::File,
            object_key: object_key(BlobKind::File, "db/a.db"),
            content_sha256: Sha256::digest(b"a").into(),
            parent_content_sha256: None,
            logical_path: "db/a.db".into(),
            logical_size: 1,
            created_unix_ms: 1,
            body: BlobBody::File {
                changes: Vec::new(),
            },
        };
        let bytes = file_manifest.encode().unwrap();
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let file_chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::Manifest,
            file_manifest.object_key,
            file_manifest.content_sha256,
            hash,
            0,
            bytes.len() as u64,
            bytes,
        )
        .unwrap();
        receiver.accept_chunk(&file_chunk).unwrap();

        let folder_manifest = BlobManifest {
            kind: BlobKind::Folder,
            object_key: object_key(BlobKind::Folder, "root"),
            content_sha256: folder_digest(&[]),
            parent_content_sha256: None,
            logical_path: "root".into(),
            logical_size: 0,
            created_unix_ms: 1,
            body: BlobBody::Folder {
                entries: Vec::new(),
            },
        };
        let bytes = folder_manifest.encode().unwrap();
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let folder_chunk = TransferChunk::new(
            BlobKind::Folder,
            TransferResource::Manifest,
            folder_manifest.object_key,
            folder_manifest.content_sha256,
            hash,
            0,
            bytes.len() as u64,
            bytes,
        )
        .unwrap();
        assert!(receiver.accept_chunk(&folder_chunk).is_err());

        fs::remove_dir_all(root).unwrap();
    }
}
