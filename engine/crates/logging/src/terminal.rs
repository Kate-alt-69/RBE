//! Wires up `tracing-subscriber` so output looks like the Node
//! backend's colored terminal format when `logging.format = "pretty"`,
//! or structured JSON when `logging.format = "json"`.
//!
//! Output format: `[TIMESTAMP] LEVEL [MODULE] message key=value ...`.
//! The formatter collects the actual event message and every structured
//! field directly from the tracing event before rendering. Fields are not
//! discarded just because they were variables rather than part of the
//! message string.

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

        write!(writer, "{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ"))?;

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

        let mut fields = EventFields::default();
        event.record(&mut fields);

        if let Some(module) = fields.module.as_deref() {
            if self.ansi {
                write!(writer, " \x1b[36m[{module}]\x1b[0m")?;
            } else {
                write!(writer, " [{module}]")?;
            }
        }

        if fields.message.is_empty() {
            if fields.fields.is_empty() {
                writeln!(writer)
            } else {
                writeln!(writer, ": {}", fields.fields.join(" "))
            }
        } else if fields.fields.is_empty() {
            writeln!(writer, ": {}", fields.message)
        } else {
            writeln!(writer, ": {} {}", fields.message, fields.fields.join(" "))
        }
    }
}

#[derive(Default)]
struct EventFields {
    module: Option<String>,
    message: String,
    fields: Vec<String>,
}

impl EventFields {
    fn record_named(&mut self, field: &tracing::field::Field, value: String) {
        match field.name() {
            "message" => self.message = value,
            "module" => self.module = Some(value),
            _ => self.fields.push(format!("{}={}", field.name(), value)),
        }
    }

    fn push_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.record_named(field, format!("{value:?}"));
    }
}

impl tracing::field::Visit for EventFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_named(field, value.to_owned());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_named(field, value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_named(field, value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_named(field, value.to_string());
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        self.record_named(field, value.to_string());
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.record_named(field, value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.record_named(field, value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.push_debug(field, value);
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
