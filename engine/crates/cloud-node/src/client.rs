use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::{random_session_and_nonce, NodeProof, DEFAULT_AUTH_SKEW_MS};
use crate::config::CloudNodeSettings;
use crate::crypto::load_signing_key_from_env;
use crate::protocol::{Frame, FrameKind};
use crate::store::CloudNodeStore;
use crate::sync::SyncPlanHeader;

pub const KNOCK_PATH: &str = "/.rbe/cn/v1/knock";
pub const SYNC_PATH: &str = "/.rbe/cn/v1/sync";
pub const SESSION_PROOF_HEADER: &str = "x-rbe-cn-proof";
const MAX_PROOF_RESPONSE_BYTES: usize = 1024;
const MAX_SYNC_RESPONSE_BYTES: usize = 4096;

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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
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
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node has no configured upstream"))?;
    if peer.node_id != upstream.node_id {
        anyhow::bail!("Cloud Node authenticated peer no longer matches configured upstream");
    }

    let local = store.sync_plan()?.header()?;
    let request = Frame {
        kind: FrameKind::SyncHello,
        session: peer.session,
        payload: local.encode(),
    };
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

    let endpoint = format!("{}{}", upstream.url.trim_end_matches('/'), SYNC_PATH);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(SESSION_PROOF_HEADER, hex::encode(proof.encode()?))
        .body(request.encode()?)
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        anyhow::bail!("Cloud Node upstream did not admit sync negotiation");
    }
    if response.content_length().unwrap_or(0) > MAX_SYNC_RESPONSE_BYTES as u64 {
        anyhow::bail!("Cloud Node sync negotiation response is oversized");
    }
    let body = response.bytes().await?;
    if body.len() > MAX_SYNC_RESPONSE_BYTES {
        anyhow::bail!("Cloud Node sync negotiation response is oversized");
    }
    let response = Frame::decode(&body)?;
    if response.kind != FrameKind::SyncHello || response.session != peer.session {
        anyhow::bail!("Cloud Node upstream returned an invalid sync negotiation frame");
    }
    let remote = SyncPlanHeader::decode(&response.payload)?;
    Ok(SyncNegotiation { local, remote })
}

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}
