//! Wires up `tracing-subscriber` so output looks like the Node
//! backend's colored terminal format when `logging.format = "pretty"`,
//! or structured JSON when `logging.format = "json"` — same switch the
//! handbook's Logging Architecture described, just backed by
//! `tracing-subscriber`'s built-in formatters rather than a hand-rolled
//! ANSI formatter. (A byte-for-byte recreation of the original's exact
//! `GRAY_timestamp BOLD_COLOR[LEVEL] CYAN[MODULE] message` layout would
//! need a custom `FormatEvent` impl — worth doing later if pixel-perfect
//! parity with the old terminal output actually matters to someone's
//! muscle memory; the built-in compact formatter gets the same
//! information across today.)
//!
//! Call this once, at the very start of `boot()` in `main.rs` — before
//! config loading, per §3.2 — so failures before config even loads are
//! still visible.

use config::LoggingConfig;
use tracing_subscriber::EnvFilter;

/// Initializes the global `tracing` subscriber. Call exactly once.
pub fn init(cfg: &LoggingConfig) -> anyhow::Result<()> {
    // `RUST_LOG` env var wins if set (standard tracing convention and
    // useful for one-off debugging without editing settings.json);
    // otherwise fall back to `logging.level` from config.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let is_json = cfg.format == "json";

    if is_json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_ansi(true)
            .compact()
            .init();
    }

    Ok(())
}
