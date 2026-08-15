//! Wires up `tracing-subscriber` so output looks like the Node
//! backend's colored terminal format when `logging.format = "pretty"`,
//! or structured JSON when `logging.format = "json"` — same switch the
//! handbook's Logging Architecture described, just backed by
//! `tracing-subscriber`'s built-in formatters rather than a hand-rolled
//! ANSI formatter. Output format: `[TIMESTAMP] LEVEL [MODULE] message`
//! where MODULE comes from the structured `module` field in Logger,
//! not the Rust file path target.
//!
//! Call this once, at the very start of `boot()` in `main.rs` — before
//! config loading, per §3.2 — so failures before config even loads are
//! still visible.

use config::LoggingConfig;
use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FmtContext};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// Custom event formatter that shows the `module` field from Logger
/// instead of the Rust file target path.
struct CustomFormatter;

impl<S> FormatEvent<S, tracing_subscriber::fmt::format::DefaultFields> for CustomFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, tracing_subscriber::fmt::format::DefaultFields>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        use tracing::Level;
        
        let metadata = event.metadata();
        let level = metadata.level();
        
        // Format the timestamp
        let now = chrono::Local::now();
        write!(writer, "{}", now.format("%Y-%m-%dT%H:%M:%S%.6fZ"))?;
        
        // Format the level with color
        let level_str = match *level {
            Level::ERROR => "\x1b[91mERROR\x1b[0m",  // Bright red
            Level::WARN => "\x1b[93mWARN\x1b[0m",    // Bright yellow
            Level::INFO => "\x1b[92mINFO\x1b[0m",    // Bright green
            Level::DEBUG => "\x1b[94mDEBUG\x1b[0m",  // Bright blue
            Level::TRACE => "\x1b[95mTRACE\x1b[0m",  // Bright magenta
        };
        write!(writer, "  {}", level_str)?;
        
        // Try to get the module field from the event
        let mut module = None;
        let mut visitor = ModuleVisitor { module: &mut module };
        event.record(&mut visitor);
        
        // Write module name if available
        if let Some(m) = module {
            write!(writer, " \x1b[36m[{}]\x1b[0m", m)?;  // Cyan
        }
        
        // Write the message
        write!(writer, ": ")?;
        
        // Get the message field
        let mut message = String::new();
        let mut msg_visitor = MessageVisitor { message: &mut message };
        event.record(&mut msg_visitor);
        write!(writer, "{}", message)?;
        
        writeln!(writer)?;
        
        Ok(())
    }
}

/// Visitor to extract the `module` field from a tracing Event
struct ModuleVisitor<'a> {
    module: &'a mut Option<String>,
}

impl<'a> tracing::field::Visit for ModuleVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "module" {
            *self.module = Some(format!("{:?}", value).trim_matches('"').to_string());
        }
    }
}

/// Visitor to extract the message from a tracing Event
struct MessageVisitor<'a> {
    message: &'a mut String,
}

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.message = format!("{:?}", value).trim_matches('"').to_string();
        }
    }
}

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
            .with_target(false)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(true)
            .event_format(CustomFormatter)
            .with_writer(std::io::stderr)
            .init();
    }

    Ok(())
}
