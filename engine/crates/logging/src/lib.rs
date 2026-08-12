//! The Rust side of what the Node backend split across
//! `core/utilities/logger.ts` (the colored per-level `Logger` class) and
//! `core/client/errorReporterClient.ts` + `errorReporterDaemon.ts` (the
//! global `unhandledRejection`/`uncaughtExceptionMonitor` hooks feeding
//! a signed, deduped error-reports log).
//!
//! Three pieces, each in its own module:
//! - [`terminal`] — sets up `tracing-subscriber` so output looks like
//!   their colored `[LEVEL] [MODULE] message` terminal format (or plain
//!   JSON, for production/log-aggregation — same `logging.format` config
//!   switch they had).
//! - [`Logger`] — a thin wrapper so call sites read like their
//!   `log.info(...)` / `log.warn(...)` / `log.error(...)` ergonomics,
//!   just backed by `tracing` instead of `console.log`.
//! - [`error_reporter`] — the global-error-logger half of the ask: a
//!   panic hook (their `uncaughtExceptionMonitor` equivalent) plus a
//!   deduped, HMAC-signed error-reports log (their error-reporter
//!   daemon), redesigned to run as an in-process supervised task
//!   instead of a separate subprocess polling a queue file — see the
//!   module doc comment on `error_reporter` for why that's a real
//!   design change, not just a port.

mod error_reporter;
mod logger;
pub mod terminal;

pub use error_reporter::{
    install_panic_hook, report_runtime_issue, spawn_error_reporter, IssueCategory, IssueLevel,
    RuntimeIssue,
};
pub use logger::Logger;
