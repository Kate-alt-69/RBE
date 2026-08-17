//! Trusted control-plane primitives for the container runtime.
//!
//! The Swamp runtime in `runtime` is the first testable scheduler slice:
//! environments own Swamps, Swamps own reusable Workers, executions have
//! stable IDs, and completion data feeds an artifact/profile cache.
//!
//! This is intentionally not yet the OS sandbox. `sandbox-primitives`,
//! `resource-limits`, `execution-engine`, and `ipc-protocol` remain the
//! layers where kernel isolation, enforced limits, real WASM execution,
//! and authenticated control IPC will land.

mod runtime;

pub use environments::{
    AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,
    EnvironmentRegistry, HealthStatus, PaymentEnvironment,
};

pub use runtime::{
    ArtifactCache, ExecutionId, ExecutionProfile, ExecutionTask, Runtime, RuntimeConfig,
    SwampSnapshot, WorkerSnapshot, WorkerState, WorkCost,
};
