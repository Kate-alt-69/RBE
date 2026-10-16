//! Bounded provider diagnostics for stderr/host log collectors. Never serialize
//! a request URL, response body, authorization header or provider credential.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Response;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_EVENTS: usize = 256;
type EventKey = (String, String, u16);

#[derive(Default)]
struct Limiter {
    events: HashMap<EventKey, (Instant, u64)>,
}

impl Limiter {
    /// Bound both memory and log volume even if a failing provider returns many
    /// different object paths. Counts describe repeats since the previous emit.
    fn admit(&mut self, key: EventKey, now: Instant) -> Option<u64> {
        if let Some((last, suppressed)) = self.events.get_mut(&key) {
            if now.duration_since(*last) < WINDOW {
                *suppressed = suppressed.saturating_add(1);
                return None;
            }
            let count = *suppressed;
            *last = now;
            *suppressed = 0;
            return Some(count);
        }
        self.events
            .retain(|_, (last, _)| now.duration_since(*last) < WINDOW);
        if self.events.len() >= MAX_EVENTS {
            return None;
        }
        self.events.insert(key, (now, 0));
        Some(0)
    }
}

fn http_event(url: &reqwest::Url, status: u16, operation: &str, missing: bool) -> Value {
    // Object paths can contain user filenames. A stable digest correlates
    // failures without logging those paths or any signed query parameters.
    json!({
        "component": "cloud_node",
        "event": if missing { "provider_object_missing" } else { "provider_http_error" },
        "level": if missing { "info" } else { "warning" },
        "operation": operation,
        "status": status,
        "pathSha256": hex::encode(Sha256::digest(url.path().as_bytes())),
    })
}

pub(crate) fn record_http(response: &Response, operation: &str, missing: bool) {
    let event = http_event(
        response.url(),
        response.status().as_u16(),
        operation,
        missing,
    );
    let key = (
        operation.to_owned(),
        event["pathSha256"].as_str().unwrap_or_default().to_owned(),
        response.status().as_u16(),
    );
    emit(event, key);
}

pub(crate) fn record_transport(error: &reqwest::Error) {
    let category = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "transfer"
    };
    emit(
        json!({ "component": "cloud_node", "event": "provider_transport_error", "level": "warning", "category": category }),
        ("transport".to_owned(), category.to_owned(), 0),
    );
}

fn emit(mut event: Value, key: EventKey) {
    static LIMITER: OnceLock<Mutex<Limiter>> = OnceLock::new();
    let Ok(mut limiter) = LIMITER
        .get_or_init(|| Mutex::new(Limiter::default()))
        .lock()
    else {
        return;
    };
    let Some(suppressed) = limiter.admit(key, Instant::now()) else {
        return;
    };
    drop(limiter);
    event["suppressed"] = json!(suppressed);
    event["timestampUnixMs"] = json!(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64);
    eprintln!("{event}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_urls_and_personal_paths_never_enter_events() {
        let url = "https://user:password@storage.example/private-email/snapshot.json?X-Amz-Signature=secret#secret".parse().unwrap();
        let event = http_event(&url, 404, "download", true);
        assert_eq!(event["status"], 404);
        assert_eq!(event["pathSha256"].as_str().unwrap().len(), 64);
        let text = event.to_string();
        for secret in [
            "password",
            "private-email",
            "Signature",
            "secret",
            "credentials",
            "storage.example",
        ] {
            assert!(!text.contains(secret));
        }
    }

    #[test]
    fn repeated_errors_are_summarized_and_memory_is_bounded() {
        let mut limiter = Limiter::default();
        let now = Instant::now();
        let key = ("download".to_owned(), "path".to_owned(), 503);
        assert_eq!(limiter.admit(key.clone(), now), Some(0));
        assert_eq!(limiter.admit(key.clone(), now), None);
        assert_eq!(limiter.admit(key, now + WINDOW), Some(1));
        for index in 0..MAX_EVENTS * 2 {
            limiter.admit(
                ("download".to_owned(), index.to_string(), 404),
                now + WINDOW,
            );
        }
        assert_eq!(limiter.events.len(), MAX_EVENTS);
    }
}
