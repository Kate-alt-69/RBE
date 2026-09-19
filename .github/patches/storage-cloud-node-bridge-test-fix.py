from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "engine/crates/cloud-node/src/sync.rs",
    '''        let root = test_root();
        let settings = settings(&root);
        let store = CloudNodeStore::open(&settings).unwrap();
        let high = root.join("high.json");
''',
    '''        let root = test_root();
        let settings: crate::CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "node": {
                "id": "priority-test",
                "mode": "primary",
                "storageRoot": root.to_string_lossy(),
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            },
            "replication": { "targets": [] }
        }))
        .unwrap();
        let store = CloudNodeStore::open(&settings).unwrap();
        let high = root.join("high.json");
''',
    "sync priority settings fixture",
)

replace_once(
    "engine/crates/cloud-node/src/ingest.rs",
    '''        assert_eq!(plan.files[0].priority, 1);
        assert_eq!(plan.files[0].content_sha256, Sha256::digest(bytes).into());
        let root_before = plan.root_sha256;
''',
    '''        assert_eq!(plan.files[0].priority, 1);
        let expected_hash: [u8; 32] = Sha256::digest(bytes).into();
        assert_eq!(plan.files[0].content_sha256, expected_hash);
        let root_before = plan.root_sha256;
''',
    "ingest SHA-256 test type",
)
