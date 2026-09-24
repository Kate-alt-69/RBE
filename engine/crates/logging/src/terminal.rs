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
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt::{FmtContext, FormatEvent};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
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
        write!(
            writer,
            "{}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ")
        )?;

        let mut fields = EventFields::default();
        event.record(&mut fields);
        let fatal = fields.fatal && *event.metadata().level() == Level::ERROR;

        if self.ansi {
            let level = if fatal {
                "\x1b[91mFATAL\x1b[0m"
            } else {
                match *event.metadata().level() {
                    Level::ERROR => "\x1b[91mERROR\x1b[0m",
                    Level::WARN => "\x1b[93mWARN\x1b[0m",
                    Level::INFO => "\x1b[92mINFO\x1b[0m",
                    Level::DEBUG => "\x1b[94mDEBUG\x1b[0m",
                    Level::TRACE => "\x1b[95mTRACE\x1b[0m",
                }
            };
            write!(writer, "  {level}")?;
        } else if fatal {
            write!(writer, "  FATAL")?;
        } else {
            write!(writer, "  {}", event.metadata().level())?;
        }

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
    fatal: bool,
    fields: Vec<String>,
}

impl EventFields {
    fn record_named(&mut self, field: &tracing::field::Field, value: String) {
        match field.name() {
            "message" => self.message = value,
            "module" => self.module = Some(value),
            "fatal" => self.fatal = value == "true",
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

#[derive(Debug, Clone, Copy, Default)]
struct SuppressedLevels {
    error: bool,
    warn: bool,
    info: bool,
    debug: bool,
    trace: bool,
}

impl SuppressedLevels {
    fn from_process() -> anyhow::Result<Self> {
        let mut suppression = Self::default();

        if let Ok(value) = std::env::var("RBE_SUPPRESS_LOG_LEVELS") {
            suppression.extend(&value)?;
        }

        for argument in std::env::args().skip(1) {
            let value = argument
                .strip_prefix("-suppress=")
                .or_else(|| argument.strip_prefix("--suppress="));
            if let Some(value) = value {
                suppression.extend(value)?;
            }
        }

        Ok(suppression)
    }

    fn extend(&mut self, raw: &str) -> anyhow::Result<()> {
        for value in raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match value.to_ascii_lowercase().as_str() {
                "warning" | "warn" => self.warn = true,
                "error" => self.error = true,
                "info" => self.info = true,
                "debug" => self.debug = true,
                "trace" => self.trace = true,
                "all" => {
                    self.error = true;
                    self.warn = true;
                    self.info = true;
                    self.debug = true;
                    self.trace = true;
                }
                "none" => {}
                "fatal" => anyhow::bail!(
                    "-suppress=fatal is not supported because RBE FATAL events share the ERROR tracing level; suppress 'error' only if you intentionally want both hidden"
                ),
                unknown => anyhow::bail!(
                    "unknown log level {unknown:?} in -suppress; expected warning, error, info, debug, trace, all, or none"
                ),
            }
        }
        Ok(())
    }

    fn contains(self, metadata: &Metadata<'_>) -> bool {
        match *metadata.level() {
            Level::ERROR => self.error,
            Level::WARN => self.warn,
            Level::INFO => self.info,
            Level::DEBUG => self.debug,
            Level::TRACE => self.trace,
        }
    }
}

pub fn init(cfg: &LoggingConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let suppressed = SuppressedLevels::from_process()?;
    let suppression_filter = filter_fn(move |metadata| !suppressed.contains(metadata));

    if cfg.format == "json" {
        let output = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .with_filter(suppression_filter);
        tracing_subscriber::registry()
            .with(filter)
            .with(output)
            .try_init()?;
    } else {
        let ansi = std::io::stderr().is_terminal();
        let output = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(ansi)
            .event_format(CustomFormatter { ansi })
            .with_writer(std::io::stderr)
            .with_filter(suppression_filter);
        tracing_subscriber::registry()
            .with(filter)
            .with(output)
            .try_init()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SuppressedLevels;
    use tracing::Level;

    #[test]
    fn parses_warning_alias_without_suppressing_errors() {
        let mut levels = SuppressedLevels::default();
        levels.extend("warning").unwrap();
        assert!(levels.warn);
        assert!(!levels.error);
    }

    #[test]
    fn parses_multiple_levels() {
        let mut levels = SuppressedLevels::default();
        levels.extend("warning,debug").unwrap();
        assert!(levels.warn);
        assert!(levels.debug);
        assert!(!levels.info);
    }

    #[test]
    fn rejects_fatal_alias_to_avoid_hiding_fatal_by_accident() {
        let mut levels = SuppressedLevels::default();
        assert!(levels.extend("fatal").is_err());
    }

    #[test]
    fn level_constants_are_still_exact() {
        assert_ne!(Level::WARN, Level::ERROR);
    }
}
