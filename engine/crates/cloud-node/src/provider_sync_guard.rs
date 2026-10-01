use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::config::CloudNodeSettings;
use crate::format::BlobKind;
use crate::provider::ProviderClient;
use crate::provider_sync_raw;
pub use crate::provider_sync_raw::{
    provider_status, ProviderSyncAction, ProviderSyncRelation, ProviderSyncResult,
    ProviderSyncStatus,
};
use crate::store::CloudNodeStore;

const MAX_PROVIDER_STABILIZATION_PASSES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSnapshotAudit {
    Verified,
    LocalChanged,
}

/// Run the normal provider transaction and prove the live provider HEAD still
/// agrees with the activated local snapshot before reporting success.
///
/// Provider objects are immutable, but HEAD is intentionally mutable. A remote
/// writer can therefore publish another commit while a large snapshot is being
/// transferred. Re-checking status after each complete transaction prevents a
/// stale pull/push from being reported as the final synchronized state. Forward
/// remote movement is reconciled again within a small bounded number of passes.
/// A provider that disappears or moves backwards immediately after a pull is
/// failed closed instead of turning the freshly pulled snapshot into a push.
pub async fn synchronize_provider(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncResult> {
    let mut first_before = None;
    let mut last_mutating_action = None;

    for pass in 1..=MAX_PROVIDER_STABILIZATION_PASSES {
        let mut result = provider_sync_raw::synchronize_provider(settings, store)
            .await
            .map_err(|error| {
                anyhow::anyhow!("Cloud Node provider synchronization pass {pass} failed: {error:#}")
            })?;

        if first_before.is_none() {
            first_before = Some(result.before.clone());
        }
        if result.action != ProviderSyncAction::None {
            last_mutating_action = Some(result.action);
        }

        let mut status = provider_sync_raw::provider_status(settings, store)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node provider stabilization check after pass {pass} failed: {error:#}"
                )
            })?;

        if provider_sync_stable(&status) && last_mutating_action.is_none() {
            let expected = status.clone();
            match verify_stable_provider_snapshot_bytes(settings, store, &expected.local_root).await
            {
                Ok(ProviderSnapshotAudit::Verified) => {
                    status = provider_sync_raw::provider_status(settings, store)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "Cloud Node provider stabilization check after integrity audit on pass {pass} failed: {error:#}"
                            )
                        })?;
                    if !same_stable_state(&expected, &status) {
                        if pass < MAX_PROVIDER_STABILIZATION_PASSES {
                            continue;
                        }
                    }
                }
                Ok(ProviderSnapshotAudit::LocalChanged) => {
                    status = provider_sync_raw::provider_status(settings, store)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "Cloud Node provider stabilization check after local snapshot movement on pass {pass} failed: {error:#}"
                            )
                        })?;
                    if pass < MAX_PROVIDER_STABILIZATION_PASSES {
                        continue;
                    }
                }
                Err(audit_error) => {
                    let observed = provider_sync_raw::provider_status(settings, store)
                        .await
                        .map_err(|status_error| {
                            anyhow::anyhow!(
                                "Cloud Node provider integrity audit failed ({audit_error:#}) and the follow-up stabilization check also failed: {status_error:#}"
                            )
                        })?;
                    if !same_stable_state(&expected, &observed) {
                        status = observed;
                        if pass < MAX_PROVIDER_STABILIZATION_PASSES {
                            continue;
                        }
                    } else {
                        store.verify().map_err(|local_error| {
                            anyhow::anyhow!(
                                "Cloud Node provider integrity audit failed while the provider HEAD remained stable ({audit_error:#}), and the local snapshot also failed verification: {local_error:#}"
                            )
                        })?;
                        return Err(anyhow::anyhow!(
                            "Cloud Node provider immutable snapshot failed byte-for-byte integrity audit while HEAD remained stable: {audit_error:#}"
                        ));
                    }
                }
            }
        }

        if provider_sync_stable(&status) {
            result.before = first_before.take().unwrap_or(result.before);
            if let Some(action) = last_mutating_action {
                result.action = action;
            }
            result.final_root = status.local_root;
            result.final_head = status.local_head;
            return Ok(result);
        }

        if pull_regressed(last_mutating_action, status.relation) {
            anyhow::bail!(
                "Cloud Node provider moved backwards or disappeared after a pull (relation={:?}, localHead={}, remoteHead={}); refusing to republish the pulled snapshot. Retry after provider HEAD is stable",
                status.relation,
                status.local_head,
                status.remote_head.as_deref().unwrap_or("<empty>")
            );
        }

        if pass == MAX_PROVIDER_STABILIZATION_PASSES {
            anyhow::bail!(
                "Cloud Node provider did not stabilize after {MAX_PROVIDER_STABILIZATION_PASSES} synchronization passes: relation={:?} localHead={} remoteHead={}. Provider HEAD or local snapshot is changing too quickly; retry synchronization",
                status.relation,
                status.local_head,
                status.remote_head.as_deref().unwrap_or("<empty>")
            );
        }
    }

    unreachable!("bounded provider stabilization loop must return or fail")
}

async fn verify_stable_provider_snapshot_bytes(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    expected_root: &str,
) -> anyhow::Result<ProviderSnapshotAudit> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let plan = store.sync_plan()?;
    if plan.root_hex() != expected_root {
        return Ok(ProviderSnapshotAudit::LocalChanged);
    }

    let client = ProviderClient::new(provider)?;
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{expected_root}/objects/{object_hex}/{content_hex}");

        let (manifest_sha, manifest_size) = hash_and_size(&object.manifest_path).await?;
        verify_provider_resource(
            &client,
            &format!("{base}/{}", object.kind.manifest_name()),
            manifest_sha,
            manifest_size,
        )
        .await?;

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::File => {
                let payload = object.payload_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node file sync object has no payload during provider audit"
                    )
                })?;
                let payload_size = tokio::fs::metadata(payload).await?.len();
                verify_provider_resource(
                    &client,
                    &format!("{base}/payload"),
                    object.content_sha256,
                    payload_size,
                )
                .await?;
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let (chunk_sha, chunk_size) = hash_and_size(chunk_path).await?;
                    let chunk_hex = hex::encode(chunk_sha);
                    verify_provider_resource(
                        &client,
                        &format!("{base}/chunks/{chunk_hex}.chunk"),
                        chunk_sha,
                        chunk_size,
                    )
                    .await?;
                }
            }
        }
    }

    if store.sync_plan()?.root_hex() != expected_root {
        return Ok(ProviderSnapshotAudit::LocalChanged);
    }
    Ok(ProviderSnapshotAudit::Verified)
}

async fn verify_provider_resource(
    client: &ProviderClient,
    key: &str,
    expected_sha256: [u8; 32],
    expected_size: u64,
) -> anyhow::Result<()> {
    client
        .verify_object(key, expected_sha256, expected_size)
        .await
        .map_err(|error| {
            anyhow::anyhow!("provider resource {key:?} failed integrity verification: {error:#}")
        })
}

async fn hash_and_size(path: &Path) -> anyhow::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node provider audit source {}: {error}",
            path.display()
        )
    })?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node provider audit source {}: {error}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        size = size
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| anyhow::anyhow!("provider audit read size exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("provider audit size overflow"))?;
    }
    Ok((digest.finalize().into(), size))
}

fn provider_sync_stable(status: &ProviderSyncStatus) -> bool {
    status.relation == ProviderSyncRelation::InSync
}

fn same_stable_state(expected: &ProviderSyncStatus, observed: &ProviderSyncStatus) -> bool {
    provider_sync_stable(expected)
        && provider_sync_stable(observed)
        && expected.local_head == observed.local_head
        && expected.remote_head == observed.remote_head
        && expected.local_root == observed.local_root
        && expected.remote_root == observed.remote_root
}

fn pull_regressed(last_action: Option<ProviderSyncAction>, relation: ProviderSyncRelation) -> bool {
    matches!(
        last_action,
        Some(ProviderSyncAction::Pull | ProviderSyncAction::ForcedPull)
    ) && matches!(
        relation,
        ProviderSyncRelation::EmptyRemote | ProviderSyncRelation::LocalAhead
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(relation: ProviderSyncRelation) -> ProviderSyncStatus {
        ProviderSyncStatus {
            relation,
            local_head: "11".repeat(32),
            remote_head: Some("22".repeat(32)),
            local_root: "33".repeat(32),
            remote_root: Some("44".repeat(32)),
        }
    }

    #[test]
    fn stabilization_requires_exact_in_sync_relation() {
        assert!(provider_sync_stable(&status(ProviderSyncRelation::InSync)));
        for relation in [
            ProviderSyncRelation::EmptyRemote,
            ProviderSyncRelation::LocalAhead,
            ProviderSyncRelation::RemoteAhead,
            ProviderSyncRelation::Diverged,
        ] {
            assert!(!provider_sync_stable(&status(relation)));
        }
    }

    #[test]
    fn integrity_audit_requires_the_same_stable_heads_and_roots() {
        let expected = status(ProviderSyncRelation::InSync);
        assert!(same_stable_state(&expected, &expected));

        let mut moved = expected.clone();
        moved.remote_head = Some("55".repeat(32));
        assert!(!same_stable_state(&expected, &moved));

        moved = expected.clone();
        moved.remote_root = Some("66".repeat(32));
        assert!(!same_stable_state(&expected, &moved));

        moved = expected.clone();
        moved.relation = ProviderSyncRelation::RemoteAhead;
        assert!(!same_stable_state(&expected, &moved));
    }

    #[test]
    fn pull_never_turns_provider_regression_into_automatic_push() {
        for action in [ProviderSyncAction::Pull, ProviderSyncAction::ForcedPull] {
            assert!(pull_regressed(
                Some(action),
                ProviderSyncRelation::EmptyRemote
            ));
            assert!(pull_regressed(
                Some(action),
                ProviderSyncRelation::LocalAhead
            ));
            assert!(!pull_regressed(
                Some(action),
                ProviderSyncRelation::RemoteAhead
            ));
            assert!(!pull_regressed(
                Some(action),
                ProviderSyncRelation::Diverged
            ));
        }
        assert!(!pull_regressed(
            Some(ProviderSyncAction::Push),
            ProviderSyncRelation::LocalAhead
        ));
    }
}
