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

/// Run the stabilized provider transaction and prove the published snapshot
/// index still describes the exact local sync plan before reporting success.
///
/// The Amazon S3 path signs the exact payload SHA-256 in SigV4
/// (`x-amz-content-sha256`), so S3 and S3-compatible endpoints do not need to
/// download every freshly uploaded object again. Direct Supabase, Azure Blob,
/// Google Cloud Storage, and generic HTTP uploads do not currently expose the
/// same SHA-256 write proof through this provider interface, so a mutating push
/// additionally gets one bounded byte-for-byte readback of every immutable
/// resource. Every stable sync re-reads the immutable snapshot index and then
/// confirms HEAD/root did not move during that audit.
pub async fn synchronize_provider(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncResult> {
    let result = provider_sync_guard::synchronize_provider(settings, store).await?;
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;

    let audit = if provider_push_requires_readback(provider.kind, result.action) {
        verify_published_provider_snapshot(settings, store, &result.final_root).await
    } else {
        verify_published_provider_index(settings, store, &result.final_root).await
    };

    let observed = provider_sync_guard::provider_status(settings, store)
        .await
        .map_err(|status_error| {
            anyhow::anyhow!(
                "Cloud Node provider integrity audit finished but the stabilization status check failed: {status_error:#}"
            )
        })?;

    if !same_published_state(&result, &observed) {
        anyhow::bail!(
            "Cloud Node provider state changed during final integrity verification; retry synchronization (expectedHead={} observedLocalHead={} observedRemoteHead={} expectedRoot={} observedLocalRoot={} observedRemoteRoot={} relation={:?})",
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
            "Cloud Node provider immutable snapshot failed final integrity verification while HEAD remained stable: {error:#}"
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
    let plan = verified_plan_for_audit(store, expected_root)?;
    let expected_resources = collect_snapshot_index_resources(&plan, expected_root).await?;
    let client = ProviderClient::new(provider)?;

    for resource in &expected_resources {
        verify_provider_resource(
            &client,
            &resource.key,
            decode_resource_hash(&resource.resource_sha256)?,
            resource.size,
        )
        .await?;
    }
    verify_snapshot_index_object(&client, &plan, expected_root, &expected_resources).await?;
    verify_local_root_unchanged(store, expected_root)
}

async fn verify_published_provider_index(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    expected_root: &str,
) -> anyhow::Result<()> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let plan = verified_plan_for_audit(store, expected_root)?;
    let expected_resources = collect_snapshot_index_resources(&plan, expected_root).await?;
    let client = ProviderClient::new(provider)?;
    verify_snapshot_index_object(&client, &plan, expected_root, &expected_resources).await?;
    verify_local_root_unchanged(store, expected_root)
}

fn verified_plan_for_audit(
    store: &CloudNodeStore,
    expected_root: &str,
) -> anyhow::Result<SyncPlan> {
    let plan = store.sync_plan()?;
    if plan.root_hex() != expected_root {
        anyhow::bail!(
            "Cloud Node local snapshot changed before provider integrity verification: expected root {expected_root}, found {}",
            plan.root_hex()
        );
    }
    Ok(plan)
}

fn verify_local_root_unchanged(store: &CloudNodeStore, expected_root: &str) -> anyhow::Result<()> {
    let after = store.sync_plan()?.root_hex();
    if after != expected_root {
        anyhow::bail!(
            "Cloud Node local snapshot changed during provider integrity verification: expected root {expected_root}, found {after}"
        );
    }
    Ok(())
}

async fn collect_snapshot_index_resources(
    plan: &SyncPlan,
    expected_root: &str,
) -> anyhow::Result<Vec<PublishedSnapshotResource>> {
    let mut resources = Vec::new();
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{expected_root}/objects/{object_hex}/{content_hex}");

        let (manifest_sha, manifest_size) = hash_and_size(&object.manifest_path).await?;
        resources.push(snapshot_resource(
            object.kind,
            TransferResource::Manifest,
            &object_hex,
            &content_hex,
            manifest_sha,
            manifest_size,
            format!("{base}/{}", object.kind.manifest_name()),
        ));

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::File => {
                let payload = object.payload_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node file sync object has no payload during provider integrity verification"
                    )
                })?;
                let payload_size = tokio::fs::metadata(payload)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "failed to stat Cloud Node provider audit payload {}: {error}",
                            payload.display()
                        )
                    })?
                    .len();
                resources.push(snapshot_resource(
                    object.kind,
                    TransferResource::FilePayload,
                    &object_hex,
                    &content_hex,
                    object.content_sha256,
                    payload_size,
                    format!("{base}/payload"),
                ));
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let chunk_sha = chunk_hash_from_path(chunk_path)?;
                    let chunk_size = tokio::fs::metadata(chunk_path)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "failed to stat Cloud Node provider audit chunk {}: {error}",
                                chunk_path.display()
                            )
                        })?
                        .len();
                    let chunk_hex = hex::encode(chunk_sha);
                    resources.push(snapshot_resource(
                        object.kind,
                        TransferResource::VideoChunk,
                        &object_hex,
                        &content_hex,
                        chunk_sha,
                        chunk_size,
                        format!("{base}/chunks/{chunk_hex}.chunk"),
                    ));
                }
            }
        }
    }
    Ok(resources)
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

async fn verify_snapshot_index_object(
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

fn chunk_hash_from_path(path: &Path) -> anyhow::Result<[u8; 32]> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".chunk"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node provider audit video chunk has invalid file name: {}",
                path.display()
            )
        })?;
    decode_resource_hash(name).map_err(|error| {
        anyhow::anyhow!(
            "Cloud Node provider audit video chunk name is not a SHA-256 hash ({}): {error:#}",
            path.display()
        )
    })
}

fn decode_resource_hash(value: &str) -> anyhow::Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("provider resource hash must be a 32-byte hexadecimal SHA-256 value");
    }
    let bytes = hex::decode(value)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("provider resource hash decoded to the wrong length"))
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
            "failed to open Cloud Node provider verification source {}: {error}",
            path.display()
        )
    })?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node provider verification source {}: {error}",
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
                    .map_err(|_| anyhow::anyhow!("provider verification read exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("provider verification size overflow"))?;
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
    fn post_sync_confirmation_requires_exact_head_and_root() {
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

    #[test]
    fn resource_hash_decoder_is_strict() {
        assert_eq!(decode_resource_hash(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        assert!(decode_resource_hash("ab").is_err());
        assert!(decode_resource_hash(&format!("{}zz", "ab".repeat(31))).is_err());
    }
}
