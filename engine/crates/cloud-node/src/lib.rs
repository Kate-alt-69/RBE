//! Low-level RBE Cloud Node primitives.
//!
//! Cloud Node sits below REL/RELC. Language programs never receive node keys,
//! tunnel identities, topology, storage roots, or direct access to this crate.

mod auth;
mod client;
mod config;
mod crypto;
mod format;
mod protocol;
mod store;
mod sync;

pub use auth::{random_session_and_nonce, NodeProof, NodeProofKind, DEFAULT_AUTH_SKEW_MS};
pub use client::{probe_upstream, AuthenticatedPeer, KNOCK_PATH};
pub use config::{
    CloudNodeSettings, NodeMode, NodeSettings, ReplicationSettings, ReplicationTarget,
    UpstreamSettings, SETTINGS_FILE_NAME,
};
pub use crypto::{
    load_signing_key_from_env, public_key_hex, sign_challenge, verify_challenge,
    CLOUD_NODE_PRIVATE_KEY_ENV,
};
pub use format::{
    BlobKind, BlobManifest, ByteRangeChange, ChunkRef, FolderEntry, BLOB_FORMAT_VERSION,
};
pub use protocol::{Frame, FrameKind, CN_PROTOCOL, MAX_FRAME_BYTES};
pub use store::{CloudNodeStore, StoreSummary, StoredObject};
pub use sync::{SyncObject, SyncPlan, SyncPlanHeader};
