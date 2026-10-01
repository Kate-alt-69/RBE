use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::config::{CloudNodeSettings, ProviderConflictPolicy};
use crate::format::BlobKind;
use crate::provider::ProviderClient;
use crate::provider_sync_raw;
pub use crate::provider_sync_raw::{
    provider_status, ProviderSyncAction, ProviderSyncRelation, ProviderSyncResult,
    ProviderSyncStatus,
};
use crate::store::CloudNodeStore;
use crate::sync::SyncPlan;

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
        // A complete immutable snapshot can be reused by the raw push path
        // without re-uploading its objects. Prove those bytes before HEAD is
        // allowed to move to that snapshot. Missing/incomplete snapshots are
        // deliberately left to raw synchronization, whose create/collision
        // path verifies every pre-existing object while repairing the gaps.
        let push_preflight_root = preflight_reused_provider_snapshot(settings, store)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node provider pre-push snapshot audit on pass {pass} failed: {error:#}"
                )
            })?;

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

        // If local/provider state moved between the preflight and the raw
        // transaction, conservatively audit an unexpected pushed root after the
        // publish. Normal pushes avoid this readback because the exact root was
        // already proven safe (or was forced through raw create/collision
        // repair) before publication.
        if matches!(
            result.action,
            ProviderSyncAction::Push | ProviderSyncAction::ForcedPush
        ) && push_preflight_root.as_deref() != Some(result.final_root.as_str())
        {
            match verify_stable_provider_snapshot_bytes(settings, store, &result.final_root).await {
                Ok(ProviderSnapshotAudit::Verified) => {}
                Ok(ProviderSnapshotAudit::LocalChanged) if pass < MAX_PROVIDER_STABILIZATION_PASSES => {
                    continue;
                }
                Ok(ProviderSnapshotAudit::LocalChanged) => {
                    anyhow::bail!(
                        "Cloud Node local snapshot changed during the final post-push provider integrity audit; refusing to report the pushed state as synchronized"
                    );
                }
                Err(audit_error) => {
                    store.verify().map_err(|local_error| {
                        anyhow::anyhow!(
                            "Cloud Node post-push provider integrity audit failed ({audit_error:#}), and the local snapshot also failed verification: {local_error:#}"
                        )
                    })?;
                    return Err(anyhow::anyhow!(
                        "Cloud Node pushed provider snapshot failed byte-for-byte integrity verification: {audit_error:#}"
                    ));
                }
            }
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
                        anyhow::bail!(
                            "Cloud Node provider state changed during the final integrity audit pass; refusing to report an unaudited synchronized state"
                        );
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
                    anyhow::bail!(
                        "Cloud Node local snapshot changed during the final provider integrity audit pass; refusing to report an unaudited synchronized state (relation={:?}, localHead={}, remoteHead={})",
                        status.relation,
                        status.local_head,
                        status.remote_head.as_deref().unwrap_or("<empty>")
                    );
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
                        anyhow::bail!(
                            "Cloud Node provider state changed while the final integrity audit was failing; refusing to classify the moved state as corrupted or synchronized (relation={:?}, localHead={}, remoteHead={})",
                            status.relation,
                            status.local_head,
                            status.remote_head.as_deref().unwrap_or("<empty>")
                        );
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

async fn preflight_reused_provider_snapshot(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<Option<String>> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let status = provider_sync_raw::provider_status(settings, store).await?;
    if !provider_push_expected(status.relation, provider.conflict_policy) {
        return Ok(None);
    }

    let plan = store.sync_plan()?;
    let root = plan.root_hex();
    if root != status.local_root {
        return Ok(None);
    }

    let client = ProviderClient::new(provider)?;
    let index_key = format!("snapshots/{root}/index.json");
    let Some(index_response) = client.open_download(&index_key, 0).await? else {
        // No immutable index means raw synchronization will build this snapshot
        // instead of taking the reuse fast path.
        return Ok(Some(root));
    };
    drop(index_response);

    if !provider_plan_resources_complete(&client, &plan, &root).await? {
        // An incomplete snapshot is intentionally repaired by raw sync. Its
        // collision path fully verifies every object that already exists.
        return Ok(Some(root));
    }

    match verify_stable_provider_snapshot_bytes(settings, store, &root).await? {
        ProviderSnapshotAudit::Verified => Ok(Some(root)),
        ProviderSnapshotAudit::LocalChanged => Ok(None),
    }
}

fn provider_push_expected(
    relation: ProviderSyncRelation,
    conflict_policy: ProviderConflictPolicy,
) -> bool {
    matches!(
        relation,
        ProviderSyncRelation::EmptyRemote | ProviderSyncRelation::LocalAhead
    ) || (relation == ProviderSyncRelation::Diverged
        && conflict_policy == ProviderConflictPolicy::PreferLocal)
}

async fn provider_plan_resources_complete(
    client: &ProviderClient,
    plan: &SyncPlan,
    root: &str,
) -> anyhow::Result<bool> {
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{root}/objects/{object_hex}/{content_hex}");

        let manifest_size = tokio::fs::metadata(&object.manifest_path)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to stat Cloud Node provider preflight manifest {}: {error}",
                    object.manifest_path.display()
                )
            })?
            .len();
        if !provider_resource_size_available(
            client,
            &format!("{base}/{}", object.kind.manifest_name()),
            manifest_size,
        )
        .await?
        {
            return Ok(false);
        }

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::File => {
                let payload = object.payload_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node file sync object has no payload during provider preflight"
                    )
                })?;
                let payload_size = tokio::fs::metadata(payload)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "failed to stat Cloud Node provider preflight payload {}: {error}",
                            payload.display()
                        )
                    })?
                    .len();
                if !provider_resource_size_available(client, &format!("{base}/payload"), payload_size)
                    .await?
                {
                    return Ok(false);
                }
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let chunk_name = chunk_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.strip_suffix(".chunk"))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Cloud Node provider preflight video chunk has invalid file name: {}",
                                chunk_path.display()
                            )
                        })?;
                    if !valid_sha256_hex(chunk_name) {
                        anyhow::bail!(
                            "Cloud Node provider preflight video chunk name is not a SHA-256 hash: {}",
                            chunk_path.display()
                        );
                    }
                    let chunk_size = tokio::fs::metadata(chunk_path)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "failed to stat Cloud Node provider preflight chunk {}: {error}",
                                chunk_path.display()
                            )
                        })?
                        .len();
                    if !provider_resource_size_available(
                        client,
                        &format!("{base}/chunks/{chunk_name}.chunk"),
                        chunk_size,
                    )
                    .await?
                    {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(true)
}

async fn provider_resource_size_available(
    client: &ProviderClient,
    key: &str,
    expected_size: u64,
) -> anyhow::Result<bool> {
    let start = expected_size.saturating_sub(1);
    let Some(mut response) = client.open_download(key, start).await? else {
        return Ok(false);
    };

    if expected_size == 0 {
        return match response.content_length() {
            Some(0) => Ok(true),
            Some(_) => Ok(false),
            None => Ok(response
                .chunk()
                .await
                .map_err(crate::provider::provider_transport_error)?
                .is_none()),
        };
    }

    if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cloud Node provider preflight range probe omitted Content-Range for {key:?}"
                )
            })?;
        let total = content_range_total(content_range).ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node provider preflight returned malformed Content-Range {content_range:?} for {key:?}"
            )
        })?;
        return Ok(total == expected_size);
    }

    if let Some(length) = response.content_length() {
        return Ok(length == expected_size);
    }

    Ok(false)
}

fn content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    fn preflight_only_targets_relations_that_can_publish_local_head() {
        assert!(provider_push_expected(
            ProviderSyncRelation::EmptyRemote,
            ProviderConflictPolicy::Fail
        ));
        assert!(provider_push_expected(
            ProviderSyncRelation::LocalAhead,
            ProviderConflictPolicy::Fail
        ));
        assert!(provider_push_expected(
            ProviderSyncRelation::Diverged,
            ProviderConflictPolicy::PreferLocal
        ));
        assert!(!provider_push_expected(
            ProviderSyncRelation::Diverged,
            ProviderConflictPolicy::Fail
        ));
        assert!(!provider_push_expected(
            ProviderSyncRelation::Diverged,
            ProviderConflictPolicy::PreferRemote
        ));
        assert!(!provider_push_expected(
            ProviderSyncRelation::RemoteAhead,
            ProviderConflictPolicy::PreferLocal
        ));
        assert!(!provider_push_expected(
            ProviderSyncRelation::InSync,
            ProviderConflictPolicy::PreferLocal
        ));
    }

    #[test]
    fn provider_preflight_hash_names_are_strict() {
        assert!(valid_sha256_hex(&"ab".repeat(32)));
        assert!(!valid_sha256_hex("abc"));
        assert!(!valid_sha256_hex(&format!("{}z", "a".repeat(63))));
    }

    #[test]
    fn provider_preflight_range_total_is_exact() {
        assert_eq!(content_range_total("bytes 4-4/5"), Some(5));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
        assert_eq!(content_range_total("garbage"), None);
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
