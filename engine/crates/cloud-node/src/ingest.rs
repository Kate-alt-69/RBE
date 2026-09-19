use std::path::Path;

use crate::store::CloudNodeStore;

impl CloudNodeStore {
    /// Consume durable project-write intents into Cloud Node's content-addressed
    /// store. Replay is idempotent; ACK removes only the exact intent consumed.
    pub fn ingest_storage_journal(&self, project_root: &Path) -> anyhow::Result<usize> {
        let intents = storage_sync_journal::ready_intents(project_root)?;
        let mut consumed = 0usize;
        for intent in intents {
            let source = storage_sync_journal::source_path(project_root, &intent)?;
            let stored =
                self.store_file_with_priority(&source, &intent.logical_path, intent.level)?;
            if stored.content_sha256 != intent.content_sha256 {
                // The project file changed after the journal scan. Never ACK a
                // version we did not ingest; the next pass will reconcile it.
                continue;
            }
            if storage_sync_journal::acknowledge(project_root, &intent)? {
                consumed = consumed.saturating_add(1);
            }
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::CloudNodeSettings;

    fn root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-cn-storage-ingest-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "node": {
                "id": "ingest-test",
                "mode": "primary",
                "storageRoot": root.join("cloud").to_string_lossy(),
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            },
            "replication": { "targets": [] }
        }))
        .unwrap()
    }

    #[test]
    fn ingest_is_idempotent_and_preserves_data_level_outside_sync_identity() {
        let project = root("project");
        let source = project.join("data/state.json");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let bytes = br#"{"state":"critical"}"#;
        let prepared =
            storage_sync_journal::prepare(&project, "data/state.json", 1, bytes).unwrap();
        fs::write(&source, bytes).unwrap();
        storage_sync_journal::commit(&prepared).unwrap();

        let cloud_root = root("store");
        let store = CloudNodeStore::open(&settings(&cloud_root)).unwrap();
        assert_eq!(store.ingest_storage_journal(&project).unwrap(), 1);
        assert_eq!(store.ingest_storage_journal(&project).unwrap(), 0);

        let plan = store.sync_plan().unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].priority, 1);
        let expected_hash: [u8; 32] = Sha256::digest(bytes).into();
        assert_eq!(plan.files[0].content_sha256, expected_hash);
        let root_before = plan.root_sha256;
        store
            .store_file_with_priority(&source, "data/state.json", 3)
            .unwrap();
        let reprioritized = store.sync_plan().unwrap();
        assert_eq!(reprioritized.files[0].priority, 3);
        assert_eq!(reprioritized.root_sha256, root_before);

        let _ = fs::remove_dir_all(project);
        let _ = fs::remove_dir_all(cloud_root);
    }
}
