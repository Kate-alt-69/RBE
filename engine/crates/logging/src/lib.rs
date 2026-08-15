//! The Rust side of what the Node backend's
//! `core/utilities/logger.ts` covered: colored per-level terminal
//! output. The other half of that Node file's job — the global error
//! hooks feeding a signed error-reports log
//! (`errorReporterClient.ts`/`errorReporterDaemon.ts`) — used to live
//! here too, as an in-process supervised task. **It's moved out**:
//! the error-reporter daemon is now a genuinely separate OS process
//! (`backend.exe --er --launch`, see the `backend` crate's
//! `error_reporter_daemon` module), and the client/writer half that
//! any process calls to report an issue is the standalone
//! `error-client` crate (so container-runtime and friends can use it
//! too, without needing this whole crate's tokio/tracing-subscriber
//! dependency weight). See `error-client`'s own doc comment for the
//! full design and why it changed from the in-process version.
//!
//! Two pieces left here, each in its own module:
//! - [`terminal`] — sets up `tracing-subscriber` so output looks like
//!   the Node backend's colored `[LEVEL] [MODULE] message` terminal
//!   format (or plain JSON, for production/log-aggregation — same
//!   `logging.format` config switch they had).
//! - [`Logger`] — a thin wrapper so call sites read like their
//!   `log.info(...)` / `log.warn(...)` / `log.error(...)` ergonomics,
//!   just backed by `tracing` instead of `console.log`.

mod logger;
pub mod terminal;

pub use logger::Logger;
