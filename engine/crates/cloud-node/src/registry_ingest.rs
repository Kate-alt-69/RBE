use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{CloudNodeStore, StoredObject};

const REGISTRY_LOGICAL_ROOT: &str = "registry";
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "index",
    "packages",
    "ownership",
    "releases",
    "history",
    "analytics",
    "artifacts",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryIngestResult {
    pub files: usize,
    pub metadata_files: usize,
    pub artifact_files: usize,
    pub stored: Vec<RegistryStoredObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryStoredObject {
    pub logical_path: String,
    pub object_key: String,
    pub content_sha256: String,
}

/// Ingest a trusted Kastrick/RPX registry export into Cloud Node's normal
/// content-addressed store.
///
/// The resulting objects participate in the existing provider snapshot and
/// revision-history model, so registry state gets the same local-ahead/push,
/// remote-ahead/pull, and equal/no-op behavior as every other Cloud Node
/// object. Published `.rbe.zip` artifacts are treated as opaque immutable
/// bytes here; package archive and `package.rbe.yaml` verification belongs to
/// the trusted registry publisher before the export reaches Cloud Node.
pub fn ingest_registry_export(
    store: &CloudNodeStore,
    export_root: &Path,
) -> anyhow::Result<RegistryIngestResult> {
    if !export_root.is_dir() {
        anyhow::bail!(
            "Cloud Node registry export root is not a directory: {}",
            export_root.display()
        );
    }

    let mut files = Vec::new();
    collect_registry_files(export_root, export_root, &mut files)?;
    files.sort();

    let mut result = RegistryIngestResult::default();
    for source in files {
        let relative = source
            .strip_prefix(export_root)
            .map_err(|_| anyhow::anyhow!("Cloud Node registry export path escaped its root"))?;
        let normalized = validate_registry_relative_path(relative)?;
        let is_artifact = normalized.ends_with(".rbe.zip");
        let logical_path = format!("{REGISTRY_LOGICAL_ROOT}/{normalized}");
        let stored = store.store_file(&source, &logical_path)?;
        result.files += 1;
        if is_artifact {
            result.artifact_files += 1;
        } else {
            result.metadata_files += 1;
        }
        result
            .stored
            .push(stored_registry_object(logical_path, stored));
    }

    Ok(result)
}

fn stored_registry_object(logical_path: String, stored: StoredObject) -> RegistryStoredObject {
    RegistryStoredObject {
        logical_path,
        object_key: stored.object_key,
        content_sha256: stored.content_sha256,
    }
}

fn collect_registry_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(current).map_err(|error| {
        anyhow::anyhow!(
            "failed to read Cloud Node registry export directory {}: {error}",
            current.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Cloud Node registry export cannot contain symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_registry_files(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            anyhow::bail!(
                "Cloud Node registry export contains a non-regular entry: {}",
                path.display()
            );
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow::anyhow!("Cloud Node registry export path escaped its root"))?;
        validate_registry_relative_path(relative)?;
        files.push(path);
    }
    Ok(())
}

fn validate_registry_relative_path(path: &Path) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Cloud Node registry paths must be valid UTF-8")
                })?;
                if value.is_empty() || value == "." || value == ".." || value.contains('\\') {
                    anyhow::bail!("Cloud Node registry path contains an unsafe segment");
                }
                parts.push(value.to_owned());
            }
            _ => anyhow::bail!("Cloud Node registry paths must be relative and normalized"),
        }
    }

    if parts.len() < 2 {
        anyhow::bail!("Cloud Node registry files must live below a recognized registry collection");
    }
    if !ALLOWED_TOP_LEVEL.contains(&parts[0].as_str()) {
        anyhow::bail!(
            "Cloud Node registry collection {:?} is not supported",
            parts[0]
        );
    }
    if parts.last().is_some_and(|name| name == "package.rbe.json") {
        anyhow::bail!(
            "package.rbe.json is an RBE application/project manifest, not registry archive metadata"
        );
    }

    let normalized = parts.join("/");
    let is_json = normalized.ends_with(".json");
    let is_artifact = normalized.ends_with(".rbe.zip");
    if parts[0] == "artifacts" {
        if !is_json && !is_artifact {
            anyhow::bail!(
                "Cloud Node registry artifacts may contain only metadata .json and .rbe.zip payloads"
            );
        }
    } else if !is_json {
        anyhow::bail!("Cloud Node registry metadata collections may contain only .json files");
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudNodeSettings;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cn-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": "registry-test",
                "storageRoot": root,
                "backupVersions": 5,
                "preserveOriginal": true
            }
        }))
        .unwrap()
    }

    #[test]
    fn registry_ingest_accepts_all_frozen_server_collections() {
        let root = temp_root("layout");
        let export = root.join("export");
        let files = [
            ("index/snapshot.json", b"{}".as_slice()),
            ("packages/demo.json", b"{}".as_slice()),
            ("ownership/demo.json", b"{}".as_slice()),
            ("releases/demo/1.0.0.json", b"{}".as_slice()),
            ("history/demo/publish-1.json", b"{}".as_slice()),
            ("analytics/demo/snapshot-1.json", b"{}".as_slice()),
            ("artifacts/demo/1.0.0/metadata.json", b"{}".as_slice()),
            (
                "artifacts/demo/1.0.0/demo.rbe.zip",
                b"PK\x03\x04".as_slice(),
            ),
        ];
        for (relative, bytes) in files {
            let path = export.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        let store = CloudNodeStore::open(&settings(&root)).unwrap();
        let result = ingest_registry_export(&store, &export).unwrap();
        assert_eq!(result.files, 8);
        assert_eq!(result.metadata_files, 7);
        assert_eq!(result.artifact_files, 1);
        assert!(result
            .stored
            .iter()
            .any(|stored| stored.logical_path == "registry/index/snapshot.json"));
        assert!(result
            .stored
            .iter()
            .any(|stored| stored.logical_path.ends_with("demo.rbe.zip")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_ingest_rejects_project_manifest_and_unknown_collections() {
        assert!(validate_registry_relative_path(Path::new("packages/package.rbe.json")).is_err());
        assert!(validate_registry_relative_path(Path::new("other/demo.json")).is_err());
        assert!(validate_registry_relative_path(Path::new(
            "artifacts/demo/1.0.0/package.rbe.yaml"
        ))
        .is_err());
    }

    #[test]
    fn registry_ingest_rejects_non_json_metadata_and_non_rbe_artifacts() {
        assert!(validate_registry_relative_path(Path::new("packages/demo.yaml")).is_err());
        assert!(
            validate_registry_relative_path(Path::new("artifacts/demo/1.0.0/demo.zip")).is_err()
        );
        validate_registry_relative_path(Path::new("artifacts/demo/1.0.0/demo.rbe.zip")).unwrap();
    }
}
