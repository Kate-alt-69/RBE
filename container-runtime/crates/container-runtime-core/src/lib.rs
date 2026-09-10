//! Trusted control-plane primitives for the container runtime.
//!
//! The Swamp runtime is the first testable scheduler slice: environments
//! own Swamps, Swamps own reusable Workers, executions have unique IDs,
//! and completion data feeds the artifact/profile cache.
//!
//! `storage` is the durable Environment state boundary. It intentionally
//! separates volatile/staging data from atomically committed namespace
//! generations so a crash cannot expose a half-written multi-file update.

mod cache;
mod environment;
mod execution;
mod runtime;
mod storage;
mod swamp;
mod worker;

pub use cache::{ArtifactCache, ExecutionProfile};
pub use environment::EnvironmentSnapshot;
pub use execution::{
    ExecutionId, ExecutionOutcome, ExecutionRecord, ExecutionState, ExecutionTask, WorkCost,
};
pub use runtime::{Runtime, RuntimeConfig};
pub use storage::{EnvironmentStorageManager, StorageCommit, StorageSnapshot, StorageTransaction};
pub use swamp::SwampSnapshot;
pub use worker::{WorkerSnapshot, WorkerState};

pub use environments::{
    AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,
    EnvironmentRegistry, HealthStatus, PaymentEnvironment,
};
