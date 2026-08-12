//! Phase 2 stub for the layered execution chain (sandbox-primitives ->
//! resource-limits -> execution-engine -> ipc-protocol ->
//! container-runtime-core -> container-bin) described in
//! rust-migration-plan.md §5.1 — that part is still not built.
//!
//! What IS real: this crate re-exports `environments` so `container-bin`
//! (and eventually `execution-engine`, once it exists) has one place
//! to get at the six-environment registry — health monitoring, abuse
//! detection, and the payment environment's encryption boundary. See
//! `environments`'s crate doc comment for exactly what it does and
//! doesn't cover.

pub use environments::{
    AbuseDimension, AbuseVerdict, EncryptedPayload, EnvironmentId, EnvironmentKind,
    EnvironmentRegistry, HealthStatus, PaymentEnvironment,
};
