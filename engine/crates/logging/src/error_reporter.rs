//! Rust equivalent of the Node backend's `core/client/errorReporterClient.ts`
//! + `core/client/errorReporterDaemon.ts` pair: a global hook that
//! catches otherwise-unhandled failures, dedupes them, signs them
//! (HMAC-SHA256, same scheme), and appends to a reports log with a
//! periodically-flushed status file.
//!
//! **Real design change from the original, not just a port:** the Node
//! version was a writer (in the main process) appending to a queue
//! *file*, plus a wholly separate *subprocess* polling that file every
//! ~800ms, signing entries, and writing the real log — because that's
//! how you get fault-isolated, independently-restartable error
//! reporting when your language can't cheaply do it in-process. Rust
//! doesn't have that constraint (see migration-plan §7): this is one
//! in-process `tokio` task, fed by an in-memory channel instead of a
//! polled file. Same signed-log/status-file output (so any existing
//! tooling pointed at `data/admin/error-reports.log` and
//! `error-reporter-status.json` still works), same dedupe behavior,
//! zero polling latency, zero disk-based queue to lose data if the
//! process dies between a write and the daemon picking it up.
//!
//! `unhandledRejection`: intentionally not replicated — see the doc
//! comment on [`install_panic_hook`] for why there's no Rust equivalent
//! to hook.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IssueLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueCategory {
    RustRuntimeError,
    NetworkError,
    SecurityError,
    OperationFailure,
    UnknownError,
}

fn infer_category(message: &str) -> IssueCategory {
    let m = message.to_lowercase();
    if m.contains("econn")
        || m.contains("dns")
        || m.contains("network")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("connection refused")
    {
        IssueCategory::NetworkError
    } else if m.contains("forbidden")
        || m.contains("denied")
        || m.contains("unauthor")
        || m.contains("security")
    {
        IssueCategory::SecurityError
    } else if m.contains("panicked") || m.contains("panic") {
        IssueCategory::RustRuntimeError
    } else if m.contains("failed") || m.contains("error") || m.contains("exception") {
        IssueCategory::OperationFailure
    } else {
        IssueCategory::UnknownError
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeIssue {
    pub source: String,
    pub level: IssueLevel,
    pub category: Option<IssueCategory>,
    pub message: String,
    pub stack: Option<String>,
}

static REPORTER_TX: OnceLock<mpsc::UnboundedSender<RuntimeIssue>> = OnceLock::new();

/// Callable from anywhere — including a synchronous panic hook — same
/// ergonomics as the Node backend's free-standing `reportRuntimeIssue()`.
/// Falls back to logging directly (rather than silently dropping the
/// issue) both before the reporter task has started *and* if it's no
/// longer running to receive on the channel (e.g. it panicked) — see
/// the note on [`spawn_error_reporter`] about why this task isn't
/// restarted through the generic supervisor factory pattern.
pub fn report_runtime_issue(issue: RuntimeIssue) {
    let delivered = REPORTER_TX
        .get()
        .map(|tx| tx.send(issue.clone()).is_ok())
        .unwrap_or(false);

    if !delivered {
        tracing::error!(
            source = %issue.source,
            "error reporter unavailable, logging directly: {}",
            issue.message
        );
    }
}

/// Rust's `std::panic::set_hook` equivalent of the Node backend's
/// `process.on('uncaughtExceptionMonitor', ...)`. Call once, early in
/// `boot()` — right after `terminal::init`.
///
/// Deliberately NOT trying to also replicate `unhandledRejection`: Rust
/// has no equivalent failure mode. A future that's dropped without
/// being awaited just never runs again — there's no "this settled and
/// nobody was listening" event to hook, because futures are inert until
/// polled. The closest real analog is a supervised `tokio` task
/// returning `Err` or panicking, which `supervisor::Supervisor::run()`
/// already logs and restarts — that's the correct place for it, not a
/// second global hook here.
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

        report_runtime_issue(RuntimeIssue {
            source: "rust_panic_hook".to_string(),
            level: IssueLevel::Error,
            category: Some(IssueCategory::RustRuntimeError),
            message: format!("{message} at {location}"),
            stack,
        });

        previous(panic_info);
    }));
}

const MAX_REPORT_BYTES: u64 = 512 * 1024;
const MAX_REPORT_LINES: usize = 2_500;
const DEDUPE_WINDOW: Duration = Duration::from_secs(2);
const STATUS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize, Clone)]
struct ReportedBy {
    service: &'static str,
    pid: u32,
    processed_at_ms: u64,
}

#[derive(Serialize, Clone)]
struct IssuePayload {
    id: String,
    ts_ms: u64,
    iso: String,
    source: String,
    level: IssueLevel,
    category: IssueCategory,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<String>,
    reported_by: ReportedBy,
}

#[derive(Serialize)]
struct Signature {
    algo: &'static str,
    value: String,
}

#[derive(Serialize)]
struct SignedIssueRecord {
    #[serde(flatten)]
    payload: IssuePayload,
    signature: Signature,
}

#[derive(Serialize)]
struct StatusReport {
    ok: bool,
    service: &'static str,
    pid: u32,
    started_at_ms: u64,
    updated_at_ms: u64,
    processed_count: u64,
    dropped_count: u64,
    last_error_message: Option<String>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn iso_now(ts_ms: u64) -> String {
    // Avoids pulling in `chrono`/`time` just for one timestamp field —
    // reports are machine-consumed (signed JSON), so an RFC 3339-ish
    // millis-since-epoch-derived string is enough; swap for `time` if
    // a real calendar-formatted ISO 8601 string turns out to matter.
    format!("{ts_ms}")
}

fn read_or_create_signing_key(
    io: &atomic_io::AtomicIo,
    admin_dir: &Path,
) -> anyhow::Result<String> {
    if let Ok(from_env) = std::env::var("ERROR_REPORT_SIGNING_KEY") {
        if from_env.len() >= 16 {
            return Ok(from_env);
        }
    }

    fs::create_dir_all(admin_dir)?;
    let key_path = admin_dir.join("error-reporter.key");

    if let Ok(existing) = fs::read_to_string(&key_path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 16 {
            return Ok(trimmed.to_string());
        }
    }

    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let generated = hex::encode(bytes);
    io.write_atomic(&key_path, generated.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&key_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&key_path, perms)?;
    }

    Ok(generated)
}

fn sign(canonical_json: &str, key: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(canonical_json.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn compact_reports_file(io: &atomic_io::AtomicIo, path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_REPORT_BYTES {
        return;
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() <= MAX_REPORT_LINES {
        return;
    }
    let compacted = lines[lines.len() - MAX_REPORT_LINES..].join("\n") + "\n";
    let _ = io.write_atomic(path, compacted.as_bytes());
}

fn append_report_line(io: &atomic_io::AtomicIo, path: &Path, line: &str) -> anyhow::Result<()> {
    let mut with_newline = line.to_string();
    with_newline.push('\n');
    io.append_locked(path, with_newline.as_bytes())?;
    Ok(())
}

fn write_status(io: &atomic_io::AtomicIo, path: &Path, status: &StatusReport) {
    if let Ok(json) = serde_json::to_string_pretty(status) {
        let _ = io.write_atomic(path, json.as_bytes());
    }
}

/// Spawns the background task that consumes reported issues, dedupes,
/// signs, and appends them to `<admin_dir>/error-reports.log`, with a
/// status file at `<admin_dir>/error-reporter-status.json` — same
/// paths the Node backend used under `data/admin/`, so existing
/// tooling/dashboards pointed there keep working.
///
/// **Not registered through `supervisor::Supervisor`'s generic
/// factory-based restart**, unlike other bootstrap services (§7) —
/// deliberately, not an oversight. This task owns the process-global
/// `REPORTER_TX` static, set exactly once via `OnceLock`; a generic
/// restart would call this function again to build a "fresh" future,
/// which would fail trying to re-set that static. Call this once from
/// `main.rs`, hand the returned future to a plain `tokio::spawn`, and
/// rely on [`report_runtime_issue`]'s direct-logging fallback if this
/// task ever dies — losing the signed-log path temporarily beats a
/// fragile restart that can't actually reconnect a new receiver to the
/// same static sender. Worth a proper fix (e.g. an `ArcSwap` over the
/// sender, or moving off a static entirely once `AppState` threads
/// through everywhere that calls `report_runtime_issue`) if this proves
/// to matter in practice.
pub fn spawn_error_reporter(
    io: atomic_io::AtomicIo,
    admin_dir: PathBuf,
) -> anyhow::Result<supervisor::TaskFuture> {
    let (tx, mut rx) = mpsc::unbounded_channel::<RuntimeIssue>();
    REPORTER_TX.set(tx).map_err(|_| {
        anyhow::anyhow!("error reporter already started (spawn_error_reporter called twice?)")
    })?;

    let signing_key = read_or_create_signing_key(&io, &admin_dir)?;
    let reports_path = admin_dir.join("error-reports.log");
    let status_path = admin_dir.join("error-reporter-status.json");
    let pid = std::process::id();
    let started_at_ms = now_unix_ms();

    let task: supervisor::TaskFuture = Box::pin(async move {
        let mut recent_fingerprints: HashMap<String, Instant> = HashMap::new();
        let mut processed_count: u64 = 0;
        let mut dropped_count: u64 = 0;
        let mut last_error_message: Option<String> = None;
        let mut status_interval = tokio::time::interval(STATUS_FLUSH_INTERVAL);

        loop {
            tokio::select! {
                maybe_issue = rx.recv() => {
                    let Some(issue) = maybe_issue else {
                        // Sender dropped — shouldn't happen (REPORTER_TX
                        // holds it for the process lifetime), but exit
                        // cleanly rather than spin if it ever does.
                        break;
                    };

                    let now = Instant::now();
                    let message_prefix: String = issue.message.chars().take(256).collect();
                    let fingerprint = format!(
                        "{}|{:?}|{}",
                        issue.source, issue.level, message_prefix
                    );

                    let is_duplicate = recent_fingerprints
                        .get(&fingerprint)
                        .map(|seen_at| now.duration_since(*seen_at) < DEDUPE_WINDOW)
                        .unwrap_or(false);
                    recent_fingerprints.insert(fingerprint, now);
                    recent_fingerprints
                        .retain(|_, seen_at| now.duration_since(*seen_at) < DEDUPE_WINDOW);

                    if is_duplicate {
                        dropped_count += 1;
                        continue;
                    }

                    let category = issue.category.unwrap_or_else(|| infer_category(&issue.message));
                    let ts_ms = now_unix_ms();

                    let payload = IssuePayload {
                        id: uuid::Uuid::new_v4().to_string(),
                        ts_ms,
                        iso: iso_now(ts_ms),
                        source: issue.source,
                        level: issue.level,
                        category,
                        message: issue.message.chars().take(4_000).collect(),
                        stack: issue.stack.map(|s| s.chars().take(4_000).collect()),
                        reported_by: ReportedBy {
                            service: "error-reporter",
                            pid,
                            processed_at_ms: ts_ms,
                        },
                    };

                    match serde_json::to_string(&payload) {
                        Ok(canonical) => {
                            let signature = Signature {
                                algo: "hmac-sha256",
                                value: sign(&canonical, &signing_key),
                            };
                            let signed = SignedIssueRecord { payload, signature };

                            match serde_json::to_string(&signed) {
                                Ok(line) => {
                                    if let Err(err) = append_report_line(&io, &reports_path, &line) {
                                        last_error_message = Some(format!("failed to write report: {err:#}"));
                                    } else {
                                        processed_count += 1;
                                        compact_reports_file(&io, &reports_path);
                                    }
                                }
                                Err(err) => {
                                    last_error_message = Some(format!("failed to serialize signed report: {err:#}"));
                                }
                            }
                        }
                        Err(err) => {
                            last_error_message = Some(format!("failed to serialize report payload: {err:#}"));
                        }
                    }
                }
                _ = status_interval.tick() => {
                    write_status(&io, &status_path, &StatusReport {
                        ok: true,
                        service: "error-reporter",
                        pid,
                        started_at_ms,
                        updated_at_ms: now_unix_ms(),
                        processed_count,
                        dropped_count,
                        last_error_message: last_error_message.clone(),
                    });
                }
            }
        }

        Ok(())
    });

    Ok(task)
}
