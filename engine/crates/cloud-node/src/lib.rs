//! Low-level RBE Cloud Node primitives.
//!
//! Cloud Node sits below REL/RELC. Language programs never receive node keys,
//! tunnel identities, topology, storage roots, or direct access to this crate.

mod auth;
#[cfg(feature = "client")]
mod client;
mod config;
mod crypto;
mod format;
mod protocol;
#[cfg(feature = "client")]
mod provider;
mod recovery;
mod server;
mod store;
mod sync;
mod transfer;

pub use auth::{random_session_and_nonce, NodeProof, NodeProofKind, DEFAULT_AUTH_SKEW_MS};
#[cfg(feature = "client")]
pub use client::{
    negotiate_sync, probe_upstream, synchronize_upstream, AuthenticatedPeer, SyncNegotiation,
    KNOCK_PATH, SESSION_PROOF_HEADER, SYNC_PATH, TRANSFER_PATH,
};
#[cfg(not(feature = "client"))]
pub const KNOCK_PATH: &str = "/.rbe/cn/v1/knock";
#[cfg(not(feature = "client"))]
pub const SYNC_PATH: &str = "/.rbe/cn/v1/sync";
#[cfg(not(feature = "client"))]
pub const TRANSFER_PATH: &str = "/.rbe/cn/v1/transfer";
#[cfg(not(feature = "client"))]
pub const SESSION_PROOF_HEADER: &str = "x-rbe-cn-proof";
pub use config::{
    CloudNodeSettings, NodeMode, NodeSettings, ProviderConflictPolicy, ProviderKind,
    ProviderSettings, ReplicationSettings, ReplicationTarget, UpstreamSettings, SETTINGS_FILE_NAME,
};
pub use crypto::{
    load_signing_key_from_env, public_key_hex, sign_challenge, verify_challenge,
    CLOUD_NODE_PRIVATE_KEY_ENV,
};
pub use format::{
    BlobKind, BlobManifest, ByteRangeChange, ChunkRef, FolderEntry, BLOB_FORMAT_VERSION,
};
pub use protocol::{Frame, FrameKind, CN_PROTOCOL, MAX_FRAME_BYTES};
#[cfg(feature = "client")]
pub use provider::ProviderClient;
pub use recovery::{CloudNodeRecoveryReceiver, RecoveryReceipt};
pub use server::{
    AcceptedKnock, AuthenticatedSession, CloudNodeAuthenticator, DEFAULT_SESSION_TTL_MS,
    MAX_AUTH_PROOF_BYTES,
};
pub use store::{CloudNodeStore, StoreSummary, StoredObject};
pub use sync::{SyncObject, SyncPlan, SyncPlanHeader};
pub use transfer::{TransferChunk, TransferResource, MAX_TRANSFER_DATA_BYTES};
