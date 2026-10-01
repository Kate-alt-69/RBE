use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::config::{CloudNodeSettings, ProviderKind};
use crate::format::BlobKind;
use crate::provider::ProviderClient;
use crate::provider_sync_guard;
pub use crate::provider_sync_guard::{
    provider_status, ProviderSyncAction, ProviderSyncRelation, ProviderSyncResult,
    ProviderSyncStatus,
};
use crate::store::CloudNodeStore;

/// Run the stabilized provider transaction and, for transports that cannot
/// cryptographically prove the uploaded body at write time, read back the
/// published immutable snapshot before reporting a successful push.
///
/// The Amazon S3 path signs the exact payload SHA-256 in SigV4
/// (`x-amz-content-sha256`), so S3 and S3-compatible endpoints do not need to
/// download every freshly uploaded object again. Direct Supabase, Azure Blob,
/// Google Cloud Storage, and generic HTTP uploads do not currently expose the
/// same SHA-256 write proof through this provider interface, so a mutating push
/// gets one bounded byte-for-byte readback here.
pub async fn synchronize_provider(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncResult> {
    let result = provider_sync_guard::synchronize_provider(settings, store).await?;
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;

    if !provider_push_requires_readback(provider.kind, result.action) {
        return Ok(result);
    }

    let audit = verify_published_provider_snapshot(settings, store, &result.final_root).await;
    let observed = provider_sync_guard::provider_status(settings, store)
        .await
        .map_err(|status_error| {
            anyhow::anyhow!(
                "Cloud Node post-push provider readback finished but the stabilization status check failed: {status_error:#}"
            )
        })?;

    if !same_published_state(&result, &observed) {
        anyhow::bail!(
            "Cloud Node provider state changed during post-push byte verification; retry synchronization (expectedHead={} observedLocalHead={} observedRemoteHead={} expectedRoot={} observedLocalRoot={} observedRemoteRoot={} relation={:?})",
            result.final_head,
            observed.local_head,
            observed.remote_head.as_deref().unwrap_or("<empty>"),
            result.final_root,
            observed.local_root,
            observed.remote_root.as_deref().unwrap_or("<empty>"),
            observed.relation
        );
    }

    audit.map_err(|error| {
        anyhow::anyhow!(
            "Cloud Node provider accepted a push but the immutable snapshot failed post-publish byte verification while HEAD remained stable: {error:#}"
        )
    })?;
    Ok(result)
}

fn provider_push_requires_readback(kind: ProviderKind, action: ProviderSyncAction) -> bool {
    matches!(
        action,
        ProviderSyncAction::Push | ProviderSyncAction::ForcedPush
    ) && kind != ProviderKind::AmazonS3
}

fn same_published_state(result: &ProviderSyncResult, status: &ProviderSyncStatus) -> bool {
    status.relation == ProviderSyncRelation::InSync
        && status.local_head == result.final_head
        && status.remote_head.as_deref() == Some(result.final_head.as_str())
        && status.local_root == result.final_root
        && status.remote_root.as_deref() == Some(result.final_root.as_str())
}

async fn verify_published_provider_snapshot(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    expected_root: &str,
) -> anyhow::Result<()> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let plan = store.sync_plan()?;
    if plan.root_hex() != expected_root {
        anyhow::bail!(
            "Cloud Node local snapshot changed before post-push verification: expected root {expected_root}, found {}",
            plan.root_hex()
        );
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
                        "Cloud Node file sync object has no payload during post-push verification"
                    )
                })?;
                let payload_size = tokio::fs::metadata(payload)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "failed to stat Cloud Node post-push payload {}: {error}",
                            payload.display()
                        )
                    })?
                    .len();
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

    let after = store.sync_plan()?.root_hex();
    if after != expected_root {
        anyhow::bail!(
            "Cloud Node local snapshot changed during post-push verification: expected root {expected_root}, found {after}"
        );
    }
    Ok(())
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
            anyhow::anyhow!(
                "provider resource {key:?} failed post-push integrity verification: {error:#}"
            )
        })
}

async fn hash_and_size(path: &Path) -> anyhow::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node post-push verification source {}: {error}",
            path.display()
        )
    })?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node post-push verification source {}: {error}",
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
                    .map_err(|_| anyhow::anyhow!("post-push verification read exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("post-push verification size overflow"))?;
    }
    Ok((digest.finalize().into(), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_s3_provider_pushes_require_readback() {
        for action in [ProviderSyncAction::Push, ProviderSyncAction::ForcedPush] {
            assert!(!provider_push_requires_readback(
                ProviderKind::AmazonS3,
                action
            ));
            for kind in [
                ProviderKind::Supabase,
                ProviderKind::AzureBlob,
                ProviderKind::GoogleCloudStorage,
                ProviderKind::Http,
            ] {
                assert!(provider_push_requires_readback(kind, action));
            }
        }

        for action in [
            ProviderSyncAction::None,
            ProviderSyncAction::Pull,
            ProviderSyncAction::ForcedPull,
        ] {
            for kind in [
                ProviderKind::AmazonS3,
                ProviderKind::Supabase,
                ProviderKind::AzureBlob,
                ProviderKind::GoogleCloudStorage,
                ProviderKind::Http,
            ] {
                assert!(!provider_push_requires_readback(kind, action));
            }
        }
    }

    #[test]
    fn post_push_confirmation_requires_exact_head_and_root() {
        let result = ProviderSyncResult {
            action: ProviderSyncAction::Push,
            before: ProviderSyncStatus {
                relation: ProviderSyncRelation::LocalAhead,
                local_head: "00".repeat(32),
                remote_head: None,
                local_root: "11".repeat(32),
                remote_root: None,
            },
            final_root: "22".repeat(32),
            final_head: "33".repeat(32),
        };
        let stable = ProviderSyncStatus {
            relation: ProviderSyncRelation::InSync,
            local_head: result.final_head.clone(),
            remote_head: Some(result.final_head.clone()),
            local_root: result.final_root.clone(),
            remote_root: Some(result.final_root.clone()),
        };
        assert!(same_published_state(&result, &stable));

        let mut moved = stable.clone();
        moved.remote_head = Some("44".repeat(32));
        assert!(!same_published_state(&result, &moved));

        moved = stable.clone();
        moved.remote_root = Some("55".repeat(32));
        assert!(!same_published_state(&result, &moved));

        moved = stable;
        moved.relation = ProviderSyncRelation::RemoteAhead;
        assert!(!same_published_state(&result, &moved));
    }
}
