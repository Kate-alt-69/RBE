use std::collections::BTreeMap;
use std::path::Path;

use crate::{
    read_verified_root_worker_identities, read_verified_rpx_root_indexes, InstallRuntimeError,
    VerifiedPackageWorkerIdentity, VerifiedRpxRootIndex,
};

/// One fail-closed snapshot of every explicit RPX root needed by Runtime Image
/// linking. Export metadata and worker identity are independently verified, then
/// joined only when package/version/artifact identity is byte-for-byte equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRpxRootSnapshot {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub index_json: String,
    pub worker: VerifiedPackageWorkerIdentity,
}

/// Read and cross-check the public export index and worker contract for every
/// explicit project root.
///
/// The two underlying readers both validate the active lock and cached artifact.
/// If project activation changes between those reads, this join rejects any
/// difference in root set, resolved version, or artifact SHA rather than mixing
/// metadata from two package graphs.
pub fn read_verified_rpx_root_snapshots(
    project_root: &Path,
) -> Result<Vec<VerifiedRpxRootSnapshot>, VerifiedRootSnapshotError> {
    let indexes = read_verified_rpx_root_indexes(project_root)?;
    let workers = read_verified_root_worker_identities(project_root)?;
    join_verified_roots(indexes, workers.into_values().collect())
}

fn join_verified_roots(
    indexes: Vec<VerifiedRpxRootIndex>,
    workers: Vec<VerifiedPackageWorkerIdentity>,
) -> Result<Vec<VerifiedRpxRootSnapshot>, VerifiedRootSnapshotError> {
    let mut workers = workers
        .into_iter()
        .map(|worker| (worker.package.clone(), worker))
        .collect::<BTreeMap<_, _>>();

    if indexes.len() != workers.len() {
        return Err(VerifiedRootSnapshotError::GraphChanged {
            detail: format!(
                "root count changed while reading verified package state (indexes={}, workers={})",
                indexes.len(),
                workers.len()
            ),
        });
    }

    let mut snapshots = Vec::with_capacity(indexes.len());
    for index in indexes {
        let worker = workers.remove(&index.package).ok_or_else(|| {
            VerifiedRootSnapshotError::GraphChanged {
                detail: format!(
                    "root {:?} exists in the verified export view but not the verified worker view",
                    index.package
                ),
            }
        })?;
        if worker.version != index.version {
            return Err(VerifiedRootSnapshotError::GraphChanged {
                detail: format!(
                    "root {:?} version changed while reading verified package state (exports={}, worker={})",
                    index.package, index.version, worker.version
                ),
            });
        }
        if !worker
            .artifact_sha256
            .eq_ignore_ascii_case(&index.artifact_sha256)
        {
            return Err(VerifiedRootSnapshotError::GraphChanged {
                detail: format!(
                    "root {:?} artifact identity changed while reading verified package state",
                    index.package
                ),
            });
        }
        snapshots.push(VerifiedRpxRootSnapshot {
            package: index.package,
            version: index.version,
            artifact_sha256: index.artifact_sha256.to_ascii_lowercase(),
            index_json: index.index_json,
            worker,
        });
    }

    if let Some(package) = workers.keys().next() {
        return Err(VerifiedRootSnapshotError::GraphChanged {
            detail: format!(
                "root {package:?} exists in the verified worker view but not the verified export view"
            ),
        });
    }
    Ok(snapshots)
}

#[derive(Debug, thiserror::Error)]
pub enum VerifiedRootSnapshotError {
    #[error(transparent)]
    Install(#[from] InstallRuntimeError),
    #[error("verified RPX root snapshot changed during read: {detail}")]
    GraphChanged { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(package: &str, version: &str, artifact: char) -> VerifiedRpxRootIndex {
        VerifiedRpxRootIndex {
            package: package.into(),
            version: version.into(),
            artifact_sha256: artifact.to_string().repeat(64),
            index_json: "{}".into(),
        }
    }

    fn worker(package: &str, version: &str, artifact: char) -> VerifiedPackageWorkerIdentity {
        VerifiedPackageWorkerIdentity {
            package: package.into(),
            version: version.into(),
            artifact_sha256: artifact.to_string().repeat(64),
            rbe_abi_min: 1,
            rbe_abi_max: 1,
            sdk_language: "bun".into(),
            sdk_name: "@rbe/sdk".into(),
            sdk_version: "0.1.9".into(),
            runtime_kind: "bun".into(),
            runtime_version: "1.3.7".into(),
            runtime_entry: "src/index.js".into(),
            runtime_managed: true,
        }
    }

    #[test]
    fn joins_only_exact_root_version_and_artifact_identity() {
        let snapshots = join_verified_roots(
            vec![index("advancenet", "2.0.0", 'a')],
            vec![worker("advancenet", "2.0.0", 'a')],
        )
        .unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].worker.runtime_version, "1.3.7");
    }

    #[test]
    fn rejects_root_set_or_identity_drift() {
        assert!(matches!(
            join_verified_roots(
                vec![index("advancenet", "2.0.0", 'a')],
                vec![worker("advancenet", "2.0.1", 'a')],
            ),
            Err(VerifiedRootSnapshotError::GraphChanged { .. })
        ));
        assert!(matches!(
            join_verified_roots(
                vec![index("advancenet", "2.0.0", 'a')],
                vec![worker("advancenet", "2.0.0", 'b')],
            ),
            Err(VerifiedRootSnapshotError::GraphChanged { .. })
        ));
        assert!(matches!(
            join_verified_roots(
                vec![index("advancenet", "2.0.0", 'a')],
                vec![worker("other", "2.0.0", 'a')],
            ),
            Err(VerifiedRootSnapshotError::GraphChanged { .. })
        ));
    }
}
