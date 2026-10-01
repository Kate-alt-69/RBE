use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
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
use crate::sync::{SyncPlan, SyncPlanHeader};
use crate::transfer::TransferResource;

const SNAPSHOT_VERSION: u16 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishedSnapshotIndex {
    format_version: u16,
    root_sha256: String,
    folder_count: u32,
    video_count: u32,
    file_count: u32,
    resources: Vec<PublishedSnapshotResource>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishedSnapshotResource {
    kind: u8,
    resource: u8,
    object_key: String,
    content_sha256: String,
    resource_sha256: String,
    size: u64,
    key: String,
}

/// Run the stabilized provider transaction and, for transports that cannot
/// cryptographically prove the uploaded body at write time, read back the
/// published immutable snapshot before reporting a successful push.
///
/// The Amazon S3 path signs the exact payload SHA-256 in SigV4
/// (`x-amz-content-sha256`), so S3 and S3-compatible endpoints do not need to
/// download every freshly uploaded object again. Direct Supabase, Azure Blob,
/// Google Cloud Storage, and generic HTTP uploads do not currently expose the
/// same SHA-256 write proof through this provider interface, so a mutating push
/// gets one bounded byte-for-byte readback here. The immutable snapshot index is
/// also re-read and compared to the exact resource metadata that was verified.
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
    let mut expected_resources = Vec::new();
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{expected_root}/objects/{object_hex}/{content_hex}");

        let (manifest_sha, manifest_size) = hash_and_size(&object.manifest_path).await?;
        let manifest_key = format!("{base}/{}", object.kind.manifest_name());
        verify_provider_resource(&client, &manifest_key, manifest_sha, manifest_size).await?;
        expected_resources.push(snapshot_resource(
            object.kind,
            TransferResource::Manifest,
            &object_hex,
            &content_hex,
            manifest_sha,
            manifest_size,
            manifest_key,
        ));

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
                let payload_key = format!("{base}/payload");
                verify_provider_resource(
                    &client,
                    &payload_key,
                    object.content_sha256,
                    payload_size,
                )
                .await?;
                expected_resources.push(snapshot_resource(
                    object.kind,
                    TransferResource::FilePayload,
                    &object_hex,
                    &content_hex,
                    object.content_sha256,
                    payload_size,
                    payload_key,
                ));
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let (chunk_sha, chunk_size) = hash_and_size(chunk_path).await?;
                    let chunk_hex = hex::encode(chunk_sha);
                    let chunk_key = format!("{base}/chunks/{chunk_hex}.chunk");
                    verify_provider_resource(&client, &chunk_key, chunk_sha, chunk_size).await?;
                    expected_resources.push(snapshot_resource(
                        object.kind,
                        TransferResource::VideoChunk,
                        &object_hex,
                        &content_hex,
                        chunk_sha,
                        chunk_size,
                        chunk_key,
                    ));
                }
            }
        }
    }

    verify_provider_snapshot_index(&client, &plan, expected_root, &expected_resources).await?;

    let after = store.sync_plan()?.root_hex();
    if after != expected_root {
        anyhow::bail!(
            "Cloud Node local snapshot changed during post-push verification: expected root {expected_root}, found {after}"
        );
    }
    Ok(())
}

fn snapshot_resource(
    kind: BlobKind,
    resource: TransferResource,
    object_key: &str,
    content_sha256: &str,
    resource_sha256: [u8; 32],
    size: u64,
    key: String,
) -> PublishedSnapshotResource {
    PublishedSnapshotResource {
        kind: kind as u8,
        resource: resource as u8,
        object_key: object_key.to_owned(),
        content_sha256: content_sha256.to_owned(),
        resource_sha256: hex::encode(resource_sha256),
        size,
        key,
    }
}

async fn verify_provider_snapshot_index(
    client: &ProviderClient,
    plan: &SyncPlan,
    expected_root: &str,
    expected_resources: &[PublishedSnapshotResource],
) -> anyhow::Result<()> {
    let key = format!("snapshots/{expected_root}/index.json");
    let bytes = client.get(&key).await?.ok_or_else(|| {
        anyhow::anyhow!("Cloud Node published provider snapshot index {key:?} is missing")
    })?;
    let index: PublishedSnapshotIndex = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!("Cloud Node published provider snapshot index {key:?} is invalid: {error}")
    })?;
    verify_snapshot_index_metadata(&index, expected_root, plan.header()?, expected_resources)
}

fn verify_snapshot_index_metadata(
    index: &PublishedSnapshotIndex,
    expected_root: &str,
    header: SyncPlanHeader,
    expected_resources: &[PublishedSnapshotResource],
) -> anyhow::Result<()> {
    if index.format_version != SNAPSHOT_VERSION {
        anyhow::bail!(
            "Cloud Node published provider snapshot index has unsupported format version {}",
            index.format_version
        );
    }
    if index.root_sha256 != expected_root
        || index.folder_count != header.folder_count
        || index.video_count != header.video_count
        || index.file_count != header.file_count
    {
        anyhow::bail!(
            "Cloud Node published provider snapshot index metadata does not match the verified local sync plan"
        );
    }

    let expected = resource_map(expected_resources, "expected")?;
    let observed = resource_map(&index.resources, "published")?;
    if observed != expected {
        anyhow::bail!(
            "Cloud Node published provider snapshot index resources do not match the verified immutable provider objects"
        );
    }
    Ok(())
}

fn resource_map(
    resources: &[PublishedSnapshotResource],
    label: &str,
) -> anyhow::Result<BTreeMap<String, PublishedSnapshotResource>> {
    let mut map = BTreeMap::new();
    for resource in resources {
        if map.insert(resource.key.clone(), resource.clone()).is_some() {
            anyhow::bail!(
                "Cloud Node {label} provider snapshot index contains duplicate resource key {:?}",
                resource.key
            );
        }
    }
    Ok(map)
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

    fn resource(key: &str) -> PublishedSnapshotResource {
        PublishedSnapshotResource {
            kind: BlobKind::File as u8,
            resource: TransferResource::FilePayload as u8,
            object_key: "11".repeat(32),
            content_sha256: "22".repeat(32),
            resource_sha256: "22".repeat(32),
            size: 7,
            key: key.to_owned(),
        }
    }

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

    #[test]
    fn published_index_requires_exact_metadata_and_resource_set() {
        let root = "aa".repeat(32);
        let header = SyncPlanHeader {
            root_sha256: [0xaa; 32],
            folder_count: 0,
            video_count: 0,
            file_count: 1,
        };
        let expected = vec![resource("snapshots/root/object/payload")];
        let index = PublishedSnapshotIndex {
            format_version: SNAPSHOT_VERSION,
            root_sha256: root.clone(),
            folder_count: 0,
            video_count: 0,
            file_count: 1,
            resources: expected.clone(),
        };
        assert!(verify_snapshot_index_metadata(&index, &root, header, &expected).is_ok());

        let mut wrong_root = index.clone();
        wrong_root.root_sha256 = "bb".repeat(32);
        assert!(verify_snapshot_index_metadata(&wrong_root, &root, header, &expected).is_err());

        let mut missing = index.clone();
        missing.resources.clear();
        assert!(verify_snapshot_index_metadata(&missing, &root, header, &expected).is_err());

        let mut duplicate = index;
        duplicate.resources.push(expected[0].clone());
        assert!(verify_snapshot_index_metadata(&duplicate, &root, header, &expected).is_err());
    }
}
