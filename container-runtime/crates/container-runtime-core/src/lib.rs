//! Trusted control-plane primitives for the container runtime.
//!
//! The Swamp runtime is the first testable scheduler slice: environments
//! own Swamps, Swamps own reusable Workers, executions have unique IDs,
//! and completion data feeds the artifact/profile cache.
//!
//! `storage` is the durable Environment state boundary. It intentionally
//! separates volatile/staging data from atomically committed namespace
//! generations so a crash cannot expose a half-written multi-file update.
//! `control_plane` is the deny-by-default authority table used by the
//! Container Controller before sandbox-originated work may touch host-owned
//! RBE capabilities.

mod cache;
mod control_plane;
mod environment;
mod execution;
mod runtime;
mod storage;
mod swamp;
mod worker;

pub use cache::{ArtifactCache, ExecutionProfile};
pub use control_plane::{AuthorizedCapability, CapabilityBroker, CapabilityCall, CapabilityError};
pub use environment::EnvironmentSnapshot;
pub use execution::{
    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionRecord, ExecutionState,
    ExecutionTask, WorkCost,
};
pub use runtime::{Runtime, RuntimeConfig, DEFAULT_ENVIRONMENT_STORAGE_BYTES};
pub use storage::{EnvironmentStorageManager, StorageCommit, StorageSnapshot, StorageTransaction};
pub use swamp::SwampSnapshot;
pub use worker::{Canceller, Runner, WorkerSnapshot, WorkerState};

pub use environments::{
    AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,
    EnvironmentRegistry, HealthStatus, PaymentEnvironment,
};
