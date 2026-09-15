use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::client::{cached_resource_path, cleanup_outbound_cache, prepare_outbound_cache};
use crate::config::{CloudNodeSettings, ProviderConflictPolicy};
use crate::format::BlobKind;
use crate::provider::ProviderClient;
use crate::recovery::CloudNodeRecoveryReceiver;
use crate::store::CloudNodeStore;
use crate::sync::{SyncPlan, SyncPlanHeader};
use crate::transfer::{TransferChunk, TransferResource, MAX_TRANSFER_DATA_BYTES};

const HISTORY_VERSION: u16 = 1;
const SNAPSHOT_VERSION: u16 = 1;
const COMMIT_DOMAIN: &[u8] = b"RBE-CN-PROVIDER-COMMIT/1\0";
const MAX_HISTORY_DEPTH: usize = 100_000;
const PROVIDER_RECOVERY_OWNER_PREFIX: &str = "provider.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSyncRelation {
    EmptyRemote,
    InSync,
    LocalAhead,
    RemoteAhead,
    Diverged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSyncAction {
    None,
    Push,
    Pull,
    ForcedPush,
    ForcedPull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSyncStatus {
    pub relation: ProviderSyncRelation,
    pub local_head: String,
    pub remote_head: Option<String>,
    pub local_root: String,
    pub remote_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSyncResult {
    pub action: ProviderSyncAction,
    pub before: ProviderSyncStatus,
    pub final_root: String,
    pub final_head: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct HistoryCommit {
    format_version: u16,
    id: String,
    parent: Option<String>,
    snapshot_root: String,
    node_id: String,
    created_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadPointer {
    format_version: u16,
    commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSnapshot {
    format_version: u16,
    root_sha256: String,
    folder_count: u32,
    video_count: u32,
    file_count: u32,
    resources: Vec<ProviderResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderResource {
    kind: u8,
    resource: u8,
    object_key: String,
    content_sha256: String,
    resource_sha256: String,
    size: u64,
    key: String,
}

struct LocalHistory {
    commits: PathBuf,
    head: PathBuf,
}

impl LocalHistory {
    fn open(store: &CloudNodeStore, namespace: &str) -> anyhow::Result<Self> {
        let root = store
            .summary()
            .root
            .join("provider-history")
            .join(namespace);
        let commits = root.join("commits");
        fs::create_dir_all(&commits)?;
        Ok(Self {
            head: root.join("HEAD.json"),
            commits,
        })
    }

    fn head(&self) -> anyhow::Result<Option<HistoryCommit>> {
        if !self.head.is_file() {
            return Ok(None);
        }
        let pointer: HeadPointer = serde_json::from_slice(&fs::read(&self.head)?)?;
        validate_head_pointer(&pointer)?;
        self.read_commit(&pointer.commit).map(Some)
    }

    fn read_commit(&self, id: &str) -> anyhow::Result<HistoryCommit> {
        validate_hash(id, "history commit id")?;
        let path = self.commits.join(format!("{id}.json"));
        let commit: HistoryCommit = serde_json::from_slice(&fs::read(&path).map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node local history commit {}: {error}",
                path.display()
            )
        })?)?;
        validate_commit(&commit)?;
        if commit.id != id {
            anyhow::bail!("Cloud Node local history commit filename/id mismatch");
        }
        Ok(commit)
    }

    fn maybe_read_commit(&self, id: &str) -> anyhow::Result<Option<HistoryCommit>> {
        validate_hash(id, "history commit id")?;
        let path = self.commits.join(format!("{id}.json"));
        if !path.is_file() {
            return Ok(None);
        }
        self.read_commit(id).map(Some)
    }

    fn write_commit(&self, commit: &HistoryCommit) -> anyhow::Result<()> {
        validate_commit(commit)?;
        let path = self.commits.join(format!("{}.json", commit.id));
        if path.is_file() {
            let existing = self.read_commit(&commit.id)?;
            if existing != *commit {
                anyhow::bail!("Cloud Node local history commit id collision");
            }
            return Ok(());
        }
        atomic_json(&path, commit)
    }

    fn set_head(&self, commit: &HistoryCommit) -> anyhow::Result<()> {
        self.write_commit(commit)?;
        let pointer = HeadPointer {
            format_version: HISTORY_VERSION,
            commit: commit.id.clone(),
        };
        atomic_json(&self.head, &pointer)
    }

    fn ensure_snapshot_commit(
        &self,
        node_id: &str,
        snapshot_root: &str,
    ) -> anyhow::Result<HistoryCommit> {
        validate_hash(snapshot_root, "snapshot root")?;
        if let Some(head) = self.head()? {
            if head.snapshot_root == snapshot_root {
                return Ok(head);
            }
            let commit = new_commit(node_id, Some(head.id), snapshot_root)?;
            self.set_head(&commit)?;
            return Ok(commit);
        }
        let commit = new_commit(node_id, None, snapshot_root)?;
        self.set_head(&commit)?;
        Ok(commit)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> anyhow::Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        let mut current = descendant.to_owned();
        let mut seen = HashSet::new();
        for _ in 0..MAX_HISTORY_DEPTH {
            if !seen.insert(current.clone()) {
                anyhow::bail!("Cloud Node local provider history contains a cycle");
            }
            let Some(commit) = self.maybe_read_commit(&current)? else {
                return Ok(false);
            };
            let Some(parent) = commit.parent else {
                return Ok(false);
            };
            if parent == ancestor {
                return Ok(true);
            }
            current = parent;
        }
        anyhow::bail!("Cloud Node local provider history exceeded maximum depth")
    }

    fn chain_to_ancestor(
        &self,
        descendant: &str,
        stop_exclusive: Option<&str>,
    ) -> anyhow::Result<Vec<HistoryCommit>> {
        let mut out = Vec::new();
        let mut current = descendant.to_owned();
        let mut seen = HashSet::new();
        for _ in 0..MAX_HISTORY_DEPTH {
            if stop_exclusive.is_some_and(|stop| stop == current) {
                out.reverse();
                return Ok(out);
            }
            if !seen.insert(current.clone()) {
                anyhow::bail!("Cloud Node local provider history contains a cycle");
            }
            let commit = self.read_commit(&current)?;
            current = match &commit.parent {
                Some(parent) => parent.clone(),
                None => {
                    out.push(commit);
                    out.reverse();
                    return Ok(out);
                }
            };
            out.push(commit);
        }
        anyhow::bail!("Cloud Node local provider history exceeded maximum depth")
    }
}

fn working_snapshot_relation(
    history: &LocalHistory,
    local_head: &HistoryCommit,
    remote: Option<&HistoryCommit>,
    local_root: &str,
) -> anyhow::Result<ProviderSyncRelation> {
    let Some(remote) = remote else {
        return Ok(ProviderSyncRelation::EmptyRemote);
    };
    if remote.snapshot_root == local_root {
        return Ok(ProviderSyncRelation::InSync);
    }
    if remote.id == local_head.id || history.is_ancestor(&remote.id, &local_head.id)? {
        return Ok(ProviderSyncRelation::LocalAhead);
    }
    Ok(ProviderSyncRelation::Diverged)
}

pub async fn provider_status(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncStatus> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let client = ProviderClient::new(provider)?;
    let plan = store.sync_plan()?;
    let local_root = plan.root_hex();
    let history = LocalHistory::open(store, &provider.namespace)?;
    let remote = remote_head(&client).await?;
    let existing_head = history.head()?;

    let Some(local_head) = existing_head else {
        let relation = match remote.as_ref() {
            None => ProviderSyncRelation::EmptyRemote,
            Some(remote_head) if remote_head.snapshot_root == local_root => {
                ProviderSyncRelation::InSync
            }
            Some(_) if plan.object_count() == 0 => ProviderSyncRelation::RemoteAhead,
            Some(_) => ProviderSyncRelation::Diverged,
        };
        return Ok(ProviderSyncStatus {
            relation,
            local_head: "<untracked>".to_owned(),
            remote_head: remote.as_ref().map(|head| head.id.clone()),
            local_root,
            remote_root: remote.as_ref().map(|head| head.snapshot_root.clone()),
        });
    };

    if local_head.snapshot_root != local_root {
        let relation =
            working_snapshot_relation(&history, &local_head, remote.as_ref(), &local_root)?;
        return Ok(ProviderSyncStatus {
            relation,
            local_head: local_head.id,
            remote_head: remote.as_ref().map(|head| head.id.clone()),
            local_root,
            remote_root: remote.as_ref().map(|head| head.snapshot_root.clone()),
        });
    }

    status_from_heads(&history, &client, local_head, remote, local_root).await
}

pub async fn synchronize_provider(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncResult> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let client = ProviderClient::new(provider)?;
    let recovery_owner = provider_recovery_owner(&provider.namespace);
    let plan = store.sync_plan()?;
    let local_root = plan.root_hex();
    let history = LocalHistory::open(store, &provider.namespace)?;
    let remote = remote_head(&client).await?;
    let existing_head = history.head()?;

    if let Some(remote_head) = remote.as_ref() {
        if should_adopt_matching_remote_history(existing_head.as_ref(), &local_root, remote_head) {
            import_remote_history(&history, &client, remote_head).await?;
            let before = ProviderSyncStatus {
                relation: ProviderSyncRelation::InSync,
                local_head: remote_head.id.clone(),
                remote_head: Some(remote_head.id.clone()),
                local_root: local_root.clone(),
                remote_root: Some(remote_head.snapshot_root.clone()),
            };
            return Ok(ProviderSyncResult {
                action: ProviderSyncAction::None,
                final_root: local_root,
                final_head: remote_head.id.clone(),
                before,
            });
        }
    }

    if existing_head.is_none() {
        if let Some(remote_head) = remote.as_ref() {
            if plan.object_count() == 0 {
                let before = ProviderSyncStatus {
                    relation: ProviderSyncRelation::RemoteAhead,
                    local_head: "<untracked>".to_owned(),
                    remote_head: Some(remote_head.id.clone()),
                    local_root,
                    remote_root: Some(remote_head.snapshot_root.clone()),
                };
                pull_provider_state(&client, &history, store, remote_head, &recovery_owner).await?;
                return Ok(ProviderSyncResult {
                    action: ProviderSyncAction::Pull,
                    final_root: remote_head.snapshot_root.clone(),
                    final_head: remote_head.id.clone(),
                    before,
                });
            }
        }
    }

    let local_head = history.ensure_snapshot_commit(&settings.node.id, &local_root)?;
    let before = status_from_heads(
        &history,
        &client,
        local_head.clone(),
        remote.clone(),
        local_root,
    )
    .await?;

    match before.relation {
        ProviderSyncRelation::InSync => Ok(ProviderSyncResult {
            action: ProviderSyncAction::None,
            final_root: before.local_root.clone(),
            final_head: before.local_head.clone(),
            before,
        }),
        ProviderSyncRelation::EmptyRemote | ProviderSyncRelation::LocalAhead => {
            push_provider_state(
                &client,
                &history,
                store,
                &plan,
                &local_head,
                remote.as_ref(),
            )
            .await?;
            Ok(ProviderSyncResult {
                action: ProviderSyncAction::Push,
                final_root: local_head.snapshot_root.clone(),
                final_head: local_head.id.clone(),
                before,
            })
        }
        ProviderSyncRelation::RemoteAhead => {
            let remote = remote.ok_or_else(|| anyhow::anyhow!("Cloud Node provider remote head disappeared"))?;
            pull_provider_state(&client, &history, store, &remote, &recovery_owner).await?;
            Ok(ProviderSyncResult {
                action: ProviderSyncAction::Pull,
                final_root: remote.snapshot_root.clone(),
                final_head: remote.id.clone(),
                before,
            })
        }
        ProviderSyncRelation::Diverged => match provider.conflict_policy {
            ProviderConflictPolicy::Fail => anyhow::bail!(
                "Cloud Node provider history diverged: local={} remote={}; set conflictPolicy explicitly only if one side should win",
                before.local_head,
                before.remote_head.as_deref().unwrap_or("<none>")
            ),
            ProviderConflictPolicy::PreferLocal => {
                push_provider_state(
                &client,
                &history,
                store,
                &plan,
                &local_head,
                remote.as_ref(),
            )
            .await?;
                Ok(ProviderSyncResult {
                    action: ProviderSyncAction::ForcedPush,
                    final_root: local_head.snapshot_root.clone(),
                    final_head: local_head.id.clone(),
                    before,
                })
            }
            ProviderConflictPolicy::PreferRemote => {
                let remote = remote.ok_or_else(|| anyhow::anyhow!("Cloud Node provider remote head disappeared"))?;
                pull_provider_state(&client, &history, store, &remote, &recovery_owner).await?;
                Ok(ProviderSyncResult {
                    action: ProviderSyncAction::ForcedPull,
                    final_root: remote.snapshot_root.clone(),
                    final_head: remote.id.clone(),
                    before,
                })
            }
        },
    }
}

fn provider_cache_owner(namespace: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-PROVIDER-CACHE/1\0");
    digest.update(namespace.as_bytes());
    format!("provider-{}", hex::encode(digest.finalize()))
}

fn provider_recovery_owner(namespace: &str) -> String {
    format!("{PROVIDER_RECOVERY_OWNER_PREFIX}{namespace}")
}

fn should_adopt_matching_remote_history(
    local_head: Option<&HistoryCommit>,
    local_root: &str,
    remote_head: &HistoryCommit,
) -> bool {
    remote_head.snapshot_root == local_root
        && local_head
            .map(|head| head.snapshot_root.as_str() != local_root)
            .unwrap_or(true)
}

async fn status_from_heads(
    history: &LocalHistory,
    client: &ProviderClient,
    local: HistoryCommit,
    remote: Option<HistoryCommit>,
    local_root: String,
) -> anyhow::Result<ProviderSyncStatus> {
    let Some(remote) = remote else {
        return Ok(ProviderSyncStatus {
            relation: ProviderSyncRelation::EmptyRemote,
            local_head: local.id,
            remote_head: None,
            local_root,
            remote_root: None,
        });
    };
    let relation = if local.id == remote.id {
        ProviderSyncRelation::InSync
    } else if history.is_ancestor(&remote.id, &local.id)? {
        ProviderSyncRelation::LocalAhead
    } else if remote_is_ancestor(client, &local.id, &remote.id).await? {
        ProviderSyncRelation::RemoteAhead
    } else {
        ProviderSyncRelation::Diverged
    };
    Ok(ProviderSyncStatus {
        relation,
        local_head: local.id,
        remote_head: Some(remote.id),
        local_root,
        remote_root: Some(remote.snapshot_root),
    })
}

async fn push_provider_state(
    client: &ProviderClient,
    history: &LocalHistory,
    store: &CloudNodeStore,
    plan: &SyncPlan,
    local_head: &HistoryCommit,
    expected_remote: Option<&HistoryCommit>,
) -> anyhow::Result<()> {
    let cache_owner = provider_cache_owner(client.namespace());
    upload_snapshot(client, store, plan, &cache_owner).await?;
    let commits = history.chain_to_ancestor(
        &local_head.id,
        expected_remote.map(|commit| commit.id.as_str()),
    )?;
    for commit in commits {
        upload_commit(client, &commit).await?;
    }

    let current_remote = remote_head(client).await?;
    let expected_id = expected_remote.map(|commit| commit.id.as_str());
    let current_id = current_remote.as_ref().map(|commit| commit.id.as_str());
    if current_id != expected_id {
        anyhow::bail!(
            "Cloud Node provider HEAD changed during push: expected {} but found {}; retry synchronization",
            expected_id.unwrap_or("<empty>"),
            current_id.unwrap_or("<empty>")
        );
    }
    upload_head(client, local_head).await?;
    cleanup_outbound_cache(store, &cache_owner).await
}

async fn pull_provider_state(
    client: &ProviderClient,
    history: &LocalHistory,
    store: &CloudNodeStore,
    remote_head: &HistoryCommit,
    recovery_owner: &str,
) -> anyhow::Result<()> {
    restore_snapshot(client, store, &remote_head.snapshot_root, recovery_owner).await?;
    import_remote_history(history, client, remote_head).await
}

async fn import_remote_history(
    history: &LocalHistory,
    client: &ProviderClient,
    remote_head: &HistoryCommit,
) -> anyhow::Result<()> {
    let missing = remote_chain_until_local(history, client, remote_head).await?;
    for commit in missing.iter().rev() {
        history.write_commit(commit)?;
    }
    history.set_head(remote_head)
}

async fn remote_chain_until_local(
    history: &LocalHistory,
    client: &ProviderClient,
    start: &HistoryCommit,
) -> anyhow::Result<Vec<HistoryCommit>> {
    let mut out = Vec::new();
    let mut current = start.clone();
    let mut seen = HashSet::new();
    for _ in 0..MAX_HISTORY_DEPTH {
        if history.maybe_read_commit(&current.id)?.is_some() {
            return Ok(out);
        }
        if !seen.insert(current.id.clone()) {
            anyhow::bail!("Cloud Node remote provider history contains a cycle");
        }
        out.push(current.clone());
        let Some(parent) = &current.parent else {
            return Ok(out);
        };
        current = fetch_remote_commit(client, parent).await?.ok_or_else(|| {
            anyhow::anyhow!("Cloud Node provider history is missing commit {parent}")
        })?;
    }
    anyhow::bail!("Cloud Node remote provider history exceeded maximum depth")
}

async fn remote_is_ancestor(
    client: &ProviderClient,
    ancestor: &str,
    descendant: &str,
) -> anyhow::Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut current = descendant.to_owned();
    let mut seen = HashSet::new();
    for _ in 0..MAX_HISTORY_DEPTH {
        if !seen.insert(current.clone()) {
            anyhow::bail!("Cloud Node remote provider history contains a cycle");
        }
        let Some(commit) = fetch_remote_commit(client, &current).await? else {
            return Ok(false);
        };
        let Some(parent) = commit.parent else {
            return Ok(false);
        };
        if parent == ancestor {
            return Ok(true);
        }
        current = parent;
    }
    anyhow::bail!("Cloud Node remote provider history exceeded maximum depth")
}

async fn remote_head(client: &ProviderClient) -> anyhow::Result<Option<HistoryCommit>> {
    let Some(bytes) = client.get("history/HEAD.json").await? else {
        return Ok(None);
    };
    let pointer: HeadPointer = serde_json::from_slice(&bytes)?;
    validate_head_pointer(&pointer)?;
    fetch_remote_commit(client, &pointer.commit)
        .await?
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node provider HEAD references missing commit {}",
                pointer.commit
            )
        })
}

async fn fetch_remote_commit(
    client: &ProviderClient,
    id: &str,
) -> anyhow::Result<Option<HistoryCommit>> {
    validate_hash(id, "history commit id")?;
    let Some(bytes) = client.get(&format!("history/commits/{id}.json")).await? else {
        return Ok(None);
    };
    let commit: HistoryCommit = serde_json::from_slice(&bytes)?;
    validate_commit(&commit)?;
    if commit.id != id {
        anyhow::bail!("Cloud Node provider history commit filename/id mismatch");
    }
    Ok(Some(commit))
}

async fn upload_commit(client: &ProviderClient, commit: &HistoryCommit) -> anyhow::Result<()> {
    validate_commit(commit)?;
    client
        .put(
            &format!("history/commits/{}.json", commit.id),
            serde_json::to_vec(commit)?,
            "application/json",
        )
        .await
}

async fn upload_head(client: &ProviderClient, commit: &HistoryCommit) -> anyhow::Result<()> {
    let pointer = HeadPointer {
        format_version: HISTORY_VERSION,
        commit: commit.id.clone(),
    };
    client
        .put(
            "history/HEAD.json",
            serde_json::to_vec(&pointer)?,
            "application/json",
        )
        .await
}

async fn upload_snapshot(
    client: &ProviderClient,
    store: &CloudNodeStore,
    plan: &SyncPlan,
    cache_owner: &str,
) -> anyhow::Result<()> {
    let root = plan.root_hex();
    let index_key = format!("snapshots/{root}/index.json");
    if let Some(existing) = client.get(&index_key).await? {
        let snapshot: ProviderSnapshot = serde_json::from_slice(&existing)?;
        validate_snapshot(&snapshot)?;
        if snapshot.root_sha256 == root {
            return Ok(());
        }
        anyhow::bail!("Cloud Node provider snapshot index collision for root {root}");
    }

    let cache_root = prepare_outbound_cache(store, cache_owner, plan).await?;
    let header = plan.header()?;
    let mut resources = Vec::new();
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{root}/objects/{object_hex}/{content_hex}");

        let manifest_path = cached_resource_path(
            &cache_root,
            object,
            TransferResource::Manifest,
            &object.manifest_path,
        )?;
        let (manifest_sha, manifest_size) = hash_and_size(&manifest_path).await?;
        let manifest_key = format!("{base}/{}", object.kind.manifest_name());
        client
            .put_file(
                &manifest_key,
                &manifest_path,
                "application/octet-stream",
                manifest_sha,
            )
            .await?;
        resources.push(resource_record(
            object.kind,
            TransferResource::Manifest,
            object.object_key,
            object.content_sha256,
            manifest_sha,
            manifest_size,
            manifest_key,
        )?);

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::File => {
                let payload_path = object
                    .payload_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Cloud Node file sync object has no payload"))?;
                let cached_payload = cached_resource_path(
                    &cache_root,
                    object,
                    TransferResource::FilePayload,
                    payload_path,
                )?;
                let payload_size = tokio::fs::metadata(&cached_payload).await?.len();
                let payload_key = format!("{base}/payload");
                client
                    .put_file(
                        &payload_key,
                        &cached_payload,
                        "application/octet-stream",
                        object.content_sha256,
                    )
                    .await?;
                resources.push(resource_record(
                    object.kind,
                    TransferResource::FilePayload,
                    object.object_key,
                    object.content_sha256,
                    object.content_sha256,
                    payload_size,
                    payload_key,
                )?);
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let cached_chunk = cached_resource_path(
                        &cache_root,
                        object,
                        TransferResource::VideoChunk,
                        chunk_path,
                    )?;
                    let (chunk_sha, chunk_size) = hash_and_size(&cached_chunk).await?;
                    let chunk_hex = hex::encode(chunk_sha);
                    let chunk_key = format!("{base}/chunks/{chunk_hex}.chunk");
                    client
                        .put_file(
                            &chunk_key,
                            &cached_chunk,
                            "application/octet-stream",
                            chunk_sha,
                        )
                        .await?;
                    resources.push(resource_record(
                        object.kind,
                        TransferResource::VideoChunk,
                        object.object_key,
                        object.content_sha256,
                        chunk_sha,
                        chunk_size,
                        chunk_key,
                    )?);
                }
            }
        }
    }

    let snapshot = ProviderSnapshot {
        format_version: SNAPSHOT_VERSION,
        root_sha256: root,
        folder_count: header.folder_count,
        video_count: header.video_count,
        file_count: header.file_count,
        resources,
    };
    validate_snapshot(&snapshot)?;
    client
        .put(
            &index_key,
            serde_json::to_vec(&snapshot)?,
            "application/json",
        )
        .await
}

async fn hash_and_size(path: &Path) -> anyhow::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        size = size
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| anyhow::anyhow!("provider upload read size exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("provider upload size overflow"))?;
    }
    Ok((digest.finalize().into(), size))
}

async fn restore_snapshot(
    client: &ProviderClient,
    store: &CloudNodeStore,
    root: &str,
    recovery_owner: &str,
) -> anyhow::Result<()> {
    validate_hash(root, "snapshot root")?;
    let index_key = format!("snapshots/{root}/index.json");
    let bytes = client
        .get(&index_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider snapshot {root} is missing"))?;
    let snapshot: ProviderSnapshot = serde_json::from_slice(&bytes)?;
    validate_snapshot(&snapshot)?;
    if snapshot.root_sha256 != root {
        anyhow::bail!("Cloud Node provider snapshot root does not match requested history commit");
    }

    let expected = SyncPlanHeader {
        root_sha256: decode_hash(root, "snapshot root")?,
        folder_count: snapshot.folder_count,
        video_count: snapshot.video_count,
        file_count: snapshot.file_count,
    };
    let mut receiver = CloudNodeRecoveryReceiver::open(store, recovery_owner, expected)?;
    for resource in &snapshot.resources {
        restore_resource(client, &mut receiver, resource).await?;
    }
    receiver.complete(store, expected)?;
    Ok(())
}

async fn restore_resource(
    client: &ProviderClient,
    receiver: &mut CloudNodeRecoveryReceiver,
    resource: &ProviderResource,
) -> anyhow::Result<()> {
    let resource_sha = decode_hash(&resource.resource_sha256, "provider resource hash")?;
    let kind = BlobKind::try_from(resource.kind)?;
    let transfer_resource = TransferResource::try_from(resource.resource)?;
    let object_key = decode_hash(&resource.object_key, "provider object key")?;
    let content_sha = decode_hash(&resource.content_sha256, "provider content hash")?;
    let total_size = resource.size;

    if total_size == 0 {
        let mut response = client
            .open_download(&resource.key, 0)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cloud Node provider snapshot resource {} is missing",
                    resource.key
                )
            })?;
        if response.chunk().await?.is_some() {
            anyhow::bail!(
                "Cloud Node provider resource size mismatch for {}",
                resource.key
            );
        }
        let chunk = TransferChunk::new(
            kind,
            transfer_resource,
            object_key,
            content_sha,
            resource_sha,
            0,
            0,
            Vec::new(),
        )?;
        receiver.accept_chunk(&chunk)?;
        return Ok(());
    }

    // Ask recovery for the already durable prefix before opening the provider
    // body. This mirrors the peer resume acknowledgement path.
    let probe = TransferChunk::new(
        kind,
        transfer_resource,
        object_key,
        content_sha,
        resource_sha,
        0,
        total_size,
        Vec::new(),
    )?;
    let receipt = receiver.accept_chunk(&probe)?;
    let mut offset = receipt.next_offset;
    if offset > total_size {
        anyhow::bail!(
            "Cloud Node provider recovery returned invalid resume offset {offset} for resource size {total_size}"
        );
    }
    if offset == total_size {
        return Ok(());
    }

    let mut response = client
        .open_download(&resource.key, offset)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node provider snapshot resource {} is missing",
                resource.key
            )
        })?;
    let ranged = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut source_offset = if ranged { offset } else { 0 };

    while let Some(bytes) = response.chunk().await? {
        let chunk_start = source_offset;
        source_offset = source_offset
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| anyhow::anyhow!("provider response chunk size exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("provider response offset overflow"))?;

        if source_offset <= offset {
            continue;
        }
        let begin = if chunk_start < offset {
            usize::try_from(offset - chunk_start)
                .map_err(|_| anyhow::anyhow!("provider resume prefix exceeds platform range"))?
        } else {
            0
        };
        let mut remaining = &bytes[begin..];
        while !remaining.is_empty() {
            if offset >= total_size {
                anyhow::bail!(
                    "Cloud Node provider resource exceeded declared size for {}",
                    resource.key
                );
            }
            let resource_remaining = usize::try_from(total_size - offset).unwrap_or(usize::MAX);
            let take = remaining
                .len()
                .min(MAX_TRANSFER_DATA_BYTES)
                .min(resource_remaining);
            if take == 0 {
                anyhow::bail!(
                    "Cloud Node provider resource exceeded declared size for {}",
                    resource.key
                );
            }
            let sent_end = offset
                .checked_add(
                    u64::try_from(take)
                        .map_err(|_| anyhow::anyhow!("provider transfer chunk exceeds u64"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("provider transfer offset overflow"))?;
            let chunk = TransferChunk::new(
                kind,
                transfer_resource,
                object_key,
                content_sha,
                resource_sha,
                offset,
                total_size,
                remaining[..take].to_vec(),
            )?;
            let receipt = receiver.accept_chunk(&chunk)?;
            if receipt.next_offset != sent_end {
                anyhow::bail!(
                    "Cloud Node provider recovery advanced to unexpected offset {} instead of {}",
                    receipt.next_offset,
                    sent_end
                );
            }
            offset = receipt.next_offset;
            remaining = &remaining[take..];
        }
    }

    if offset != total_size {
        anyhow::bail!(
            "Cloud Node provider resource ended at offset {offset} instead of declared size {total_size} for {}",
            resource.key
        );
    }
    Ok(())
}

fn resource_record(
    kind: BlobKind,
    resource: TransferResource,
    object_key: [u8; 32],
    content_sha256: [u8; 32],
    resource_sha256: [u8; 32],
    size: u64,
    key: String,
) -> anyhow::Result<ProviderResource> {
    Ok(ProviderResource {
        kind: kind as u8,
        resource: resource as u8,
        object_key: hex::encode(object_key),
        content_sha256: hex::encode(content_sha256),
        resource_sha256: hex::encode(resource_sha256),
        size,
        key,
    })
}

fn validate_snapshot(snapshot: &ProviderSnapshot) -> anyhow::Result<()> {
    if snapshot.format_version != SNAPSHOT_VERSION {
        anyhow::bail!(
            "unsupported Cloud Node provider snapshot version {}",
            snapshot.format_version
        );
    }
    validate_hash(&snapshot.root_sha256, "snapshot root")?;
    let mut phase = 0u8;
    let mut folders = HashSet::new();
    let mut videos = HashSet::new();
    let mut files = HashSet::new();
    for resource in &snapshot.resources {
        validate_hash(&resource.object_key, "provider object key")?;
        validate_hash(&resource.content_sha256, "provider content hash")?;
        validate_hash(&resource.resource_sha256, "provider resource hash")?;
        if resource.key.is_empty() || resource.key.starts_with('/') || resource.key.contains("..") {
            anyhow::bail!("Cloud Node provider snapshot contains invalid resource key");
        }
        let kind = BlobKind::try_from(resource.kind)?;
        let transfer = TransferResource::try_from(resource.resource)?;
        let required_phase = match kind {
            BlobKind::Folder => 0,
            BlobKind::Video => 1,
            BlobKind::File => 2,
        };
        if required_phase < phase {
            anyhow::bail!("Cloud Node provider snapshot resource order regressed");
        }
        phase = required_phase;
        match transfer {
            TransferResource::Manifest => match kind {
                BlobKind::Folder => {
                    folders.insert(resource.object_key.clone());
                }
                BlobKind::Video => {
                    videos.insert(resource.object_key.clone());
                }
                BlobKind::File => {
                    files.insert(resource.object_key.clone());
                }
            },
            TransferResource::FilePayload if kind == BlobKind::File => {}
            TransferResource::VideoChunk if kind == BlobKind::Video => {}
            _ => {
                anyhow::bail!("Cloud Node provider snapshot contains invalid resource kind pairing")
            }
        }
    }
    if usize::try_from(snapshot.folder_count).ok() != Some(folders.len())
        || usize::try_from(snapshot.video_count).ok() != Some(videos.len())
        || usize::try_from(snapshot.file_count).ok() != Some(files.len())
    {
        anyhow::bail!("Cloud Node provider snapshot object counts do not match manifest resources");
    }
    Ok(())
}

fn new_commit(
    node_id: &str,
    parent: Option<String>,
    snapshot_root: &str,
) -> anyhow::Result<HistoryCommit> {
    let created_unix_ms = now_ms()?;
    let id = commit_id(node_id, parent.as_deref(), snapshot_root, created_unix_ms);
    Ok(HistoryCommit {
        format_version: HISTORY_VERSION,
        id,
        parent,
        snapshot_root: snapshot_root.to_owned(),
        node_id: node_id.to_owned(),
        created_unix_ms,
    })
}

fn commit_id(node_id: &str, parent: Option<&str>, root: &str, created_unix_ms: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(COMMIT_DOMAIN);
    digest.update((node_id.len() as u64).to_be_bytes());
    digest.update(node_id.as_bytes());
    match parent {
        Some(parent) => {
            digest.update([1]);
            digest.update(parent.as_bytes());
        }
        None => digest.update([0]),
    }
    digest.update(root.as_bytes());
    digest.update(created_unix_ms.to_be_bytes());
    hex::encode(digest.finalize())
}

fn validate_commit(commit: &HistoryCommit) -> anyhow::Result<()> {
    if commit.format_version != HISTORY_VERSION {
        anyhow::bail!(
            "unsupported Cloud Node provider history version {}",
            commit.format_version
        );
    }
    validate_hash(&commit.id, "history commit id")?;
    validate_hash(&commit.snapshot_root, "snapshot root")?;
    if let Some(parent) = &commit.parent {
        validate_hash(parent, "history parent id")?;
        if parent == &commit.id {
            anyhow::bail!("Cloud Node provider history commit cannot parent itself");
        }
    }
    let expected = commit_id(
        &commit.node_id,
        commit.parent.as_deref(),
        &commit.snapshot_root,
        commit.created_unix_ms,
    );
    if expected != commit.id {
        anyhow::bail!("Cloud Node provider history commit digest mismatch");
    }
    Ok(())
}

fn validate_head_pointer(pointer: &HeadPointer) -> anyhow::Result<()> {
    if pointer.format_version != HISTORY_VERSION {
        anyhow::bail!(
            "unsupported Cloud Node provider HEAD version {}",
            pointer.format_version
        );
    }
    validate_hash(&pointer.commit, "history HEAD commit")
}

fn validate_hash(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("Cloud Node {label} must be a 32-byte hexadecimal SHA-256 value");
    }
    Ok(())
}

fn decode_hash(value: &str, label: &str) -> anyhow::Result<[u8; 32]> {
    validate_hash(value, label)?;
    let bytes = hex::decode(value)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Cloud Node {label} has invalid decoded length"))
}

fn atomic_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    AtomicIo::new()
        .write_atomic(path, &bytes)
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to durably replace Cloud Node provider history {}: {error}",
                path.display()
            )
        })?;
    Ok(())
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is before UNIX epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudNodeSettings;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-provider-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn provider_cache_owner_is_stable_and_path_safe() {
        let first = provider_cache_owner("prod-primary");
        let second = provider_cache_owner("prod-primary");
        let other = provider_cache_owner("prod-secondary");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.starts_with("provider-"));
        assert!(!first.contains('/'));
        assert!(!first.contains('\\'));
        assert!(!first.contains(".."));
    }

    #[test]
    fn working_snapshot_status_does_not_advance_local_history() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node":{"id":"node-a","storageRoot":root},
            "provider":{
                "kind":"google-cloud-storage",
                "namespace":"prod",
                "bucket":"rbe"
            }
        }))
        .unwrap();
        let store = CloudNodeStore::open(&settings).unwrap();
        let history = LocalHistory::open(&store, "prod").unwrap();
        let remote = history
            .ensure_snapshot_commit("node-a", &"11".repeat(32))
            .unwrap();
        let local = history
            .ensure_snapshot_commit("node-a", &"22".repeat(32))
            .unwrap();
        let head_before = fs::read(&history.head).unwrap();

        let relation =
            working_snapshot_relation(&history, &local, Some(&remote), &"33".repeat(32)).unwrap();

        assert_eq!(relation, ProviderSyncRelation::LocalAhead);
        assert_eq!(fs::read(&history.head).unwrap(), head_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_history_tracks_snapshot_ancestry() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node":{"id":"node-a","storageRoot":root},
            "provider":{
                "kind":"google-cloud-storage",
                "namespace":"prod",
                "bucket":"rbe"
            }
        }))
        .unwrap();
        let store = CloudNodeStore::open(&settings).unwrap();
        let history = LocalHistory::open(&store, "prod").unwrap();
        let first = history
            .ensure_snapshot_commit("node-a", &"11".repeat(32))
            .unwrap();
        let second = history
            .ensure_snapshot_commit("node-a", &"22".repeat(32))
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(history.is_ancestor(&first.id, &second.id).unwrap());
        assert!(!history.is_ancestor(&second.id, &first.id).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matching_remote_root_repairs_only_stale_or_missing_history() {
        let live_root = "11".repeat(32);
        let remote = HistoryCommit {
            format_version: HISTORY_VERSION,
            id: "22".repeat(32),
            parent: None,
            snapshot_root: live_root.clone(),
            node_id: "remote".into(),
            created_unix_ms: 1,
        };
        let stale = HistoryCommit {
            format_version: HISTORY_VERSION,
            id: "33".repeat(32),
            parent: None,
            snapshot_root: "44".repeat(32),
            node_id: "local".into(),
            created_unix_ms: 2,
        };
        let same_content_other_history = HistoryCommit {
            format_version: HISTORY_VERSION,
            id: "55".repeat(32),
            parent: None,
            snapshot_root: live_root.clone(),
            node_id: "local".into(),
            created_unix_ms: 3,
        };
        let other_remote = HistoryCommit {
            snapshot_root: "66".repeat(32),
            ..remote.clone()
        };

        assert!(should_adopt_matching_remote_history(
            None, &live_root, &remote
        ));
        assert!(should_adopt_matching_remote_history(
            Some(&stale),
            &live_root,
            &remote
        ));
        assert!(!should_adopt_matching_remote_history(
            Some(&same_content_other_history),
            &live_root,
            &remote
        ));
        assert!(!should_adopt_matching_remote_history(
            Some(&stale),
            &live_root,
            &other_remote
        ));
    }

    #[test]
    fn snapshot_validation_rejects_phase_regression() {
        let snapshot = ProviderSnapshot {
            format_version: 1,
            root_sha256: "11".repeat(32),
            folder_count: 0,
            video_count: 1,
            file_count: 1,
            resources: vec![
                ProviderResource {
                    kind: BlobKind::File as u8,
                    resource: TransferResource::Manifest as u8,
                    object_key: "22".repeat(32),
                    content_sha256: "33".repeat(32),
                    resource_sha256: "44".repeat(32),
                    size: 1,
                    key: "file".into(),
                },
                ProviderResource {
                    kind: BlobKind::Video as u8,
                    resource: TransferResource::Manifest as u8,
                    object_key: "55".repeat(32),
                    content_sha256: "66".repeat(32),
                    resource_sha256: "77".repeat(32),
                    size: 1,
                    key: "video".into(),
                },
            ],
        };
        assert!(validate_snapshot(&snapshot).is_err());
    }
}
