from pathlib import Path


def replace(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: {path}: expected one anchor, found {count}")
    p.write_text(text.replace(old, new, 1))
    print(f"patched: {label}")


recovery = "engine/crates/cloud-node/src/recovery.rs"

replace(
    recovery,
    '''    pub fn open(store: &CloudNodeStore, session: [u8; 16]) -> anyhow::Result<Self> {
        let summary = store.summary();
        let session_root = summary
            .root
            .join("recovery-staging")
            .join(hex::encode(session));
        if session_root.exists() {
            fs::remove_dir_all(&session_root)?;
        }
        let staging_storage = session_root.join("storage");
        fs::create_dir_all(&staging_storage)?;
        Ok(Self {
            session_root,
            staging_storage,
            phase: 0,
            active: None,
            completed: false,
        })
    }
''',
    '''    pub fn open(
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
''',
    "stable peer/root recovery staging",
)

replace(
    recovery,
    '''        let target = self.resource_target(chunk);
        if target.is_file() && verify_resource(&target, chunk.resource_sha256, chunk.total_size)? {
            let video_reconstructed = self.after_resource_committed(chunk, &target)?;
            return Ok(RecoveryReceipt {
                committed: true,
                duplicate: true,
                video_reconstructed,
                next_offset: chunk.total_size,
            });
        }
''',
    '''        let target = self.resource_target(chunk);
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
''',
    "clean stale transfer part for committed resource",
)

replace(
    recovery,
    '''        let identity = ResourceIdentity::from(chunk);
        if self.active.is_none() {
            if chunk.offset != 0 {
                anyhow::bail!("Cloud Node recovery resource did not begin at offset zero");
            }
            let part = self.session_root.join("active-resource.part");
            if part.exists() {
                fs::remove_file(&part)?;
            }
            File::create(&part)?;
            self.active = Some(ActiveResource {
                identity: identity.clone(),
                path: part,
                written: 0,
            });
        }
''',
    '''        let identity = ResourceIdentity::from(chunk);
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
''',
    "adopt durable resource part after process restart",
)

replace(
    recovery,
    '''        let data_len = u64::try_from(chunk.data.len())
            .map_err(|_| anyhow::anyhow!("Cloud Node recovery chunk size exceeds u64"))?;
        let end = chunk
            .offset
            .checked_add(data_len)
            .ok_or_else(|| anyhow::anyhow!("Cloud Node recovery chunk range overflow"))?;

        let mut duplicate = false;
        if chunk.offset < active.written {
            if end > active.written {
                anyhow::bail!("Cloud Node recovery chunk overlaps the committed prefix");
            }
            verify_part_range(&active.path, chunk.offset, &chunk.data)?;
            duplicate = true;
        } else {
            if chunk.offset != active.written {
                anyhow::bail!("Cloud Node recovery chunk is not contiguous");
            }
            let mut file = OpenOptions::new().append(true).open(&active.path)?;
            file.write_all(&chunk.data)?;
            file.sync_data()?;
            active.written = end;
        }
''',
    '''        let duplicate = write_or_replay_recovery_chunk(active, chunk)?;
''',
    "restart-safe partial chunk replay",
)

replace(
    recovery,
    '''        if !verify_resource(&active.path, chunk.resource_sha256, chunk.total_size)? {
            anyhow::bail!("Cloud Node recovery resource hash mismatch");
        }
''',
    '''        if !verify_resource(&active.path, chunk.resource_sha256, chunk.total_size)? {
            reset_active_resource(active)?;
            anyhow::bail!(
                "Cloud Node recovery resource hash mismatch; durable partial was reset"
            );
        }
''',
    "reset corrupt durable partial after final hash mismatch",
)

replace(
    recovery,
    '''        if self.active.is_some() {
            anyhow::bail!("Cloud Node recovery cannot complete with a partial resource");
        }

        CloudNodeStore::verify_storage_path(&self.staging_storage)?;
''',
    '''        if self.active.is_some() || contains_transfer_part(&self.staging_storage)? {
            anyhow::bail!("Cloud Node recovery cannot complete with a partial resource");
        }

        CloudNodeStore::verify_storage_path(&self.staging_storage)?;
''',
    "completion rejects process-orphaned transfer parts",
)

replace(
    recovery,
    '''fn part_path(path: &Path) -> anyhow::Result<PathBuf> {
''',
    '''fn recovery_peer_key(owner_node_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-RECOVERY-PEER/1\\0");
    digest.update(owner_node_id.as_bytes());
    hex::encode(digest.finalize())
}

fn recovery_plan_key(expected: SyncPlanHeader) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-RECOVERY-PLAN/1\\0");
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
''',
    "persistent recovery helper layer",
)

replace(
    recovery,
    'CloudNodeRecoveryReceiver::open(&destination, [9u8; 16]).unwrap()',
    'CloudNodeRecoveryReceiver::open(&destination, "source", expected).unwrap()',
    "swap test receiver identity",
)

replace(
    recovery,
    '''        let store = CloudNodeStore::open(&settings(&root, "resume-replay")).unwrap();
        let mut receiver = CloudNodeRecoveryReceiver::open(&store, [6u8; 16]).unwrap();
''',
    '''        let store = CloudNodeStore::open(&settings(&root, "resume-replay")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [6u8; 32],
            folder_count: 1,
            video_count: 0,
            file_count: 1,
        };
        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "resume-peer", expected).unwrap();
''',
    "resume replay test receiver identity",
)

replace(
    recovery,
    '''        let store = CloudNodeStore::open(&settings(&root, "order")).unwrap();
        let mut receiver = CloudNodeRecoveryReceiver::open(&store, [7u8; 16]).unwrap();
''',
    '''        let store = CloudNodeStore::open(&settings(&root, "order")).unwrap();
        let expected = SyncPlanHeader {
            root_sha256: [7u8; 32],
            folder_count: 1,
            video_count: 0,
            file_count: 1,
        };
        let mut receiver =
            CloudNodeRecoveryReceiver::open(&store, "order-peer", expected).unwrap();
''',
    "order test receiver identity",
)

marker = '''    #[test]
    fn recovery_rejects_phase_regression() {
'''
test = '''    #[test]
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

'''
replace(recovery, marker, test + marker, "process restart resume regression test")

backend = "engine/crates/backend/src/maintenance_notice.rs"
replace(
    backend,
    '''fn open_recovery_state(
    runtime: &CloudNodeRuntime,
    owner_node_id: &str,
    session: [u8; 16],
    expected: SyncPlanHeader,
) -> anyhow::Result<RecoveryState> {
    Ok(RecoveryState {
        owner_node_id: owner_node_id.to_owned(),
        expected,
        receiver: CloudNodeRecoveryReceiver::open(&runtime.store, session)?,
    })
}
''',
    '''fn open_recovery_state(
    runtime: &CloudNodeRuntime,
    owner_node_id: &str,
    _session: [u8; 16],
    expected: SyncPlanHeader,
) -> anyhow::Result<RecoveryState> {
    Ok(RecoveryState {
        owner_node_id: owner_node_id.to_owned(),
        expected,
        receiver: CloudNodeRecoveryReceiver::open(&runtime.store, owner_node_id, expected)?,
    })
}
''',
    "backend adopts persistent peer/root staging",
)

doc = "doc/cloud-node.md"
replace(
    doc,
    '''└── recovery-staging/
    └── <authenticated-session>/              # private full-snapshot recovery staging
''',
    '''├── .cache/
│   └── outbound/<peer-id>/<sync-root>/        # frozen LOCAL upload spool; retained on failure
└── recovery-staging/
    └── <peer-sha256>/<plan-sha256>/           # private REMOTE crash-resumable staging
        └── storage/.../*.transfer.part        # fsynced partial resource bytes
''',
    "Cloud Node cache/staging layout docs",
)

replace(
    doc,
    '''Recovery is intentionally full-snapshot rather than merge-based. Stale objects on the REMOTE side must disappear. The receiver writes into `recovery-staging/<session>/storage`, verifies the completed staged tree against the exact root negotiated with the authenticated sender, then swaps that storage tree into the live Cloud Node store. If post-swap verification fails, the previous live storage tree is restored.
''',
    '''Recovery is intentionally full-snapshot rather than merge-based. Stale objects on the REMOTE side must disappear. The receiver writes into a staging tree keyed by the authenticated peer plus negotiated plan rather than by the temporary authentication session. Resource bytes are first written to stable `.transfer.part` files and fsynced. If the backend process restarts, the same authenticated peer negotiating the same plan reopens that staging tree, derives the already committed recovery phase, validates replayed prefix bytes, and continues from the durable partial offset instead of deleting the recovery. A changed plan for the same peer prunes the obsolete private staging tree. The completed staged tree is verified against the exact negotiated root and only then swapped into the live Cloud Node store. If post-swap verification fails, the previous live storage tree is restored.

On the LOCAL sender, `.cache/outbound/<peer>/<sync-root>/` freezes the exact snapshot before transfer. The cache is assembled through `.part` files, survives network failure, and is removed only after the REMOTE proves that exact root was activated. Large blobs stay disk-backed rather than being buffered as whole objects in memory.
''',
    "crash-resume transport docs",
)
