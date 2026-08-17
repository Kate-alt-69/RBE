//! Trusted control-plane primitives for the container runtime.
//!
//! The Swamp runtime is the first testable scheduler slice: environments
//! own Swamps, Swamps own reusable Workers, executions have unique IDs,
//! and completion data feeds the artifact/profile cache.
//!
//! This is intentionally not yet the OS sandbox. `sandbox-primitives`,
//! `resource-limits`, `execution-engine`, and `ipc-protocol` remain the
//! layers where kernel isolation, enforced limits, real WASM execution,
//! and authenticated control IPC will land.

mod cache;
mod environment;
mod execution;
mod runtime;
mod swamp;
mod worker;

pub use cache::{ArtifactCache, ExecutionProfile};
pub use environment::EnvironmentSnapshot;
pub use execution::{ExecutionId, ExecutionRecord, ExecutionState, ExecutionTask, WorkCost};
pub use runtime::{Runtime, RuntimeConfig};
pub use swamp::SwampSnapshot;
pub use worker::{WorkerSnapshot, WorkerState};

pub use environments::{
    AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,
    EnvironmentRegistry, HealthStatus, PaymentEnvironment,
};
