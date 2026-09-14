use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::auth::{random_session_and_nonce, NodeProof, DEFAULT_AUTH_SKEW_MS};
use crate::config::CloudNodeSettings;
use crate::crypto::load_signing_key_from_env;
use crate::format::BlobKind;
use crate::protocol::{Frame, FrameKind};
use crate::store::CloudNodeStore;
use crate::sync::{SyncObject, SyncPlanHeader};
use crate::transfer::{TransferChunk, TransferResource, MAX_TRANSFER_DATA_BYTES};

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
        return Ok(negotiation);
    }

    let plan = store.sync_plan()?;
    let local = plan.header()?;
    if local != negotiation.local {
        anyhow::bail!("Cloud Node local snapshot changed after sync negotiation; retry required");
    }

    let client = http_client()?;
    for object in plan.ordered() {
        let manifest_hash = sha256_path(&object.manifest_path).await?;
        send_resource(
            &client,
            settings,
            peer,
            object,
            TransferResource::Manifest,
            &object.manifest_path,
            manifest_hash,
        )
        .await?;

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let chunk_hash = sha256_path(chunk_path).await?;
                    send_resource(
                        &client,
                        settings,
                        peer,
                        object,
                        TransferResource::VideoChunk,
                        chunk_path,
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
                send_resource(
                    &client,
                    settings,
                    peer,
                    object,
                    TransferResource::FilePayload,
                    payload,
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
    Ok(SyncNegotiation { local, remote })
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
        send_chunk(client, settings, peer, &chunk).await?;
        return Ok(());
    }

    let mut buffer = vec![0u8; MAX_TRANSFER_DATA_BYTES];
    while offset < total_size {
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
        send_chunk(client, settings, peer, &chunk).await?;
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("Cloud Node transfer offset overflow"))?;
    }
    Ok(())
}

async fn send_chunk(
    client: &reqwest::Client,
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
    chunk: &TransferChunk,
) -> anyhow::Result<()> {
    let response = post_authenticated_frame(
        client,
        settings,
        peer,
        TRANSFER_PATH,
        chunk.into_frame(peer.session)?,
        MAX_TRANSFER_RESPONSE_BYTES,
    )
    .await?;
    if response.kind != FrameKind::ObjectChunk || !response.payload.is_empty() {
        anyhow::bail!("Cloud Node upstream returned an invalid object-transfer acknowledgement");
    }
    Ok(())
}

async fn post_authenticated_frame(
    client: &reqwest::Client,
    settings: &CloudNodeSettings,
    peer: &AuthenticatedPeer,
    path: &str,
    frame: Frame,
    maximum_response_bytes: usize,
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
    let response = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(SESSION_PROOF_HEADER, hex::encode(proof.encode()?))
        .body(frame.encode()?)
        .send()
        .await?;
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
