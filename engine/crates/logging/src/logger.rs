//! Rust equivalent of the Node backend's `core/utilities/logger.ts`
//! `Logger` class. Same call-site ergonomics (`Logger::new("account")`,
//! `.info(...)`, `.warn(...)`, `.error(...)`, `.debug(...)`,
//! `.child(...)`), backed by `tracing` instead of `console.log`.
//!
//! One deliberate design change (per "probably improve on the design"):
//! the module name is attached as a structured `tracing` field, not
//! string-interpolated into the message. That means `logging::terminal`
//! renders it as `[MODULE]` for humans (§below) *and* a JSON-format log
//! line gets a real, filterable `module` key — the original's
//! `[${this.module}]` was baked into the message text, which loses that
//! structure entirely in any log aggregator that isn't specifically
//! parsing the bracket format.

/// A named logger — one per module/subsystem, same idea as the Node
/// class. Cheap to construct; typically created once per module (e.g.
/// as a `static` or a field on a service struct) rather than per call.
#[derive(Debug, Clone)]
pub struct Logger {
    module: String,
}

impl Logger {
    pub fn new(module: impl Into<String>) -> Self {
        Self {
            module: module.into().to_uppercase(),
        }
    }

    /// Create a child logger with a sub-module name, e.g.
    /// `Logger::new("CONTAINER").child("POOL")` -> module `CONTAINER:POOL`.
    pub fn child(&self, sub_module: impl std::fmt::Display) -> Self {
        Self {
            module: format!("{}:{}", self.module, sub_module),
        }
    }

    pub fn info(&self, message: impl std::fmt::Display) {
        tracing::info!(module = %self.module, "{message}");
    }

    pub fn warn(&self, message: impl std::fmt::Display) {
        tracing::warn!(module = %self.module, "{message}");
    }

    pub fn error(&self, message: impl std::fmt::Display) {
        tracing::error!(module = %self.module, "{message}");
    }

    /// Unlike the Node version (which checked `process.env.DEBUG ===
    /// 'true'` itself), level gating here is handled by the
    /// `EnvFilter` set up in `terminal::init` — `logging.level` in
    /// `settings.json` (or `RUST_LOG` env override) controls whether
    /// this actually emits, same effect, one less thing for this type
    /// to own.
    pub fn debug(&self, message: impl std::fmt::Display) {
        tracing::debug!(module = %self.module, "{message}");
    }
}
