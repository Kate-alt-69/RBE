//! Wires up `tracing-subscriber` so output looks like the Node
//! backend's colored terminal format when `logging.format = "pretty"`,
//! or structured JSON when `logging.format = "json"`.
//!
//! Output format: `[TIMESTAMP] LEVEL [MODULE] message` where MODULE
//! comes from the structured `module` field in Logger, not the Rust
//! file path target.

use config::LoggingConfig;
use std::fmt;
use std::io::IsTerminal;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::{FmtContext, FormatEvent};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

struct CustomFormatter {
    ansi: bool,
}

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

        write!(writer, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6fZ"))?;

        if self.ansi {
            let level = match *event.metadata().level() {
                Level::ERROR => "\x1b[91mERROR\x1b[0m",
                Level::WARN => "\x1b[93mWARN\x1b[0m",
                Level::INFO => "\x1b[92mINFO\x1b[0m",
                Level::DEBUG => "\x1b[94mDEBUG\x1b[0m",
                Level::TRACE => "\x1b[95mTRACE\x1b[0m",
            };
            write!(writer, "  {level}")?;
        } else {
            write!(writer, "  {}", event.metadata().level())?;
        }

        let mut module = None;
        event.record(&mut ModuleVisitor { module: &mut module });
        if let Some(module) = module {
            if self.ansi {
                write!(writer, " \x1b[36m[{module}]\x1b[0m")?;
            } else {
                write!(writer, " [{module}]")?;
            }
        }

        write!(writer, ": ")?;
        let mut message = String::new();
        event.record(&mut MessageVisitor { message: &mut message });
        writeln!(writer, "{message}")
    }
}

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

pub fn init(cfg: &LoggingConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    if cfg.format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .init();
    } else {
        let ansi = std::io::stderr().is_terminal();
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(ansi)
            .event_format(CustomFormatter { ansi })
            .with_writer(std::io::stderr)
            .init();
    }

    Ok(())
}
