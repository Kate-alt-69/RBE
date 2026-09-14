use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::{random_session_and_nonce, NodeProof, DEFAULT_AUTH_SKEW_MS};
use crate::config::CloudNodeSettings;
use crate::crypto::load_signing_key_from_env;

pub const KNOCK_PATH: &str = "/.rbe/cn/v1/knock";
const MAX_PROOF_RESPONSE_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    pub node_id: String,
    pub session: [u8; 16],
    pub peer_nonce: [u8; 32],
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

fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}
