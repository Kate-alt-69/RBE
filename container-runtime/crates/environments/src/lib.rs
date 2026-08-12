//! Six sandboxed execution environments for the container runtime:
//! five general-purpose ([`id::EnvironmentId::GENERAL`]) plus one
//! dedicated, fully-encrypted payment environment
//! ([`id::EnvironmentId::Payment`], see [`payment`]).
//!
//! **Scope of this crate, stated plainly:** this is the
//! monitoring/policy layer — which environment is healthy, whether a
//! caller's reported resource usage crosses an abuse threshold, and
//! (for payment) the encryption boundary around processing. It is
//! **not** the OS-level sandboxing itself (namespaces, cgroups,
//! actually spawning isolated processes) — that's `sandbox-primitives`
//! and `execution-engine`, still stubbed elsewhere in this workspace,
//! not touched by this change. Once those exist, they're the natural
//! caller of [`registry::EnvironmentRegistry::record_general_execution`]
//! /`record_payment_execution` with real measured usage, and the thing
//! that would actually enforce a `Blocked` verdict by refusing to run
//! (or killing) the offending execution — this crate decides, it
//! doesn't yet enforce at the OS level.
//!
//! All disk writes here (the payment environment's audit log) go
//! through the shared `atomic-io` crate — see that crate's doc comment
//! for what "atomic" does and doesn't mean. The payment environment's
//! encryption key comes from the shared `vault` crate, ACL-gated under
//! the identity [`payment::VAULT_CALLER_IDENTITY`].

mod abuse;
mod health;
mod id;
mod payment;
mod registry;

pub use abuse::{AbuseDetector, AbuseDimension, AbuseThresholds, AbuseVerdict};
pub use health::{HealthMonitor, HealthStatus, HealthThresholds};
pub use id::{EnvironmentId, EnvironmentKind};
pub use payment::{EncryptedPayload, PaymentEnvironment, VAULT_CALLER_IDENTITY};
pub use registry::EnvironmentRegistry;
