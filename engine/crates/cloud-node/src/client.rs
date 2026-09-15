use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::auth::{random_session_and_nonce, NodeProof, DEFAULT_AUTH_SKEW_MS};
use crate::config::CloudNodeSettings;
use crate::crypto::load_signing_key_from_env;
use crate::format::BlobKind;
use crate::protocol::{Frame, FrameKind};
use crate::store::CloudNodeStore;
use crate::sync::{SyncObject, SyncPlan, SyncPlanHeader};
use crate::transfer::{
    TransferChunk, TransferResource, MAX_TRANSFER_DATA_BYTES, RESUME_ACK_HEADER,
};

pub const KNOCK_PATH: &str = "/.rbe/cn/v1/knock";
pub const SYNC_PATH: &str = "/.rbe/cn/v1/sync";
pub const TRANSFER_PATH: &str = "/.rbe/cn/v1/transfer";
pub const SESSION_PROOF_HEADER: &str = "x-rbe-cn-proof";
const MAX_PROOF_RESPONSE_BYTES: usize = 1024;
const MAX_SYNC_RESPONSE_BYTES: usize = 4096;
const MAX_TRANSFER_RESPONSE_BYTES: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    pub node_id: String,
    pub session: [u8; 16],
    pub peer_nonce: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncNegotiation {
    pub local: SyncPlanHeader,
    pub remote: SyncPlanHeader,
}

impl SyncNegotiation {
    pub fn roots_match(&self) -> bool {
        self.local.root_sha256 == self.remote.root_sha256
    }
}

pub async fn probe_upstream(settings: &CloudNodeSettings) -> anyhow::Result<AuthenticatedPeer> {
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node has no configured upstream"))?;
    let signing = load_signing_key_from_env()?;
    let (session, nonce) = random_session_and_nonce();
    let knock = NodeProof::knock(&signing, &settings.node.id, now_ms()?, session, nonce)?;
    let endpoint = format!("{}{}", upstream.url.trim_end_matches('/'), KNOCK_PATH);
    let client = http_client()?;
    let response = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(knock.encode()?)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        anyhow::bail!("Cloud Node upstream did not accept authentication");
    }
    let advertised = response.content_length().unwrap_or(0);
    if advertised > MAX_PROOF_RESPONSE_BYTES as u64 {
        anyhow::bail!("Cloud Node upstream authentication response is oversized");
    }
    let body = response.bytes().await?;
    if body.len() > MAX_PROOF_RESPONSE_BYTES {
        anyhow::bail!("Cloud Node upstream authentication response is oversized");
    }
    let accept = NodeProof::decode(&body)?;
    accept.verify_accepts(&knock)?;
    accept.verify_freshness(now_ms()?, DEFAULT_AUTH_SKEW_MS)?;
    accept.verify_identity(&upstream.node_id, &upstream.public_key)?;
    Ok(AuthenticatedPeer {
        node_id: accept.node_id,
        session,
        peer_nonce: accept.nonce,
    })
}

/// Compares the local recovery root with the authenticated upstream before any
/// object transfer is attempted. The request itself is signed and bound to the
/// server nonce from the mutual-authentication handshake.
pub async fn negotiate_sync(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    peer: &AuthenticatedPeer,
) -> anyhow::Result<SyncNegotiation> {
    ensure_expected_peer(settings, peer)?;
    let local = store.sync_plan()?.header()?;
    let request = Frame {
        kind: FrameKind::SyncHello,
        session: peer.session,
        payload: local.encode(),
    };
    let client = http_client()?;
    let response = post_authenticated_frame(
        &client,
        settings,
        peer,
        SYNC_PATH,
        request,
        MAX_SYNC_RESPONSE_BYTES,
        false,
    )
    .await?;
    if response.kind != FrameKind::SyncHello {
        anyhow::bail!("Cloud Node upstream returned an invalid sync negotiation frame");
    }
    let remote = SyncPlanHeader::decode(&response.payload)?;
    Ok(SyncNegotiation { local, remote })
}

/// Pushes one complete LOCAL snapshot to the authenticated upstream when the
/// negotiated roots differ. Recovery is deliberately full-snapshot rather than
/// merge-based: stale objects on the remote side must disappear as part of the
/// verified storage-tree swap.
pub async fn synchronize_upstream(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    peer: &AuthenticatedPeer,
) -> anyhow::Result<SyncNegotiation> {
    let negotiation = negotiate_sync(settings, store, peer).await?;
    if negotiation.roots_match() {
        // A matching REMOTE snapshot means any older interrupted LOCAL
        // transport spool for this peer is no longer useful.
        let _ = cleanup_outbound_cache(store, &peer.node_id).await;
        return Ok(negotiation);
    }

    // Never freeze or transmit a LOCAL snapshot whose underlying blobs
    // already fail Cloud Node integrity verification.
    store.verify()?;
    let plan = store.sync_plan()?;
    let local = plan.header()?;
    if local != negotiation.local {
        anyhow::bail!("Cloud Node local snapshot changed after sync negotiation; retry required");
    }

    // Freeze the complete transfer set before the first object is sent.
    // Large resources are copied through disk-backed .part files rather
    // than accumulated in RAM. A failed upload leaves this root intact,
    // so a later daemon reconnect can reuse it byte-for-byte.
    let cache_root = prepare_outbound_cache(store, &peer.node_id, &plan).await?;

    let client = http_client()?;
    for object in plan.ordered() {
        let cached_manifest = cached_resource_path(
            &cache_root,
            object,
            TransferResource::Manifest,
            &object.manifest_path,
        )?;
        let manifest_hash = sha256_path(&cached_manifest).await?;
        send_resource(
            &client,
            settings,
            peer,
            object,
            TransferResource::Manifest,
            &cached_manifest,
            manifest_hash,
        )
        .await?;

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let cached_chunk = cached_resource_path(
                        &cache_root,
                        object,
                        TransferResource::VideoChunk,
                        chunk_path,
                    )?;
                    let chunk_hash = sha256_path(&cached_chunk).await?;
                    send_resource(
                        &client,
                        settings,
                        peer,
                        object,
                        TransferResource::VideoChunk,
                        &cached_chunk,
                        chunk_hash,
                    )
                    .await?;
                }
            }
            BlobKind::File => {
                let payload = object.payload_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node file {} is missing its generation payload",
                        object.logical_path
                    )
                })?;
                let cached_payload = cached_resource_path(
                    &cache_root,
                    object,
                    TransferResource::FilePayload,
                    payload,
                )?;
                send_resource(
                    &client,
                    settings,
                    peer,
                    object,
                    TransferResource::FilePayload,
                    &cached_payload,
                    object.content_sha256,
                )
                .await?;
            }
        }
    }

    let complete = Frame {
        kind: FrameKind::SyncComplete,
        session: peer.session,
        payload: local.encode(),
    };
    let response = post_authenticated_frame(
        &client,
        settings,
        peer,
        TRANSFER_PATH,
        complete,
        MAX_TRANSFER_RESPONSE_BYTES,
        false,
    )
    .await?;
    if response.kind != FrameKind::SyncComplete {
        anyhow::bail!("Cloud Node upstream returned an invalid recovery completion frame");
    }
    let remote = SyncPlanHeader::decode(&response.payload)?;
    if remote != local {
        anyhow::bail!(
            "Cloud Node upstream completed recovery with a different root: local={} remote={}",
            hex::encode(local.root_sha256),
            hex::encode(remote.root_sha256)
        );
    }

    // Only remove the durable spool after REMOTE proves the exact root
    // was committed. Network/protocol failures return earlier and keep it.
    cleanup_outbound_cache(store, &peer.node_id).await?;
    Ok(SyncNegotiation { local, remote })
}

pub(crate) async fn prepare_outbound_cache(
    store: &CloudNodeStore,
    peer_node_id: &str,
    plan: &SyncPlan,
) -> anyhow::Result<PathBuf> {
    let header = plan.header()?;
    let peer_root = outbound_peer_cache_root(store, peer_node_id);
    tokio::fs::create_dir_all(&peer_root).await?;
    let snapshot_name = hex::encode(header.root_sha256);
    prune_outbound_cache_snapshots(&peer_root, &snapshot_name).await?;
    let cache_root = peer_root.join(&snapshot_name);
    tokio::fs::create_dir_all(&cache_root).await?;

    for object in plan.ordered() {
        let manifest_hash = sha256_path(&object.manifest_path).await?;
        let manifest_target = cached_resource_path(
            &cache_root,
            object,
            TransferResource::Manifest,
            &object.manifest_path,
        )?;
        materialize_cached_resource(&object.manifest_path, &manifest_target, manifest_hash).await?;

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let chunk_hash = sha256_path(chunk_path).await?;
                    let chunk_target = cached_resource_path(
                        &cache_root,
                        object,
                        TransferResource::VideoChunk,
                        chunk_path,
                    )?;
                    materialize_cached_resource(chunk_path, &chunk_target, chunk_hash).await?;
                }
            }
            BlobKind::File => {
                let payload = object.payload_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Cloud Node file {} is missing its generation payload",
                        object.logical_path
                    )
                })?;
                let payload_target = cached_resource_path(
                    &cache_root,
                    object,
                    TransferResource::FilePayload,
                    payload,
                )?;
                materialize_cached_resource(payload, &payload_target, object.content_sha256)
                    .await?;
            }
        }
    }

    // Detect a LOCAL write racing the cache freeze. Partial cache bytes stay
    // durable, but they are never marked ready or uploaded as a mixed root.
    if store.sync_plan()?.header()? != header {
        anyhow::bail!(
            "Cloud Node local snapshot changed while preparing outbound cache; retry required"
        );
    }
    write_cache_ready_marker(&cache_root, header).await?;
    Ok(cache_root)
}

fn outbound_peer_cache_root(store: &CloudNodeStore, peer_node_id: &str) -> PathBuf {
    store
        .summary()
        .root
        .join(".cache")
        .join("outbound")
        .join(peer_node_id)
}

pub(crate) fn cached_resource_path(
    cache_root: &Path,
    object: &SyncObject,
    resource: TransferResource,
    source: &Path,
) -> anyhow::Result<PathBuf> {
    let object_root = cache_root.join(hex::encode(object.object_key));
    match resource {
        TransferResource::Manifest => Ok(object_root.join("manifest.blob.cn")),
        TransferResource::FilePayload => Ok(object_root.join("payload")),
        TransferResource::VideoChunk => {
            let name = source.file_name().ok_or_else(|| {
                anyhow::anyhow!(
                    "Cloud Node video chunk has no cacheable file name: {}",
                    source.display()
                )
            })?;
            Ok(object_root.join("chunks").join(name))
        }
    }
}

async fn materialize_cached_resource(
    source: &Path,
    target: &Path,
    expected_sha256: [u8; 32],
) -> anyhow::Result<()> {
    let source_size = tokio::fs::metadata(source).await?.len();
    if cached_file_matches(target, expected_sha256, source_size).await? {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if target.exists() {
        tokio::fs::remove_file(target).await?;
    }

    let part = cache_part_path(target)?;
    if part.exists() {
        tokio::fs::remove_file(&part).await?;
    }
    tokio::fs::copy(source, &part).await?;
    tokio::fs::File::open(&part).await?.sync_all().await?;
    if !cached_file_matches(&part, expected_sha256, source_size).await? {
        let _ = tokio::fs::remove_file(&part).await;
        anyhow::bail!(
            "Cloud Node outbound cache copy failed integrity verification: {}",
            source.display()
        );
    }
    tokio::fs::rename(&part, target).await?;
    Ok(())
}

async fn cached_file_matches(
    path: &Path,
    expected_sha256: [u8; 32],
    expected_size: u64,
) -> anyhow::Result<bool> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != expected_size {
        return Ok(false);
    }
    Ok(sha256_path(path).await? == expected_sha256)
}

fn cache_part_path(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node cache target has no file name: {}",
                path.display()
            )
        })?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{name}.part.{}", std::process::id())))
}

async fn write_cache_ready_marker(cache_root: &Path, header: SyncPlanHeader) -> anyhow::Result<()> {
    let target = cache_root.join(".ready");
    let part = cache_part_path(&target)?;
    if part.exists() {
        tokio::fs::remove_file(&part).await?;
    }
    tokio::fs::write(&part, header.encode()).await?;
    tokio::fs::File::open(&part).await?.sync_all().await?;
    if target.exists() {
        tokio::fs::remove_file(&target).await?;
    }
    tokio::fs::rename(part, target).await?;
    Ok(())
}

async fn prune_outbound_cache_snapshots(
    peer_root: &Path,
    keep_snapshot: &str,
) -> anyhow::Result<()> {
    let mut entries = tokio::fs::read_dir(peer_root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name().to_string_lossy() == keep_snapshot {
            continue;
        }
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

pub(crate) async fn cleanup_outbound_cache(
    store: &CloudNodeStore,
    peer_node_id: &str,
) -> anyhow::Result<()> {
    let root = outbound_peer_cache_root(store, peer_node_id);
    match tokio::fs::remove_dir_all(root).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn send_resource(
    client: &reqwest::Client,
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
    object: &SyncObject,
    resource: TransferResource,
    path: &Path,
    resource_sha256: [u8; 32],
) -> anyhow::Result<()> {
    let total_size = tokio::fs::metadata(path).await?.len();
    let mut file = tokio::fs::File::open(path).await?;
    let mut offset = 0u64;

    if total_size == 0 {
        let chunk = TransferChunk::new(
            object.kind,
            resource,
            object.object_key,
            object.content_sha256,
            resource_sha256,
            0,
            0,
            Vec::new(),
        )?;
        let next_offset = send_chunk(client, settings, peer, &chunk).await?;
        if next_offset != 0 {
            anyhow::bail!("Cloud Node empty resource acknowledgement has invalid offset");
        }
        return Ok(());
    }

    let mut buffer = vec![0u8; MAX_TRANSFER_DATA_BYTES];
    while offset < total_size {
        file.seek(SeekFrom::Start(offset)).await?;
        let remaining = total_size - offset;
        let wanted = usize::try_from(remaining.min(MAX_TRANSFER_DATA_BYTES as u64))
            .map_err(|_| anyhow::anyhow!("Cloud Node transfer size does not fit usize"))?;
        let read = file.read(&mut buffer[..wanted]).await?;
        if read == 0 {
            anyhow::bail!("Cloud Node transfer source ended before its declared size");
        }
        let data = buffer[..read].to_vec();
        let chunk = TransferChunk::new(
            object.kind,
            resource,
            object.object_key,
            object.content_sha256,
            resource_sha256,
            offset,
            total_size,
            data,
        )?;
        offset = send_chunk(client, settings, peer, &chunk).await?;
    }
    Ok(())
}

async fn send_chunk(
    client: &reqwest::Client,
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
    chunk: &TransferChunk,
) -> anyhow::Result<u64> {
    let response = post_authenticated_frame(
        client,
        settings,
        peer,
        TRANSFER_PATH,
        chunk.into_frame(peer.session)?,
        MAX_TRANSFER_RESPONSE_BYTES,
        true,
    )
    .await?;
    if response.kind != FrameKind::ObjectChunk {
        anyhow::bail!("Cloud Node upstream returned an invalid object-transfer acknowledgement");
    }
    decode_resume_ack(&response.payload, chunk)
}

fn decode_resume_ack(payload: &[u8], chunk: &TransferChunk) -> anyhow::Result<u64> {
    let chunk_end = chunk
        .offset
        .checked_add(u64::try_from(chunk.data.len())?)
        .ok_or_else(|| anyhow::anyhow!("Cloud Node transfer acknowledgement range overflow"))?;
    let next_offset = if payload.is_empty() {
        // Compatibility with a pre-resume server: it acknowledged only
        // the chunk that was just sent.
        chunk_end
    } else if payload.len() == 8 {
        u64::from_be_bytes(payload.try_into()?)
    } else {
        anyhow::bail!("Cloud Node upstream returned malformed resume acknowledgement");
    };
    if next_offset < chunk_end || next_offset > chunk.total_size {
        anyhow::bail!("Cloud Node upstream returned an invalid resume offset");
    }
    Ok(next_offset)
}

async fn post_authenticated_frame(
    client: &reqwest::Client,
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
    path: &str,
    frame: Frame,
    maximum_response_bytes: usize,
    request_resume_ack: bool,
) -> anyhow::Result<Frame> {
    ensure_expected_peer(settings, peer)?;
    if frame.session != peer.session {
        anyhow::bail!("Cloud Node authenticated request frame uses a different session");
    }
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node has no configured upstream"))?;
    let signing = load_signing_key_from_env()?;
    let (_, request_nonce) = random_session_and_nonce();
    let proof = NodeProof::session(
        &signing,
        &settings.node.id,
        now_ms()?,
        peer.session,
        request_nonce,
        peer.peer_nonce,
    )?;

    let endpoint = format!("{}{}", upstream.url.trim_end_matches('/'), path);
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(SESSION_PROOF_HEADER, hex::encode(proof.encode()?));
    if request_resume_ack {
        request = request.header(RESUME_ACK_HEADER, "1");
    }
    let response = request.body(frame.encode()?).send().await?;
    if response.status() != reqwest::StatusCode::OK {
        anyhow::bail!(
            "Cloud Node upstream rejected authenticated request with status {}",
            response.status()
        );
    }
    if response.content_length().unwrap_or(0) > maximum_response_bytes as u64 {
        anyhow::bail!("Cloud Node authenticated response is oversized");
    }
    let body = response.bytes().await?;
    if body.len() > maximum_response_bytes {
        anyhow::bail!("Cloud Node authenticated response is oversized");
    }
    let response = Frame::decode(&body)?;
    if response.session != peer.session {
        anyhow::bail!("Cloud Node upstream response uses a different authenticated session");
    }
    Ok(response)
}

fn ensure_expected_peer(
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
) -> anyhow::Result<()> {
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node has no configured upstream"))?;
    if peer.node_id != upstream.node_id {
        anyhow::bail!("Cloud Node authenticated peer no longer matches configured upstream");
    }
    Ok(())
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

async fn sha256_path(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rbe-cloud-node-client-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cache_test_settings(root: &Path) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": "local-test",
                "storageRoot": root,
                "backupVersions": 5,
                "preserveOriginal": true,
                "videoChunkBytes": 1048576
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn outbound_cache_freezes_snapshot_and_uses_part_commit() {
        let root = cache_test_root("outbound-cache");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("users.db");
        std::fs::write(&source, b"snapshot-one").unwrap();
        let store = CloudNodeStore::open(&cache_test_settings(&root)).unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        let plan = store.sync_plan().unwrap();
        let cache_root = prepare_outbound_cache(&store, "remote-test", &plan)
            .await
            .unwrap();
        assert!(cache_root.starts_with(store.summary().root.join(".cache/outbound")));
        assert!(cache_root.join(".ready").is_file());

        let object = plan.files.first().unwrap();
        let live_payload = object.payload_path.as_deref().unwrap();
        let cached_payload = cached_resource_path(
            &cache_root,
            object,
            TransferResource::FilePayload,
            live_payload,
        )
        .unwrap();
        assert_eq!(
            tokio::fs::read(&cached_payload).await.unwrap(),
            b"snapshot-one"
        );
        assert!(!cache_part_path(&cached_payload).unwrap().exists());

        // A later LOCAL revision must not mutate the frozen bytes that a
        // reconnect is supposed to resume sending.
        std::fs::write(&source, b"snapshot-two").unwrap();
        store.store_file(&source, "db/users.db").unwrap();
        assert_eq!(
            tokio::fs::read(&cached_payload).await.unwrap(),
            b"snapshot-one"
        );

        cleanup_outbound_cache(&store, "remote-test").await.unwrap();
        assert!(!outbound_peer_cache_root(&store, "remote-test").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_ack_accepts_old_server_and_forward_progress() {
        let chunk = TransferChunk::new(
            BlobKind::File,
            TransferResource::FilePayload,
            [1u8; 32],
            [2u8; 32],
            [2u8; 32],
            0,
            32,
            vec![7u8; 8],
        )
        .unwrap();
        assert_eq!(decode_resume_ack(&[], &chunk).unwrap(), 8);
        assert_eq!(decode_resume_ack(&24u64.to_be_bytes(), &chunk).unwrap(), 24);
        assert!(decode_resume_ack(&4u64.to_be_bytes(), &chunk).is_err());
        assert!(decode_resume_ack(&33u64.to_be_bytes(), &chunk).is_err());
    }
}
