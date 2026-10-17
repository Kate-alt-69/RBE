//! Bounded REL log encoding. Never format an arbitrary Value with Debug: that
//! traverses and allocates the complete object before a logger can truncate it.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value as Json};

use super::ModuleError;
use crate::ast::Value;

const MAX_TEXT_BYTES: usize = 1024;
const MAX_KEY_BYTES: usize = 128;
const MAX_EVENT_BYTES: usize = 8192;
const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 4;
const EVENTS_PER_SECOND: u32 = 100;

struct Encoder {
    nodes: usize,
    text_bytes: usize,
    truncated: bool,
}

impl Encoder {
    fn text(&mut self, text: &str, limit: usize) -> String {
        let mut result = String::new();
        // Query strings often contain signed credentials; a URL field keeps only
        // its path. Redaction cannot recognize secrets embedded in arbitrary prose.
        let end = text.floor_char_boundary(limit.min(text.len()));
        if end < text.len() {
            self.truncated = true;
        }
        let text = &text[..end];
        let text = if text.starts_with("https://") || text.starts_with("http://") {
            text.split(['?', '#']).next().unwrap_or_default()
        } else {
            text
        };
        for ch in text.chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            if result.len() + ch.len_utf8() > limit || self.text_bytes < ch.len_utf8() {
                self.truncated = true;
                break;
            }
            self.text_bytes -= ch.len_utf8();
            result.push(ch);
        }
        result
    }

    fn value(&mut self, value: &Value, depth: usize) -> Json {
        if self.nodes == 0 {
            self.truncated = true;
            return Json::String("[truncated]".into());
        }
        self.nodes -= 1;
        // Truncated children must still spend a node. Otherwise a very wide
        // collection just below the depth cutoff can allocate unbounded markers.
        if depth > MAX_DEPTH {
            self.truncated = true;
            return Json::String("[truncated]".into());
        }
        match value {
            Value::String(value) => Json::String(self.text(value, MAX_TEXT_BYTES)),
            Value::Number(value) => serde_json::Number::from_f64(*value)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Value::Bool(value) => Json::Bool(*value),
            Value::Null => Json::Null,
            Value::Array(values) => {
                let mut out = Vec::new();
                for value in values {
                    if self.nodes == 0 {
                        self.truncated = true;
                        break;
                    }
                    out.push(self.value(value, depth + 1));
                }
                Json::Array(out)
            }
            Value::Object(values) => {
                let mut out = Map::new();
                for (key, value) in values {
                    if self.nodes == 0 {
                        self.truncated = true;
                        break;
                    }
                    // Never truncate a secret-bearing key into an innocuous spelling.
                    if key.len() > MAX_KEY_BYTES {
                        self.nodes -= 1;
                        self.truncated = true;
                        continue;
                    }
                    let encoded = if secret_key(key) {
                        self.nodes -= 1;
                        Json::String("[redacted]".into())
                    } else {
                        self.value(value, depth + 1)
                    };
                    out.insert(self.text(key, MAX_KEY_BYTES), encoded);
                }
                Json::Object(out)
            }
        }
    }
}

fn secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    [
        "password",
        "token",
        "secret",
        "credential",
        "authorization",
        "cookie",
        "apikey",
        "privatekey",
        "signature",
    ]
    .iter()
    .any(|part| normalized.contains(part))
}

/// Fixed-size rate state has no queue or per-request map. Suppressed content is
/// discarded; the next admitted event reports only the number that was dropped.
struct RateLimit {
    window: Instant,
    count: u32,
    suppressed: u64,
}

impl RateLimit {
    fn admit(&mut self, now: Instant) -> Option<u64> {
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            self.window = now;
            self.count = 0;
        }
        if self.count == EVENTS_PER_SECOND {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.count += 1;
        Some(std::mem::take(&mut self.suppressed))
    }
}

fn encode(args: &[Value]) -> (String, String, bool) {
    let mut encoder = Encoder {
        nodes: MAX_NODES,
        text_bytes: 4096,
        truncated: false,
    };
    let message = match args.first() {
        Some(Value::String(message)) => encoder.text(message, MAX_TEXT_BYTES),
        Some(value) => encoder.value(value, 0).to_string(),
        None => String::new(),
    };
    let fields = args
        .get(1)
        .map(|value| encoder.value(value, 0))
        .unwrap_or(Json::Null);
    let mut fields = fields.to_string();
    // Sanitized nodes and text are already bounded. This final encoded-byte cap
    // also accounts for JSON escaping; never output a partial/invalid JSON object.
    let mut message = message;
    if message.len() + fields.len() > MAX_EVENT_BYTES {
        fields = "{\"truncated\":true}".into();
        message.truncate(message.floor_char_boundary(MAX_TEXT_BYTES.min(message.len())));
        encoder.truncated = true;
    }
    (message, fields, encoder.truncated)
}

pub(super) fn call(function: &str, args: &[Value]) -> Result<Value, ModuleError> {
    let enabled = match function {
        "info" => tracing::enabled!(tracing::Level::INFO),
        "warn" => tracing::enabled!(tracing::Level::WARN),
        "error" => tracing::enabled!(tracing::Level::ERROR),
        "debug" => tracing::enabled!(tracing::Level::DEBUG),
        _ => {
            return Err(ModuleError {
                message: "unknown log operation".into(),
            })
        }
    };
    if args.len() > 2
        || args
            .get(1)
            .is_some_and(|value| !matches!(value, Value::Object(_)))
    {
        return Err(ModuleError {
            message: "log accepts a message and optional fields object".into(),
        });
    }
    if !enabled {
        return Ok(Value::Null);
    }
    static RATE: OnceLock<Mutex<RateLimit>> = OnceLock::new();
    let rate = RATE.get_or_init(|| {
        Mutex::new(RateLimit {
            window: Instant::now(),
            count: 0,
            suppressed: 0,
        })
    });
    let Some(suppressed) = rate
        .lock()
        .ok()
        .and_then(|mut rate| rate.admit(Instant::now()))
    else {
        return Ok(Value::Null);
    };
    let (message, fields, truncated) = encode(args);
    // Application fields remain nested JSON data and cannot replace host severity,
    // timestamp, target, or other envelope fields supplied by tracing subscribers.
    match function {
        "info" => {
            tracing::info!(message = %message, fields = %fields, truncated, suppressed)
        }
        "warn" => {
            tracing::warn!(message = %message, fields = %fields, truncated, suppressed)
        }
        "error" => {
            tracing::error!(message = %message, fields = %fields, truncated, suppressed)
        }
        "debug" => {
            tracing::debug!(message = %message, fields = %fields, truncated, suppressed)
        }
        _ => unreachable!(),
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn public_log_calls_emit_structured_fields_at_each_level() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<Mutex<Vec<HashMap<String, String>>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
                Some(tracing::level_filters::LevelFilter::TRACE)
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Visitor(HashMap<String, String>);
                impl tracing::field::Visit for Visitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.insert(field.name().to_owned(), format!("{value:?}"));
                    }
                }
                let mut visitor = Visitor(HashMap::new());
                event.record(&mut visitor);
                visitor
                    .0
                    .insert("severity".into(), event.metadata().level().to_string());
                self.0.lock().unwrap().push(visitor.0);
            }
        }
        let capture = Capture(Default::default());
        tracing::subscriber::with_default(capture.clone(), || {
            for level in ["info", "warn", "error", "debug"] {
                call(
                    level,
                    &[
                        Value::String("operation failed".into()),
                        Value::Object(HashMap::from([
                            ("code".into(), Value::String("TEST".into())),
                            ("password".into(), Value::String("never-log-this".into())),
                        ])),
                    ],
                )
                .unwrap();
            }
        });
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 4);
        for (event, severity) in events.iter().zip(["INFO", "WARN", "ERROR", "DEBUG"]) {
            assert_eq!(event["severity"], severity);
            assert_eq!(event["message"], "operation failed");
            let fields: Json = serde_json::from_str(&event["fields"]).unwrap();
            assert_eq!(fields["code"], "TEST");
            assert_eq!(fields["password"], "[redacted]");
        }
    }

    #[test]
    fn fields_are_redacted_and_cannot_forge_the_log_envelope() {
        let fields = Value::Object(HashMap::from([
            (
                "Authorization".into(),
                Value::String("Bearer private".into()),
            ),
            (
                "nested".into(),
                Value::Object(HashMap::from([(
                    "refresh_token".into(),
                    Value::String("private".into()),
                )])),
            ),
            (
                "url".into(),
                Value::String("https://example.test/file?signature=private".into()),
            ),
            ("level".into(), Value::String("FAKE".into())),
        ]));
        let (message, fields, _) = encode(&[Value::String("hello\nforged log".into()), fields]);
        assert!(!message.contains('\n'));
        assert!(!fields.contains("private"));
        let fields: Json = serde_json::from_str(&fields).unwrap();
        assert_eq!(fields["Authorization"], "[redacted]");
        assert_eq!(fields["nested"]["refresh_token"], "[redacted]");
        assert_eq!(fields["url"], "https://example.test/file");
        assert_eq!(fields["level"], "FAKE");
    }

    #[test]
    fn huge_and_nested_values_are_bounded_before_formatting() {
        let mut value = Value::String("é".repeat(100_000));
        for _ in 0..20 {
            value = Value::Array(vec![value]);
        }
        let (message, fields, truncated) = encode(&[value]);
        assert!(truncated);
        assert!(message.len() + fields.len() <= MAX_EVENT_BYTES);
        let value = Value::Array(vec![Value::String("\\\"".repeat(50_000)); 300]);
        let (message, fields, truncated) = encode(&[value]);
        assert!(truncated);
        assert!(message.len() + fields.len() <= MAX_EVENT_BYTES);
    }

    #[test]
    fn children_at_the_depth_cutoff_still_consume_the_node_budget() {
        let mut encoder = Encoder {
            nodes: 10,
            text_bytes: 4096,
            truncated: false,
        };
        let Json::Array(values) = encoder.value(&Value::Array(vec![Value::Null; 100]), MAX_DEPTH)
        else {
            panic!("array expected");
        };
        assert_eq!(values.len(), 9);
        assert_eq!(encoder.nodes, 0);
        assert!(encoder.truncated);
    }

    #[test]
    fn rate_limit_counts_drops_and_recovers_after_window() {
        let now = Instant::now();
        let mut rate = RateLimit {
            window: now,
            count: 0,
            suppressed: 0,
        };
        for _ in 0..EVENTS_PER_SECOND {
            assert_eq!(rate.admit(now), Some(0));
        }
        assert_eq!(rate.admit(now), None);
        assert_eq!(rate.admit(now), None);
        assert_eq!(rate.admit(now + Duration::from_secs(1)), Some(2));
    }
}
