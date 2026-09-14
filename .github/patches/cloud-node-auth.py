from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one Cloud Node auth anchor, found {count}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "engine/crates/cloud-node/Cargo.toml",
    '''ed25519-dalek = "2"
''',
    '''ed25519-dalek = "2"
rand = "0.8"
tokio = { workspace = true }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "http2"] }
''',
)

Path("engine/crates/cloud-node/src/auth.rs").write_text(r'''use std::io::{Cursor, Read};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};

const AUTH_MAGIC: &[u8; 8] = b"RBECNAU1";
const AUTH_VERSION: u16 = 1;
const AUTH_DOMAIN: &[u8] = b"RBE-CLOUD-NODE-AUTH/1\0";
const MAX_NODE_ID_BYTES: usize = 128;
pub const DEFAULT_AUTH_SKEW_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeProofKind {
    Knock = 1,
    Accept = 2,
}

impl TryFrom<u8> for NodeProofKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Knock,
            2 => Self::Accept,
            _ => anyhow::bail!("unknown Cloud Node proof kind {value}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeProof {
    pub kind: NodeProofKind,
    pub timestamp_ms: u64,
    pub session: [u8; 16],
    pub nonce: [u8; 32],
    pub peer_nonce: [u8; 32],
    pub node_id: String,
    pub signature: [u8; 64],
}

impl NodeProof {
    pub fn knock(
        signing: &SigningKey,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::signed(
            signing,
            NodeProofKind::Knock,
            node_id,
            timestamp_ms,
            session,
            nonce,
            [0u8; 32],
        )
    }

    pub fn accept(
        signing: &SigningKey,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
        peer_nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::signed(
            signing,
            NodeProofKind::Accept,
            node_id,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
        )
    }

    fn signed(
        signing: &SigningKey,
        kind: NodeProofKind,
        node_id: &str,
        timestamp_ms: u64,
        session: [u8; 16],
        nonce: [u8; 32],
        peer_nonce: [u8; 32],
    ) -> anyhow::Result<Self> {
        validate_node_id(node_id)?;
        let mut proof = Self {
            kind,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
            node_id: node_id.to_string(),
            signature: [0u8; 64],
        };
        proof.signature = signing.sign(&proof.signing_digest()?).to_bytes();
        Ok(proof)
    }

    pub fn verify_identity(&self, expected_node_id: &str, public_key_hex: &str) -> anyhow::Result<()> {
        if self.node_id != expected_node_id {
            anyhow::bail!("Cloud Node proof identity does not match configured peer");
        }
        validate_node_id(&self.node_id)?;
        let public = hex::decode(public_key_hex)
            .map_err(|_| anyhow::anyhow!("Cloud Node public key is not hexadecimal"))?;
        let public: [u8; 32] = public
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cloud Node public key must contain exactly 32 bytes"))?;
        let verifying = VerifyingKey::from_bytes(&public)
            .map_err(|_| anyhow::anyhow!("Cloud Node public key is invalid"))?;
        let signature = Signature::from_bytes(&self.signature);
        verifying
            .verify(&self.signing_digest()?, &signature)
            .map_err(|_| anyhow::anyhow!("Cloud Node proof signature verification failed"))
    }

    pub fn verify_freshness(&self, now_ms: u64, max_skew_ms: u64) -> anyhow::Result<()> {
        if self.timestamp_ms.abs_diff(now_ms) > max_skew_ms {
            anyhow::bail!("Cloud Node proof is outside the accepted clock window");
        }
        Ok(())
    }

    pub fn verify_accepts(&self, knock: &Self) -> anyhow::Result<()> {
        if self.kind != NodeProofKind::Accept || knock.kind != NodeProofKind::Knock {
            anyhow::bail!("Cloud Node proof kind is invalid for challenge response");
        }
        if self.session != knock.session || self.peer_nonce != knock.nonce {
            anyhow::bail!("Cloud Node accept proof is not bound to the initiating knock");
        }
        Ok(())
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let node = self.node_id.as_bytes();
        validate_node_id(&self.node_id)?;
        let node_len = u16::try_from(node.len())
            .map_err(|_| anyhow::anyhow!("Cloud Node id exceeds proof format"))?;
        let mut out = Vec::with_capacity(166 + node.len());
        out.extend_from_slice(AUTH_MAGIC);
        out.extend_from_slice(&AUTH_VERSION.to_be_bytes());
        out.push(self.kind as u8);
        out.push(0);
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        out.extend_from_slice(&self.session);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.peer_nonce);
        out.extend_from_slice(&node_len.to_be_bytes());
        out.extend_from_slice(node);
        out.extend_from_slice(&self.signature);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() < 166 || bytes.len() > 166 + MAX_NODE_ID_BYTES {
            anyhow::bail!("invalid Cloud Node proof length");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0u8; 8];
        cursor.read_exact(&mut magic)?;
        if &magic != AUTH_MAGIC {
            anyhow::bail!("invalid Cloud Node proof magic");
        }
        let version = read_u16(&mut cursor)?;
        if version != AUTH_VERSION {
            anyhow::bail!("unsupported Cloud Node proof version {version}");
        }
        let kind = NodeProofKind::try_from(read_u8(&mut cursor)?)?;
        if read_u8(&mut cursor)? != 0 {
            anyhow::bail!("Cloud Node proof reserved bits are non-zero");
        }
        let timestamp_ms = read_u64(&mut cursor)?;
        let session = read_array::<16>(&mut cursor)?;
        let nonce = read_array::<32>(&mut cursor)?;
        let peer_nonce = read_array::<32>(&mut cursor)?;
        let node_len = read_u16(&mut cursor)? as usize;
        if node_len == 0 || node_len > MAX_NODE_ID_BYTES {
            anyhow::bail!("invalid Cloud Node proof node id length");
        }
        let expected = 166usize.saturating_add(node_len);
        if bytes.len() != expected {
            anyhow::bail!("Cloud Node proof length does not match node id length");
        }
        let mut node = vec![0u8; node_len];
        cursor.read_exact(&mut node)?;
        let node_id = String::from_utf8(node)
            .map_err(|_| anyhow::anyhow!("Cloud Node proof node id is not UTF-8"))?;
        validate_node_id(&node_id)?;
        let signature = read_array::<64>(&mut cursor)?;
        Ok(Self {
            kind,
            timestamp_ms,
            session,
            nonce,
            peer_nonce,
            node_id,
            signature,
        })
    }

    fn signing_digest(&self) -> anyhow::Result<[u8; 32]> {
        validate_node_id(&self.node_id)?;
        let mut digest = Sha256::new();
        digest.update(AUTH_DOMAIN);
        digest.update([self.kind as u8]);
        digest.update(self.timestamp_ms.to_be_bytes());
        digest.update(self.session);
        digest.update(self.nonce);
        digest.update(self.peer_nonce);
        digest.update((self.node_id.len() as u32).to_be_bytes());
        digest.update(self.node_id.as_bytes());
        Ok(digest.finalize().into())
    }
}

pub fn random_session_and_nonce() -> ([u8; 16], [u8; 32]) {
    let mut session = [0u8; 16];
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut session);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    (session, nonce)
}

fn validate_node_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > MAX_NODE_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("Cloud Node id must use 1..=128 ASCII [A-Za-z0-9_.-] characters");
    }
    Ok(())
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u8> {
    Ok(read_array::<1>(cursor)?[0])
}

fn read_u16(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u16> {
    Ok(u16::from_be_bytes(read_array::<2>(cursor)?))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<u64> {
    Ok(u64::from_be_bytes(read_array::<8>(cursor)?))
}

fn read_array<const N: usize>(cursor: &mut Cursor<&[u8]>) -> anyhow::Result<[u8; N]> {
    let mut value = [0u8; N];
    cursor.read_exact(&mut value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutual_proofs_bind_identity_session_nonce_and_time() {
        let client = SigningKey::from_bytes(&[7u8; 32]);
        let server = SigningKey::from_bytes(&[9u8; 32]);
        let (session, client_nonce) = random_session_and_nonce();
        let (_, server_nonce) = random_session_and_nonce();
        let knock = NodeProof::knock(&client, "nas-main", 50_000, session, client_nonce).unwrap();
        let encoded = knock.encode().unwrap();
        let knock = NodeProof::decode(&encoded).unwrap();
        knock
            .verify_identity("nas-main", &hex::encode(client.verifying_key().to_bytes()))
            .unwrap();
        knock.verify_freshness(50_500, DEFAULT_AUTH_SKEW_MS).unwrap();

        let accept = NodeProof::accept(
            &server,
            "render-main",
            50_600,
            session,
            server_nonce,
            client_nonce,
        )
        .unwrap();
        let accept = NodeProof::decode(&accept.encode().unwrap()).unwrap();
        accept.verify_accepts(&knock).unwrap();
        accept
            .verify_identity(
                "render-main",
                &hex::encode(server.verifying_key().to_bytes()),
            )
            .unwrap();
        assert!(accept.verify_identity("attacker", &hex::encode(server.verifying_key().to_bytes())).is_err());
    }
}
''')

Path("engine/crates/cloud-node/src/client.rs").write_text(r'''use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
''')

replace_once(
    "engine/crates/cloud-node/src/config.rs",
    '''pub struct UpstreamSettings {
    pub url: String,
    pub public_key: String,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_true")]
    pub sync_on_connect: bool,
}
''',
    '''pub struct UpstreamSettings {
    pub url: String,
    pub node_id: String,
    pub public_key: String,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_true")]
    pub sync_on_connect: bool,
    #[serde(default = "default_reconnect_delay_ms")]
    pub reconnect_delay_ms: u64,
}
''',
)
replace_once(
    "engine/crates/cloud-node/src/config.rs",
    '''        if let Some(upstream) = &self.upstream {
            validate_peer_url(&upstream.url)?;
            validate_public_key(&upstream.public_key)?;
        }
''',
    '''        if let Some(upstream) = &self.upstream {
            validate_peer_url(&upstream.url)?;
            validate_node_id(&upstream.node_id)?;
            validate_public_key(&upstream.public_key)?;
            if upstream.reconnect_delay_ms < 250 || upstream.reconnect_delay_ms > 300_000 {
                anyhow::bail!("Cloud Node reconnectDelayMs must be between 250 and 300000");
            }
        }
''',
)
replace_once(
    "engine/crates/cloud-node/src/config.rs",
    '''const fn default_video_chunk_bytes() -> usize {
    4 * 1024 * 1024
}
''',
    '''const fn default_video_chunk_bytes() -> usize {
    4 * 1024 * 1024
}
const fn default_reconnect_delay_ms() -> u64 {
    2_000
}
''',
)

replace_once(
    "engine/crates/cloud-node/src/lib.rs",
    '''mod config;
mod crypto;
mod format;
mod protocol;
mod store;
mod sync;
''',
    '''mod auth;
mod client;
mod config;
mod crypto;
mod format;
mod protocol;
mod store;
mod sync;
''',
)
replace_once(
    "engine/crates/cloud-node/src/lib.rs",
    '''pub use config::{
''',
    '''pub use auth::{random_session_and_nonce, NodeProof, NodeProofKind, DEFAULT_AUTH_SKEW_MS};
pub use client::{probe_upstream, AuthenticatedPeer, KNOCK_PATH};
pub use config::{
''',
)

Path("engine/crates/cloud-node/src/main.rs").write_text(r'''use std::path::{Path, PathBuf};
use std::time::Duration;

use cloud_node::{
    load_signing_key_from_env, probe_upstream, public_key_hex, CloudNodeSettings, CloudNodeStore,
    SETTINGS_FILE_NAME,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("cloud_node: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let config_path = take_config_arg(&mut args)?.unwrap_or_else(default_config_path);
    let command = args.first().map(String::as_str).unwrap_or("evaluate");

    if matches!(command, "--help" | "-h") {
        print_help();
        return Ok(());
    }
    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;
    let store = CloudNodeStore::open(&settings)?;
    match command {
        "evaluate" => {
            let summary = store.summary();
            let plan = store.sync_plan()?;
            println!("Cloud Node {} ready", settings.node.id);
            println!("storage={}", summary.storage.display());
            println!("backup={}", summary.backup.display());
            println!("syncRoot={}", plan.root_hex());
            println!("syncObjects={}", plan.object_count());
            if let Some(upstream) = &settings.upstream {
                println!("upstream={}", upstream.url);
                println!("upstreamNode={}", upstream.node_id);
            }
        }
        "probe-upstream" => {
            let peer = probe_upstream(&settings).await?;
            println!("authenticated={}", peer.node_id);
            println!("session={}", hex::encode(peer.session));
        }
        "run" => run_daemon(&settings).await?,
        "sync-plan" => {
            let plan = store.sync_plan()?;
            println!("root={}", plan.root_hex());
            println!("folders={}", plan.folders.len());
            println!("videos={}", plan.videos.len());
            println!("files={}", plan.files.len());
            for object in plan.ordered() {
                println!(
                    "{:?}\t{}\t{}\t{}",
                    object.kind,
                    hex::encode(object.object_key),
                    hex::encode(object.content_sha256),
                    object.logical_path
                );
            }
        }
        "store-file" | "store-video" | "snapshot-folder" => {
            if args.len() != 3 {
                anyhow::bail!("{command} requires <source> <logical-path>");
            }
            let source = Path::new(&args[1]);
            let logical = &args[2];
            let stored = match command {
                "store-file" => store.store_file(source, logical)?,
                "store-video" => store.store_video(source, logical)?,
                "snapshot-folder" => store.snapshot_folder(source, logical)?,
                _ => unreachable!(),
            };
            println!("object={}", stored.object_key);
            println!("content={}", stored.content_sha256);
            println!("manifest={}", stored.manifest.display());
        }
        "verify" => {
            println!("verified={}", store.verify()?);
        }
        other => anyhow::bail!("unknown cloud_node command {other:?}; use --help"),
    }
    Ok(())
}

async fn run_daemon(settings: &CloudNodeSettings) -> anyhow::Result<()> {
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream"))?;
    loop {
        match probe_upstream(settings).await {
            Ok(peer) => {
                println!(
                    "Cloud Node authenticated upstream {} session={}",
                    peer.node_id,
                    hex::encode(peer.session)
                );
                if !upstream.auto_reconnect {
                    return Ok(());
                }
            }
            Err(error) => {
                eprintln!("Cloud Node upstream unavailable: {error}");
                if !upstream.auto_reconnect {
                    return Err(error);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(upstream.reconnect_delay_ms)).await;
    }
}

fn take_config_arg(args: &mut Vec<String>) -> anyhow::Result<Option<PathBuf>> {
    let mut found = None;
    let mut index = 0usize;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--config=") {
            if value.is_empty() || found.is_some() {
                anyhow::bail!("--config must be supplied at most once with a non-empty path");
            }
            found = Some(PathBuf::from(value));
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(found)
}

fn default_config_path() -> PathBuf {
    std::env::var_os("RBE_CN_SETTINGS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.join(SETTINGS_FILE_NAME)))
        })
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE_NAME))
}

fn print_help() {
    println!(
        "cloud_node [--config=<setting.node.cn.json>] [evaluate|probe-upstream|run|sync-plan|verify|public-key|store-file <source> <logical>|store-video <source> <logical>|snapshot-folder <source> <logical>]"
    );
}
''')

replace_once(
    "doc/cloud-node.md",
    '''  "upstream": {
    "url": "https://example-backend.invalid",
    "publicKey": "<64 hex Ed25519 public key>",
    "autoReconnect": true,
    "syncOnConnect": true
  },
''',
    '''  "upstream": {
    "url": "https://example-backend.invalid",
    "nodeId": "render-main",
    "publicKey": "<64 hex Ed25519 public key>",
    "autoReconnect": true,
    "syncOnConnect": true,
    "reconnectDelayMs": 2000
  },
''',
)
replace_once(
    "doc/cloud-node.md",
    '''The binary `RBE-CN/1` frame envelope already reserves distinct message types for Hello, challenge/response, sync negotiation, folder manifests, object requests/chunks, completion, and ping/pong.
''',
    '''The binary `RBE-CN/1` frame envelope already reserves distinct message types for Hello, challenge/response, sync negotiation, folder manifests, object requests/chunks, completion, and ping/pong.

Cloud Node authentication additionally has a compact binary `RBECNAU1` proof. A node signs its node id, timestamp, fresh session id, and nonce. The accepting RBE node returns a separately signed proof bound to that exact session and client nonce. `cloud_node probe-upstream` performs one mutual-authentication probe; `cloud_node run` retries the probe according to `reconnectDelayMs`. Invalid peers are intentionally expected to receive a generic not-found response once the backend-side knock endpoint is enabled.
''',
)
