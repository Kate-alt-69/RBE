use std::collections::HashMap;
use std::sync::Mutex;

use ed25519_dalek::SigningKey;

use crate::auth::{random_session_and_nonce, NodeProof, NodeProofKind, DEFAULT_AUTH_SKEW_MS};
use crate::config::CloudNodeSettings;
use crate::crypto::load_signing_key_from_env;

pub const MAX_AUTH_PROOF_BYTES: usize = 1024;
pub const DEFAULT_SESSION_TTL_MS: u64 = 5 * 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub node_id: String,
    pub session: [u8; 16],
    pub peer_nonce: [u8; 32],
    pub local_nonce: [u8; 32],
    pub authenticated_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedKnock {
    pub response: Vec<u8>,
    pub session: AuthenticatedSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReplayKey {
    purpose: u8,
    node_id: String,
    session: [u8; 16],
    nonce: [u8; 32],
}

#[derive(Default)]
struct AuthState {
    replays: HashMap<ReplayKey, u64>,
    sessions: HashMap<[u8; 16], AuthenticatedSession>,
}

pub struct CloudNodeAuthenticator {
    node_id: String,
    signing: SigningKey,
    trusted_peers: HashMap<String, String>,
    max_skew_ms: u64,
    session_ttl_ms: u64,
    state: Mutex<AuthState>,
}

impl CloudNodeAuthenticator {
    pub fn from_env(settings: &CloudNodeSettings) -> anyhow::Result<Self> {
        Self::new(settings, load_signing_key_from_env()?)
    }

    pub fn new(settings: &CloudNodeSettings, signing: SigningKey) -> anyhow::Result<Self> {
        settings.validate()?;
        let trusted_peers = settings
            .replication
            .targets
            .iter()
            .map(|target| (target.node_id.clone(), target.public_key.clone()))
            .collect::<HashMap<_, _>>();
        Ok(Self {
            node_id: settings.node.id.clone(),
            signing,
            trusted_peers,
            max_skew_ms: DEFAULT_AUTH_SKEW_MS,
            session_ttl_ms: DEFAULT_SESSION_TTL_MS,
            state: Mutex::new(AuthState::default()),
        })
    }

    pub fn trusted_peer_count(&self) -> usize {
        self.trusted_peers.len()
    }

    pub fn accept_knock(&self, encoded: &[u8], now_ms: u64) -> anyhow::Result<AcceptedKnock> {
        if encoded.is_empty() || encoded.len() > MAX_AUTH_PROOF_BYTES {
            anyhow::bail!("Cloud Node authentication proof has invalid length");
        }
        let knock = NodeProof::decode(encoded)?;
        if knock.kind != NodeProofKind::Knock {
            anyhow::bail!("Cloud Node authentication request is not a knock proof");
        }
        knock.verify_freshness(now_ms, self.max_skew_ms)?;
        self.verify_trusted_identity(&knock)?;

        let replay_key = ReplayKey {
            purpose: NodeProofKind::Knock as u8,
            node_id: knock.node_id.clone(),
            session: knock.session,
            nonce: knock.nonce,
        };
        let mut state = self.lock_state()?;
        prune_state(&mut state, now_ms);
        if state.replays.contains_key(&replay_key) {
            anyhow::bail!("Cloud Node authentication replay rejected");
        }

        let (_, local_nonce) = random_session_and_nonce();
        let accept = NodeProof::accept(
            &self.signing,
            &self.node_id,
            now_ms,
            knock.session,
            local_nonce,
            knock.nonce,
        )?;
        let session = AuthenticatedSession {
            node_id: knock.node_id.clone(),
            session: knock.session,
            peer_nonce: knock.nonce,
            local_nonce,
            authenticated_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(self.session_ttl_ms),
        };
        state.replays.insert(
            replay_key,
            knock.timestamp_ms.saturating_add(self.max_skew_ms),
        );
        state.sessions.insert(knock.session, session.clone());
        Ok(AcceptedKnock {
            response: accept.encode()?,
            session,
        })
    }

    /// Verifies a signed follow-up proof and returns the session it belongs to.
    /// Every request uses a fresh proof nonce, so captured request proofs cannot
    /// be replayed even while the five-minute session itself remains valid.
    pub fn authorize_session_proof(
        &self,
        encoded: &[u8],
        now_ms: u64,
    ) -> anyhow::Result<AuthenticatedSession> {
        if encoded.is_empty() || encoded.len() > MAX_AUTH_PROOF_BYTES {
            anyhow::bail!("Cloud Node session proof has invalid length");
        }
        let proof = NodeProof::decode(encoded)?;
        if proof.kind != NodeProofKind::Session {
            anyhow::bail!("Cloud Node follow-up proof is not a session proof");
        }
        proof.verify_freshness(now_ms, self.max_skew_ms)?;
        self.verify_trusted_identity(&proof)?;

        let replay_key = ReplayKey {
            purpose: NodeProofKind::Session as u8,
            node_id: proof.node_id.clone(),
            session: proof.session,
            nonce: proof.nonce,
        };
        let mut state = self.lock_state()?;
        prune_state(&mut state, now_ms);
        if state.replays.contains_key(&replay_key) {
            anyhow::bail!("Cloud Node session proof replay rejected");
        }
        let session = state
            .sessions
            .get(&proof.session)
            .filter(|active| active.node_id == proof.node_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Cloud Node authenticated session is unavailable"))?;
        if proof.peer_nonce != session.local_nonce {
            anyhow::bail!("Cloud Node session proof is not bound to this authenticated peer");
        }
        state.replays.insert(
            replay_key,
            proof.timestamp_ms.saturating_add(self.max_skew_ms),
        );
        Ok(session)
    }

    pub fn session(
        &self,
        node_id: &str,
        session: &[u8; 16],
        now_ms: u64,
    ) -> anyhow::Result<Option<AuthenticatedSession>> {
        let mut state = self.lock_state()?;
        prune_state(&mut state, now_ms);
        Ok(state
            .sessions
            .get(session)
            .filter(|active| active.node_id == node_id)
            .cloned())
    }

    fn verify_trusted_identity(&self, proof: &NodeProof) -> anyhow::Result<()> {
        let public_key = self
            .trusted_peers
            .get(&proof.node_id)
            .ok_or_else(|| anyhow::anyhow!("Cloud Node peer is not trusted"))?;
        proof.verify_identity(&proof.node_id, public_key)
    }

    fn lock_state(&self) -> anyhow::Result<std::sync::MutexGuard<'_, AuthState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("Cloud Node authentication state lock was poisoned"))
    }
}

fn prune_state(state: &mut AuthState, now_ms: u64) {
    state.replays.retain(|_, expires_at| *expires_at >= now_ms);
    state
        .sessions
        .retain(|_, session| session.expires_at_ms >= now_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::public_key_hex;

    fn settings(client: &SigningKey) -> CloudNodeSettings {
        serde_json::from_value(serde_json::json!({
            "node": {
                "id": "render-main",
                "storageRoot": "/tmp/rbe-cn-auth"
            },
            "replication": {
                "targets": [{
                    "nodeId": "nas-main",
                    "url": "https://nas.invalid",
                    "publicKey": public_key_hex(client),
                    "durable": true
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn trusted_knock_creates_bound_session_and_replay_is_rejected() {
        let server = SigningKey::from_bytes(&[9u8; 32]);
        let client = SigningKey::from_bytes(&[7u8; 32]);
        let settings = settings(&client);
        let auth = CloudNodeAuthenticator::new(&settings, server.clone()).unwrap();
        let session = [3u8; 16];
        let peer_nonce = [5u8; 32];
        let knock = NodeProof::knock(&client, "nas-main", 50_000, session, peer_nonce).unwrap();
        let encoded = knock.encode().unwrap();

        let accepted = auth.accept_knock(&encoded, 50_100).unwrap();
        let proof = NodeProof::decode(&accepted.response).unwrap();
        proof.verify_accepts(&knock).unwrap();
        proof
            .verify_identity("render-main", &public_key_hex(&server))
            .unwrap();
        assert_eq!(accepted.session.peer_nonce, peer_nonce);
        assert_eq!(
            auth.session("nas-main", &session, 50_200)
                .unwrap()
                .unwrap()
                .session,
            session
        );
        assert!(auth.accept_knock(&encoded, 50_300).is_err());
    }

    #[test]
    fn session_proof_is_nonce_bound_signed_and_replay_safe() {
        let server = SigningKey::from_bytes(&[9u8; 32]);
        let client = SigningKey::from_bytes(&[7u8; 32]);
        let settings = settings(&client);
        let auth = CloudNodeAuthenticator::new(&settings, server).unwrap();
        let session = [3u8; 16];
        let knock = NodeProof::knock(&client, "nas-main", 50_000, session, [5u8; 32]).unwrap();
        let accepted = auth.accept_knock(&knock.encode().unwrap(), 50_100).unwrap();

        let follow_up = NodeProof::session(
            &client,
            "nas-main",
            50_200,
            session,
            [7u8; 32],
            accepted.session.local_nonce,
        )
        .unwrap()
        .encode()
        .unwrap();
        let authorized = auth.authorize_session_proof(&follow_up, 50_300).unwrap();
        assert_eq!(authorized.session, session);
        assert_eq!(authorized.node_id, "nas-main");
        assert!(auth.authorize_session_proof(&follow_up, 50_400).is_err());

        let wrong_binding =
            NodeProof::session(&client, "nas-main", 50_500, session, [8u8; 32], [99u8; 32])
                .unwrap()
                .encode()
                .unwrap();
        assert!(auth
            .authorize_session_proof(&wrong_binding, 50_600)
            .is_err());
    }

    #[test]
    fn unknown_or_stale_knock_is_rejected_without_session() {
        let server = SigningKey::from_bytes(&[9u8; 32]);
        let client = SigningKey::from_bytes(&[7u8; 32]);
        let attacker = SigningKey::from_bytes(&[11u8; 32]);
        let settings = settings(&client);
        let auth = CloudNodeAuthenticator::new(&settings, server).unwrap();

        let unknown = NodeProof::knock(&attacker, "attacker", 50_000, [1u8; 16], [2u8; 32])
            .unwrap()
            .encode()
            .unwrap();
        assert!(auth.accept_knock(&unknown, 50_100).is_err());

        let stale = NodeProof::knock(&client, "nas-main", 1, [4u8; 16], [6u8; 32])
            .unwrap()
            .encode()
            .unwrap();
        assert!(auth.accept_knock(&stale, 100_000).is_err());
        assert!(auth
            .session("nas-main", &[4u8; 16], 100_000)
            .unwrap()
            .is_none());
    }
}
