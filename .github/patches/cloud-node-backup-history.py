from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one backup-history anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


path = "engine/crates/cloud-node/src/store.rs"
replace_once(
    path,
    '''    fn update_backup(
        &self,
        source: &Path,
        logical_path: &str,
        object_hex: &str,
        manifest: &BlobManifest,
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        let name = safe_leaf_name(logical_path);
        let original_path = original.join(&name);
        if self.preserve_original && !original_path.exists() {
            copy_exact(source, &original_path)?;
        }
        copy_exact(source, &latest.join(&name))?;
        update_history(
            &root.join("history.blob.cn"),
            manifest.content_sha256,
            manifest.created_unix_ms,
            self.backup_versions,
        )
    }
''',
    '''    fn update_backup(
        &self,
        source: &Path,
        logical_path: &str,
        object_hex: &str,
        manifest: &BlobManifest,
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        let versions = root.join("versions");
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        fs::create_dir_all(&versions)?;
        let name = safe_leaf_name(logical_path);
        let original_path = original.join(&name);
        if self.preserve_original && !original_path.exists() {
            copy_exact(source, &original_path)?;
        }
        let content_hex = hex::encode(manifest.content_sha256);
        let revision_path = versions.join(&content_hex).join(&name);
        if !revision_path.exists() {
            copy_exact(source, &revision_path)?;
        }
        copy_exact(source, &latest.join(&name))?;
        let retained = update_history(
            &root.join("history.blob.cn"),
            manifest.content_sha256,
            manifest.created_unix_ms,
            self.backup_versions,
        )?;
        prune_backup_versions(&versions, &retained)
    }
''',
)
replace_once(
    path,
    '''    fn update_folder_backup(
        &self,
        object_hex: &str,
        encoded: &[u8],
        content_sha256: [u8; 32],
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        let original_path = original.join("folder.blob.cn");
        if self.preserve_original && !original_path.exists() {
            atomic_write(&original_path, encoded)?;
        }
        atomic_write(&latest.join("folder.blob.cn"), encoded)?;
        update_history(
            &root.join("history.blob.cn"),
            content_sha256,
            now_ms()?,
            self.backup_versions,
        )
    }
''',
    '''    fn update_folder_backup(
        &self,
        object_hex: &str,
        encoded: &[u8],
        content_sha256: [u8; 32],
    ) -> anyhow::Result<()> {
        let root = self.backup.join(object_hex);
        let original = root.join("original");
        let latest = root.join("latest");
        let versions = root.join("versions");
        fs::create_dir_all(&original)?;
        fs::create_dir_all(&latest)?;
        fs::create_dir_all(&versions)?;
        let original_path = original.join("folder.blob.cn");
        if self.preserve_original && !original_path.exists() {
            atomic_write(&original_path, encoded)?;
        }
        let content_hex = hex::encode(content_sha256);
        let revision_path = versions.join(&content_hex).join("folder.blob.cn");
        if !revision_path.exists() {
            atomic_write(&revision_path, encoded)?;
        }
        atomic_write(&latest.join("folder.blob.cn"), encoded)?;
        let retained = update_history(
            &root.join("history.blob.cn"),
            content_sha256,
            now_ms()?,
            self.backup_versions,
        )?;
        prune_backup_versions(&versions, &retained)
    }
''',
)
replace_once(
    path,
    '''fn read_manifest_if_present(path: &Path) -> anyhow::Result<Option<BlobManifest>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(BlobManifest::decode(&fs::read(path)?)?))
}

''',
    '''fn read_manifest_if_present(path: &Path) -> anyhow::Result<Option<BlobManifest>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(BlobManifest::decode(&fs::read(path)?)?))
}

fn prune_backup_versions(
    versions: &Path,
    retained: &VecDeque<([u8; 32], u64)>,
) -> anyhow::Result<()> {
    let retained = retained
        .iter()
        .map(|(hash, _)| hex::encode(hash))
        .collect::<std::collections::HashSet<_>>();
    for entry in fs::read_dir(versions)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if !retained.contains(&name.to_ascii_lowercase()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

''',
)
replace_once(
    path,
    '''fn update_history(
    path: &Path,
    content_sha256: [u8; 32],
    created_unix_ms: u64,
    keep: usize,
) -> anyhow::Result<()> {
    let mut entries = if path.is_file() {
        decode_history(&fs::read(path)?)?
    } else {
        VecDeque::new()
    };
    if entries.back().is_none_or(|entry| entry.0 != content_sha256) {
        entries.push_back((content_sha256, created_unix_ms));
    }
    while entries.len() > keep {
        entries.pop_front();
    }
    atomic_write(path, &encode_history(&entries)?)
}
''',
    '''fn update_history(
    path: &Path,
    content_sha256: [u8; 32],
    created_unix_ms: u64,
    keep: usize,
) -> anyhow::Result<VecDeque<([u8; 32], u64)>> {
    let mut entries = if path.is_file() {
        decode_history(&fs::read(path)?)?
    } else {
        VecDeque::new()
    };
    if entries.back().is_none_or(|entry| entry.0 != content_sha256) {
        entries.push_back((content_sha256, created_unix_ms));
    }
    while entries.len() > keep {
        entries.pop_front();
    }
    atomic_write(path, &encode_history(&entries)?)?;
    Ok(entries)
}
''',
)

# Extend the layout test so backup history proves it contains actual recoverable
# revision payloads, not only hashes pointing back into active storage.
p = Path(path)
text = p.read_text()
anchor = '''    #[test]
    fn video_storage_uses_hashed_chunks() {
'''
test = '''    #[test]
    fn backup_keeps_original_plus_five_actual_revisions() {
        let root = test_root("backup-history");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let mut object_key = String::new();
        for revision in 0..7u8 {
            fs::write(&source, [revision, b'd', b'b']).unwrap();
            let stored = store.store_file(&source, "db/users.db").unwrap();
            object_key = stored.object_key;
        }
        let backup = store.summary().backup.join(object_key);
        assert_eq!(fs::read(backup.join("original/users.db")).unwrap(), [0, b'd', b'b']);
        assert_eq!(fs::read(backup.join("latest/users.db")).unwrap(), [6, b'd', b'b']);
        let versions = fs::read_dir(backup.join("versions"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(versions.len(), 5);
        for revision in 2..7u8 {
            let hash: [u8; 32] = Sha256::digest([revision, b'd', b'b']).into();
            let path = backup
                .join("versions")
                .join(hex::encode(hash))
                .join("users.db");
            assert_eq!(fs::read(path).unwrap(), [revision, b'd', b'b']);
        }
        fs::remove_dir_all(root).unwrap();
    }

'''
if text.count(anchor) != 1:
    raise SystemExit("Cloud Node backup test anchor drifted")
p.write_text(text.replace(anchor, test + anchor, 1))

# Keep the public layout documentation accurate.
path = "doc/cloud-node.md"
replace_once(
    path,
    '''└── backup/
    └── <same-object-sha256>/
        ├── original/                         # first exact object, preserved
        ├── latest/                           # latest exact object
        └── history.blob.cn                   # binary rolling history, default 5
''',
    '''└── backup/
    └── <same-object-sha256>/
        ├── original/                         # first exact object, preserved
        ├── latest/                           # latest exact object
        ├── versions/<content-sha256>/...     # actual rolling revision payloads
        └── history.blob.cn                   # binary rolling history, default 5
''',
)
