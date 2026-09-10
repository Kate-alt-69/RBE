//! The writer side of a two-process issue-reporting system, ported
//! from the original Node backend's `errorReporterClient.ts` /
//! `errorReporterDaemon.ts` pair — see [`crate`] doc comment for the
//! full design; this file is the client half only.
//!
//! **Why a queue file instead of an in-process channel (what an
//! earlier version of this port used).** The daemon now runs as a
//! genuinely separate OS process (`backend.exe --er --launch`, spawned
//! as a child of the main engine — see the `backend` crate's
//! `error_reporter_daemon` module), not an in-process tokio task. A
//! channel can't cross a process boundary; a shared file both sides
//! agree on can, and it's exactly what lets ANY process — the main
//! engine, a container-runtime environment, eventually a WASM sandbox
//! — report an issue with nothing more than an [`atomic_io::AtomicIo`]
//! and the admin directory path. No shared state, no daemon-specific
//! coupling on the writer side at all. That's what "integrated
//! basically everywhere" means concretely: this crate is small and
//! dependency-light enough to add anywhere something can go wrong.
//!
//! **Never throws into callers.** Every public function here swallows
//! its own internal errors (a failed write, a full disk, whatever) —
//! matching the original's explicit design (`reportRuntimeIssue`
//! wraps its whole body in try/catch and silently returns on
//! failure). Reporting a problem should never itself become a new
//! problem for the caller to handle.

mod category;

pub use category::IssueCategory;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Queue file name, relative to the admin dir — matches the original's
/// `error-report.queue.log` exactly, since the whole point is that
/// both the client (writer, here) and the daemon (reader, in
/// `backend::error_reporter_daemon`) agree on this path without
/// either one telling the other.
pub const QUEUE_FILE_NAME: &str = "error-report.queue.log";

const MAX_QUEUE_BYTES: u64 = 512 * 1024;
const MAX_QUEUE_LINES: usize = 2_500;
const DEDUPE_CACHE_MAX: usize = 800;
const DEFAULT_DEDUPE_WINDOW_MS: u64 = 2_000;
const MAX_DEDUPE_WINDOW_MS: u64 = 30_000;
const REDACTED: &str = "[REDACTED]";
const REDACTED_PRIVATE_KEY: &str = "[REDACTED PRIVATE KEY]";
const SENSITIVE_VALUE_NAMES: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "api_key",
    "api-key",
    "apikey",
    "x-api-key",
    "secret",
    "client_secret",
    "client-secret",
    "access_token",
    "access-token",
    "refresh_token",
    "refresh-token",
    "auth_token",
    "auth-token",
    "session_token",
    "session-token",
    "cookie",
    "set-cookie",
    "database_url",
    "database-url",
    "db_url",
    "db-url",
    "connection_string",
    "connection-string",
    "dsn",
];
const SENSITIVE_LINE_HEADERS: &[&str] = &[
    "authorization:",
    "proxy-authorization:",
    "cookie:",
    "set-cookie:",
    "x-api-key:",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueLevel {
    Info,
    Warn,
    Error,
}

/// What a caller passes to [`report_issue`]. Only `source` and
/// `message` are required — everything else has the same fallback
/// behavior the original had (level defaults to Info, category is
/// inferred from the message/stack if not given).
pub struct IssueInput<'a> {
    pub source: &'a str,
    pub level: Option<IssueLevel>,
    pub category: Option<IssueCategory>,
    pub message: &'a str,
    pub stack: Option<&'a str>,
}

/// The wire format written to the queue file — one JSON object per
/// line. Public (and `Deserialize`, not just `Serialize`) specifically
/// so the daemon side (`backend::error_reporter_daemon`) can parse
/// exactly what this crate writes without a second, hand-maintained
/// copy of the same shape that could quietly drift out of sync.
#[derive(Serialize, serde::Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub ts: u64,
    pub iso: String,
    pub pid: u32,
    /// Always 0 — there's no dependency-free, cross-platform way to
    /// get the parent PID from stable Rust std the way Node's
    /// `process.ppid` is just always there. Not worth an `unsafe`
    /// platform-specific FFI call (or a new dependency) for one
    /// informational field on a struct that's mostly consumed for
    /// human debugging, not program logic. Flagged here rather than
    /// silently faked as something more precise.
    pub ppid: u32,
    pub source: String,
    pub level: IssueLevel,
    pub category: IssueCategory,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

struct ReporterState {
    io: atomic_io::AtomicIo,
    queue_path: PathBuf,
    dedupe_window: Duration,
    recent_fingerprints: Mutex<HashMap<String, Instant>>,
}

static STATE: OnceLock<ReporterState> = OnceLock::new();

/// Sets up the global reporter state for this process — call once,
/// early in boot, before installing the panic hook (so the hook can
/// safely call [`report_issue`] immediately). Calling this more than
/// once is a no-op (the second call is silently ignored, matching
/// "never throws" — an accidental double-init shouldn't crash boot).
pub fn init(io: atomic_io::AtomicIo, admin_dir: &Path) {
    let dedupe_window_ms = std::env::var("ERROR_REPORT_DEDUPE_WINDOW_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.min(MAX_DEDUPE_WINDOW_MS))
        .unwrap_or(DEFAULT_DEDUPE_WINDOW_MS);

    let _ = STATE.set(ReporterState {
        io,
        queue_path: admin_dir.join(QUEUE_FILE_NAME),
        dedupe_window: Duration::from_millis(dedupe_window_ms),
        recent_fingerprints: Mutex::new(HashMap::new()),
    });
}

/// Reports one issue — normalizes and redacts the input, drops it silently if
/// it's a near-duplicate of something reported very recently from this same
/// process, and appends one JSON line to the queue file for the daemon to pick
/// up. Redaction happens before anything reaches persistent diagnostic storage.
pub fn report_issue(input: IssueInput) {
    let Some(state) = STATE.get() else {
        return; // not initialized yet (or ever) — best-effort, no-op
    };

    let message = input.message.trim();
    if message.is_empty() {
        return;
    }
    let message = redact_sensitive_text(message);
    let message: String = message.chars().take(4_000).collect();

    let stack = input.stack.map(|s| s.trim()).filter(|s| !s.is_empty());
    let stack = stack.map(redact_sensitive_text);
    let stack_for_fingerprint: String = stack.as_deref().unwrap_or("").chars().take(256).collect();
    let stack: Option<String> = stack.map(|s| s.chars().take(4_000).collect());

    let source: String = {
        let trimmed = input.source.trim();
        let source = if trimmed.is_empty() {
            "unknown_source"
        } else {
            trimmed
        };
        source.chars().take(96).collect()
    };

    let level = input.level.unwrap_or(IssueLevel::Info);
    let category = input
        .category
        .unwrap_or_else(|| category::infer(&message, stack.as_deref().unwrap_or("")));

    let now = Instant::now();
    let message_prefix: String = message.chars().take(256).collect();
    let fingerprint =
        format!("{source}|{level:?}|{category:?}|{message_prefix}|{stack_for_fingerprint}");

    if state.dedupe_window > Duration::ZERO {
        let mut fingerprints = state.recent_fingerprints.lock().unwrap();
        let is_duplicate = fingerprints
            .get(&fingerprint)
            .map(|seen_at| now.duration_since(*seen_at) < state.dedupe_window)
            .unwrap_or(false);
        fingerprints.insert(fingerprint, now);
        if fingerprints.len() > DEDUPE_CACHE_MAX {
            fingerprints.retain(|_, seen_at| now.duration_since(*seen_at) < state.dedupe_window);
        }
        if is_duplicate {
            return;
        }
    }

    let entry = QueueEntry {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now_unix_ms(),
        iso: iso_now(),
        pid: std::process::id(),
        ppid: 0,
        source,
        level,
        category,
        message,
        stack,
    };

    let Ok(mut line) = serde_json::to_string(&entry) else {
        return; // shouldn't happen for this shape, but never throw
    };
    line.push('\n');

    // Best-effort — matches the original's bare try/catch around the
    // whole reporting path. A failure to report an issue is itself
    // logged nowhere further; there's nowhere further for it to go.
    let _ = state.io.append_locked(&state.queue_path, line.as_bytes());
    compact_queue_if_needed(&state.io, &state.queue_path);
}

fn redact_sensitive_text(input: &str) -> String {
    let mut text = input.to_string();
    redact_private_key_blocks(&mut text);
    redact_url_userinfo(&mut text);
    redact_auth_schemes(&mut text);
    for name in SENSITIVE_VALUE_NAMES {
        redact_named_values(&mut text, name);
    }
    redact_sensitive_headers(&mut text);
    text
}

fn redact_private_key_blocks(text: &mut String) {
    let mut search_from = 0usize;
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(relative_begin) = lower[search_from..].find("-----begin ") else {
            break;
        };
        let begin = search_from + relative_begin;
        let header_end = text[begin..]
            .find('\n')
            .map(|offset| begin + offset)
            .unwrap_or(text.len());
        if !lower[begin..header_end].contains("private key-----") {
            search_from = header_end.min(text.len());
            if search_from == text.len() {
                break;
            }
            continue;
        }

        let end = lower[header_end..]
            .find("-----end ")
            .map(|offset| header_end + offset)
            .and_then(|end_begin| {
                text[end_begin..]
                    .find("-----")
                    .map(|first| end_begin + first + 5)
                    .and_then(|after_first| {
                        text[after_first..]
                            .find("-----")
                            .map(|second| after_first + second + 5)
                    })
            })
            .unwrap_or(text.len());
        text.replace_range(begin..end, REDACTED_PRIVATE_KEY);
        search_from = begin + REDACTED_PRIVATE_KEY.len();
    }
}

fn redact_url_userinfo(text: &mut String) {
    let mut search_from = 0usize;
    loop {
        let Some(relative_scheme) = text[search_from..].find("://") else {
            break;
        };
        let authority_start = search_from + relative_scheme + 3;
        let authority_end = text[authority_start..]
            .find(|ch: char| ch == '/' || ch == '?' || ch == '#' || ch.is_whitespace())
            .map(|offset| authority_start + offset)
            .unwrap_or(text.len());
        let Some(relative_at) = text[authority_start..authority_end].rfind('@') else {
            search_from = authority_end;
            continue;
        };
        let userinfo_end = authority_start + relative_at;
        if userinfo_end > authority_start {
            text.replace_range(authority_start..userinfo_end, REDACTED);
            search_from = authority_start + REDACTED.len() + 1;
        } else {
            search_from = authority_end;
        }
    }
}

fn redact_auth_schemes(text: &mut String) {
    for scheme in ["bearer", "basic"] {
        let mut search_from = 0usize;
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(relative) = lower[search_from..].find(scheme) else {
                break;
            };
            let start = search_from + relative;
            if start > 0 && text.as_bytes()[start - 1].is_ascii_alphanumeric() {
                search_from = start + scheme.len();
                continue;
            }
            let mut cursor = start + scheme.len();
            let bytes = text.as_bytes();
            if cursor >= bytes.len() || !bytes[cursor].is_ascii_whitespace() {
                search_from = cursor;
                continue;
            }
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let value_start = cursor;
            while cursor < text.len() {
                let byte = text.as_bytes()[cursor];
                if byte.is_ascii_whitespace()
                    || matches!(byte, b',' | b';' | b'"' | b'\'' | b')' | b']' | b'}')
                {
                    break;
                }
                cursor += 1;
            }
            if cursor > value_start {
                text.replace_range(value_start..cursor, REDACTED);
                search_from = value_start + REDACTED.len();
            } else {
                search_from = cursor;
            }
        }
    }
}

fn redact_named_values(text: &mut String, name: &str) {
    let mut search_from = 0usize;
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(relative) = lower[search_from..].find(name) else {
            break;
        };
        let start = search_from + relative;
        let name_end = start + name.len();
        let bytes = text.as_bytes();
        if (start > 0 && is_identifier_byte(bytes[start - 1]))
            || (name_end < bytes.len() && is_identifier_byte(bytes[name_end]))
        {
            search_from = name_end;
            continue;
        }

        let mut cursor = name_end;
        if cursor < bytes.len() && matches!(bytes[cursor], b'"' | b'\'') {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || !matches!(bytes[cursor], b'=' | b':') {
            search_from = name_end;
            continue;
        }
        cursor += 1;
        while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= text.len() {
            break;
        }

        let quote = match text.as_bytes()[cursor] {
            b'"' => Some(b'"'),
            b'\'' => Some(b'\''),
            _ => None,
        };
        if let Some(quote) = quote {
            let value_start = cursor + 1;
            let value_end = text.as_bytes()[value_start..]
                .iter()
                .position(|byte| *byte == quote)
                .map(|offset| value_start + offset)
                .unwrap_or(text.len());
            if value_end > value_start {
                text.replace_range(value_start..value_end, REDACTED);
                search_from = value_start + REDACTED.len();
            } else {
                search_from = value_end;
            }
        } else {
            let value_start = cursor;
            while cursor < text.len() && !is_secret_terminator(text.as_bytes()[cursor]) {
                cursor += 1;
            }
            if cursor > value_start {
                text.replace_range(value_start..cursor, REDACTED);
                search_from = value_start + REDACTED.len();
            } else {
                search_from = cursor.saturating_add(1);
            }
        }
    }
}

fn redact_sensitive_headers(text: &mut String) {
    for header in SENSITIVE_LINE_HEADERS {
        let mut search_from = 0usize;
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(relative) = lower[search_from..].find(header) else {
                break;
            };
            let header_start = search_from + relative;
            if header_start > 0 {
                let previous = text.as_bytes()[header_start - 1];
                if is_identifier_byte(previous) {
                    search_from = header_start + header.len();
                    continue;
                }
            }
            let value_start = header_start + header.len();
            let line_end = text[value_start..]
                .find(['\r', '\n'])
                .map(|offset| value_start + offset)
                .unwrap_or(text.len());
            text.replace_range(value_start..line_end, REDACTED);
            search_from = value_start + REDACTED.len();
        }
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn is_secret_terminator(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'&' | b',' | b';' | b'"' | b'\'' | b')' | b']' | b'}' | b'<' | b'>'
        )
}

fn compact_queue_if_needed(io: &atomic_io::AtomicIo, queue_path: &Path) {
    let Ok(metadata) = std::fs::metadata(queue_path) else {
        return;
    };
    if metadata.len() < MAX_QUEUE_BYTES {
        return;
    }
    let Ok(raw) = io.read(queue_path) else {
        return;
    };
    let Ok(text) = String::from_utf8(raw) else {
        return;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() <= MAX_QUEUE_LINES {
        return;
    }
    let compacted = lines[lines.len() - MAX_QUEUE_LINES..].join("\n") + "\n";
    let _ = io.write_atomic(queue_path, compacted.as_bytes());
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Rust's `std::panic::set_hook` equivalent of the original Node
/// backend's `process.on('uncaughtExceptionMonitor', ...)`. Call once,
/// early in boot, after [`init`] — lives here rather than in the
/// engine's `logging` crate specifically so any process that links
/// this crate (container-bin, eventually a WASM sandbox host) gets the
/// same panic-reporting behavior without needing to also pull in
/// `logging`'s heavier dependencies (tokio, tracing-subscriber) just
/// for this.
///
/// Deliberately NOT trying to also replicate `unhandledRejection`:
/// Rust has no equivalent failure mode. A future that's dropped
/// without being awaited just never runs again — there's no "this
/// settled and nobody was listening" event to hook, because futures
/// are inert until polled. The closest real analog is a supervised
/// `tokio` task returning `Err` or panicking, which
/// `supervisor::Supervisor::run()` already logs and restarts — that's
/// the correct place for it, not a second global hook here.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = panic_info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic_info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());

        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());

        let stack = if std::env::var("RUST_BACKTRACE").is_ok() {
            Some(std::backtrace::Backtrace::force_capture().to_string())
        } else {
            None
        };

        report_issue(IssueInput {
            source: "rust_panic_hook",
            level: Some(IssueLevel::Error),
            category: Some(IssueCategory::RustRuntimeError),
            message: &format!("{message} at {location}"),
            stack: stack.as_deref(),
        });

        previous(panic_info);
    }));
}

/// Minimal ISO-8601 UTC formatting without pulling in a datetime
/// crate (`chrono`/`time`) for one timestamp field — matches this
/// project's "hand-roll it if it's small" dependency philosophy. Only
/// needs to handle "now," not arbitrary dates, which keeps this a lot
/// simpler than a general-purpose formatter would need to be.
fn iso_now() -> String {
    let ms = now_unix_ms();
    let secs = ms / 1000;
    let millis = ms % 1000;

    let days_since_epoch = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );

    let (year, month, day) = civil_from_days(days_since_epoch as i64);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days-since-epoch to (year, month, day) — Howard Hinnant's
/// well-known constant-time civil-from-days algorithm (proleptic
/// Gregorian calendar, correct for any date the u64 millisecond
/// timestamp above could ever represent). Public domain algorithm,
/// widely used exactly for cases like this one where pulling in a
/// full datetime crate for one conversion isn't worth it.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("error-client-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // `STATE` is a process-global `OnceLock` — matching the original's
    // module-level singleton exactly (one reporter per process). That
    // makes it awkward to unit-test more than once per test binary:
    // whichever call to `init()` runs first wins, and every later
    // `init()` call anywhere in this binary is silently a no-op, so
    // `report_issue` in every OTHER test would keep writing to the
    // FIRST test's directory regardless of what directory they pass.
    // Rather than fight that, this file has exactly one test that
    // touches global state (this one) and covers dedup + the actual
    // file write together; everything else below tests pure functions
    // that don't depend on `STATE` at all.
    #[test]
    fn report_issue_dedupes_and_writes_expected_lines_to_the_queue() {
        let dir = temp_dir("integration");
        init(atomic_io::AtomicIo::new(), &dir);

        report_issue(IssueInput {
            source: "integration_test_source",
            level: Some(IssueLevel::Error),
            category: None,
            message: "first distinct issue",
            stack: None,
        });
        report_issue(IssueInput {
            source: "integration_test_source",
            level: Some(IssueLevel::Error),
            category: None,
            message: "first distinct issue",
            stack: None,
        });
        report_issue(IssueInput {
            source: "integration_test_source",
            level: Some(IssueLevel::Warn),
            category: None,
            message: "second distinct issue",
            stack: None,
        });
        report_issue(IssueInput {
            source: "integration_test_source",
            level: None,
            category: None,
            message: "   ",
            stack: None,
        });

        let queue_path = dir.join(QUEUE_FILE_NAME);
        let contents = std::fs::read_to_string(&queue_path).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly 2 lines: one deduped away, one empty-message dropped"
        );
        assert!(lines[0].contains("first distinct issue"));
        assert!(lines[1].contains("second distinct issue"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostic_redaction_covers_common_secret_shapes() {
        let input = "Authorization: Bearer top-secret-token\npassword=hunter2&next=1\nurl=https://alice:correct-horse@example.com/private\naccess_token=abc.def.ghi\nCookie: session=very-secret\n-----BEGIN PRIVATE KEY-----\nsecret-pem-body\n-----END PRIVATE KEY-----";
        let redacted = redact_sensitive_text(input);
        for secret in [
            "top-secret-token",
            "hunter2",
            "alice",
            "correct-horse",
            "abc.def.ghi",
            "very-secret",
            "secret-pem-body",
        ] {
            assert!(
                !redacted.contains(secret),
                "secret {secret:?} leaked: {redacted}"
            );
        }
        assert!(redacted.contains(REDACTED));
        assert!(redacted.contains(REDACTED_PRIVATE_KEY));
        assert!(redacted.contains("next=1"));
        assert!(redacted.contains("example.com/private"));
    }

    #[test]
    fn diagnostic_redaction_preserves_normal_error_context() {
        let input = "database request failed after 250ms: connection refused";
        assert_eq!(redact_sensitive_text(input), input);
    }

    #[test]
    fn iso_now_produces_a_parseable_looking_timestamp() {
        let iso = iso_now();
        assert_eq!(
            iso.len(),
            24,
            "expected YYYY-MM-DDTHH:MM:SS.mmmZ, got {iso:?}"
        );
        assert!(iso.ends_with('Z'));
        assert_eq!(&iso[4..5], "-");
        assert_eq!(&iso[7..8], "-");
        assert_eq!(&iso[10..11], "T");
    }

    #[test]
    fn civil_from_days_matches_known_epoch_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
    }
}
