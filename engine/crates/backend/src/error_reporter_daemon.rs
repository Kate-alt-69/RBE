//! The reader/processor side of the issue-reporting system, ported
//! from the original Node backend's `errorReporterDaemon.ts` — the
//! counterpart to `error_client`'s writer side (see that crate's doc
//! comment for the full two-process design).
//!
//! Runs when `backend.exe` is invoked as `--er --launch` (see
//! `main.rs`'s CLI dispatch) — genuinely a separate OS process, spawned
//! as a child of the normal engine process at boot (or run standalone,
//! e.g. under a process manager). Polls the queue file
//! `error_client::QUEUE_FILE_NAME` at a fixed interval, signs each new
//! entry (HMAC-SHA256 over a stable-key-sorted canonical JSON encoding
//! — same scheme the original used, via [`canonical_json`]), and
//! appends the signed record to `error-reports.log`, with a
//! periodically-flushed status file at `error-reporter-status.json` —
//! same file names the original used under `data/admin/`, so anything
//! already pointed there keeps working.
//!
//! **A known characteristic inherited from the original, not a new
//! bug:** the byte offset into the queue file is in-memory only, reset
//! to 0 on daemon start. If the daemon restarts while the queue file
//! still contains already-processed entries (normal — the queue isn't
//! truncated as it's consumed, only compacted by size), those entries
//! get re-signed and re-appended to `error-reports.log` as functional
//! duplicates. The original Node daemon has this exact same
//! characteristic (`queueOffset` is plain in-memory module state
//! there too). A real fix would persist the offset (e.g. in the status
//! file, resumed on start) — worth doing at some point, not done here
//! since the goal was porting what's there, not silently changing its
//! behavior.
//!
//! **Also not implemented:** setting the process title for visibility
//! in a task manager / `ps` listing (Node's `process.title = ...`).
//! There's no dependency-free, verified-without-a-compiler way to do
//! this correctly across Windows/Linux/macOS from stable Rust std, and
//! it's a "nice for observability" feature, not a functional one — see
//! `error_client::QueueEntry`'s doc comment for the same reasoning
//! applied to `ppid`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use error_client::{IssueCategory, IssueLevel, QueueEntry};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const MAX_REPORT_BYTES: u64 = 512 * 1024;
const MAX_REPORT_LINES: usize = 2_500;
const DEFAULT_POLL_INTERVAL_MS: u64 = 800;
const MIN_POLL_INTERVAL_MS: u64 = 250;
const MAX_POLL_INTERVAL_MS: u64 = 5_000;
const STATUS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize)]
struct ReportedBy {
    service: &'static str,
    pid: u32,
    processed_at_ms: u64,
    processed_iso: String,
}

/// What the ORIGINAL submitting process reported about itself — kept
/// distinct from [`ReportedBy`] (which describes the DAEMON doing the
/// signing) so a signed record can answer both "who noticed this" and
/// "who processed it," matching the original's `runtime: { pid, ppid }`
/// field on top of its own `reportedBy`.
#[derive(Serialize)]
struct SubmittedBy {
    pid: u32,
    ppid: u32,
}

#[derive(Serialize)]
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
    submitted_by: SubmittedBy,
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
    launched_as_separate_process: bool,
    started_at_ms: u64,
    updated_at_ms: u64,
    queue_offset: u64,
    processed_count: u64,
    dropped_count: u64,
    last_error_message: Option<String>,
}

/// Runs the daemon loop until a shutdown signal arrives. Returns
/// `Ok(())` on a clean shutdown; only returns `Err` for a setup
/// failure severe enough that there's no point continuing (couldn't
/// read/create the signing key, couldn't create `admin_dir`).
pub async fn run(
    io: atomic_io::AtomicIo,
    admin_dir: PathBuf,
    launched_as_separate_process: bool,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&admin_dir)?;
    let signing_key = read_or_create_signing_key(&io, &admin_dir)?;

    let queue_path = admin_dir.join(error_client::QUEUE_FILE_NAME);
    let reports_path = admin_dir.join("error-reports.log");
    let status_path = admin_dir.join("error-reporter-status.json");

    let poll_interval_ms = std::env::var("ERROR_REPORT_POLL_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms.clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS))
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS);

    tracing::info!(
        pid = std::process::id(),
        queue_path = %queue_path.display(),
        reports_path = %reports_path.display(),
        poll_interval_ms,
        "error-reporter daemon started"
    );

    let pid = std::process::id();
    let started_at_ms = now_unix_ms();
    let mut tail = TailState::default();
    let mut processed_count: u64 = 0;
    let mut dropped_count: u64 = 0;
    let mut last_error_message: Option<String> = None;

    let mut poll_interval = tokio::time::interval(Duration::from_millis(poll_interval_ms));
    let mut status_interval = tokio::time::interval(STATUS_FLUSH_INTERVAL);

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                let lines = read_new_lines(&queue_path, &mut tail);
                for line in lines {
                    match serde_json::from_str::<QueueEntry>(&line) {
                        Ok(entry) => {
                            match sign_and_append(&io, &reports_path, entry, pid, &signing_key) {
                                Ok(()) => {
                                    processed_count += 1;
                                    compact_reports_file(&io, &reports_path);
                                }
                                Err(e) => {
                                    dropped_count += 1;
                                    last_error_message = Some(format!("failed to write signed report: {e:#}"));
                                }
                            }
                        }
                        Err(e) => {
                            // Malformed line — drop it and move on, same
                            // as the original (a corrupted/truncated
                            // queue line shouldn't wedge the whole
                            // daemon).
                            dropped_count += 1;
                            last_error_message = Some(format!("malformed queue entry: {e}"));
                        }
                    }
                }
            }
            _ = status_interval.tick() => {
                write_status(&io, &status_path, &StatusReport {
                    ok: true,
                    service: "error-reporter-daemon",
                    pid,
                    launched_as_separate_process,
                    started_at_ms,
                    updated_at_ms: now_unix_ms(),
                    queue_offset: tail.offset,
                    processed_count,
                    dropped_count,
                    last_error_message: last_error_message.clone(),
                });
            }
            _ = shutdown_signal() => {
                tracing::info!("error-reporter daemon: shutdown signal received");
                break;
            }
        }
    }

    // Final status flush on the way out — matches the original's
    // graceful-shutdown status write before exit.
    write_status(&io, &status_path, &StatusReport {
        ok: true,
        service: "error-reporter-daemon",
        pid,
        launched_as_separate_process,
        started_at_ms,
        updated_at_ms: now_unix_ms(),
        queue_offset: tail.offset,
        processed_count,
        dropped_count,
        last_error_message,
    });

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[derive(Default)]
struct TailState {
    offset: u64,
    partial: String,
}

/// Reads whatever's new in `queue_path` since the last call and
/// returns complete lines (each expected to be one JSON `QueueEntry`).
/// A line split across two polls (the writer appended mid-line right
/// as this read happened) is buffered in `state.partial` and completed
/// on a later call — not returned early as a broken line.
fn read_new_lines(queue_path: &Path, state: &mut TailState) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(queue_path) else {
        return Vec::new(); // doesn't exist yet — nothing to tail
    };

    let Ok(metadata) = file.metadata() else {
        return Vec::new();
    };
    let file_len = metadata.len();

    if file_len < state.offset {
        // Queue file shrank — externally truncated/rotated/replaced.
        // Matches the original's identical safety check: don't seek
        // past EOF, just start over.
        tracing::warn!(
            "error-reporter daemon: queue file shrank ({} -> {file_len} bytes) — resetting read offset to 0",
            state.offset
        );
        state.offset = 0;
        state.partial.clear();
    }

    if file_len == state.offset {
        return Vec::new(); // nothing new
    }

    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return Vec::new();
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }

    let Ok(text) = std::str::from_utf8(&buf) else {
        // Almost certainly read mid-write, slicing a multi-byte UTF-8
        // character at the chunk boundary — genuinely possible with a
        // concurrent writer, however rare. Deliberately does NOT
        // advance `state.offset` here, so the next poll re-reads from
        // the same point once the writer's append has fully landed,
        // rather than committing past bytes we couldn't even decode.
        tracing::warn!("error-reporter daemon: queue file chunk was not valid UTF-8 — will retry on next poll");
        return Vec::new();
    };

    // Only commit the offset advance once we've successfully decoded
    // the bytes — see the comment above.
    state.offset += buf.len() as u64;

    let combined = std::mem::take(&mut state.partial) + text;
    let ends_with_newline = combined.ends_with('\n');
    let parts: Vec<&str> = combined.split('\n').collect();
    let last_index = parts.len().saturating_sub(1);

    let mut lines = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i == last_index && !ends_with_newline {
            state.partial = part.to_string();
        } else if !part.is_empty() {
            lines.push(part.to_string());
        }
    }

    lines
}

fn sign_and_append(
    io: &atomic_io::AtomicIo,
    reports_path: &Path,
    entry: QueueEntry,
    daemon_pid: u32,
    signing_key: &str,
) -> anyhow::Result<()> {
    let processed_at_ms = now_unix_ms();
    let payload = IssuePayload {
        id: entry.id,
        ts_ms: entry.ts,
        iso: entry.iso,
        source: entry.source,
        level: entry.level,
        category: entry.category,
        message: entry.message,
        stack: entry.stack,
        submitted_by: SubmittedBy {
            pid: entry.pid,
            ppid: entry.ppid,
        },
        reported_by: ReportedBy {
            service: "error-reporter-daemon",
            pid: daemon_pid,
            processed_at_ms,
            processed_iso: iso_from_ms(processed_at_ms),
        },
    };

    let canonical = canonical_json(&payload)?;
    let signature = Signature {
        algo: "hmac-sha256",
        value: sign(&canonical, signing_key),
    };
    let signed = SignedIssueRecord { payload, signature };

    let mut line = serde_json::to_string(&signed)?;
    line.push('\n');
    io.append_locked(reports_path, line.as_bytes())?;
    Ok(())
}

/// JSON with object keys sorted alphabetically at every nesting level
/// — the original's `stableSortObject`, ported. Signing over a
/// canonical (deterministic-key-order) encoding matters here
/// specifically because [`IssuePayload`] gets serialized twice (once
/// to produce the string that's signed, once — via `SignedIssueRecord`
/// — to actually write the record with its signature attached); if key
/// order weren't pinned down, those two serializations only need to
/// stay consistent with EACH OTHER for the signature to still verify,
/// but pinning it to a fixed, sorted order also means a signature
/// computed by this exact scheme is reproducible independent of
/// serde's field-declaration-order default — the same property the
/// original's stable-sort was for.
fn canonical_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let mut json = serde_json::to_value(value)?;
    sort_keys(&mut json);
    Ok(serde_json::to_string(&json)?)
}

fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                sort_keys(v);
            }
            let sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::mem::take(map).into_iter().collect();
            *map = sorted.into_iter().collect();
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                sort_keys(v);
            }
        }
        _ => {}
    }
}

fn sign(canonical_json: &str, key: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(canonical_json.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn read_or_create_signing_key(io: &atomic_io::AtomicIo, admin_dir: &Path) -> anyhow::Result<String> {
    if let Ok(from_env) = std::env::var("ERROR_REPORT_SIGNING_KEY") {
        if from_env.len() >= 16 {
            return Ok(from_env);
        }
    }

    let key_path = admin_dir.join("error-reporter.key");

    if let Ok(existing) = std::fs::read_to_string(&key_path) {
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
        let mut perms = std::fs::metadata(&key_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&key_path, perms)?;
    }

    Ok(generated)
}

fn compact_reports_file(io: &atomic_io::AtomicIo, path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_REPORT_BYTES {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() <= MAX_REPORT_LINES {
        return;
    }
    let compacted = lines[lines.len() - MAX_REPORT_LINES..].join("\n") + "\n";
    let _ = io.write_atomic(path, compacted.as_bytes());
}

fn write_status(io: &atomic_io::AtomicIo, path: &Path, status: &StatusReport) {
    if let Ok(json) = serde_json::to_string_pretty(status) {
        let _ = io.write_atomic(path, json.as_bytes());
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Same minimal ISO-8601 formatting `error_client` uses for its own
/// timestamp — duplicated rather than shared because pulling in a
/// whole extra crate-boundary dependency for one ~15-line function
/// isn't worth it, and unlike the signing/dedup logic (where drift
/// between the two sides would be a real correctness bug), a
/// human-readable timestamp format drifting slightly wouldn't actually
/// break anything.
fn iso_from_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days_since_epoch = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (hour, minute, second) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (year, month, day) = civil_from_days(days_since_epoch as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

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
        let dir = std::env::temp_dir().join(format!("error-reporter-daemon-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_new_lines_returns_nothing_for_a_missing_file() {
        let dir = temp_dir("missing");
        let mut state = TailState::default();
        let lines = read_new_lines(&dir.join("does-not-exist.log"), &mut state);
        assert!(lines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_new_lines_reads_complete_lines_only() {
        let dir = temp_dir("complete-lines");
        let path = dir.join("queue.log");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let mut state = TailState::default();
        let lines = read_new_lines(&path, &mut state);
        assert_eq!(lines, vec!["line1".to_string(), "line2".to_string()]);
        assert_eq!(state.offset, 12); // "line1\nline2\n".len()
        assert!(state.partial.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_new_lines_buffers_a_partial_trailing_line() {
        let dir = temp_dir("partial-line");
        let path = dir.join("queue.log");
        std::fs::write(&path, "complete\nincomple").unwrap();

        let mut state = TailState::default();
        let lines = read_new_lines(&path, &mut state);
        assert_eq!(lines, vec!["complete".to_string()]);
        assert_eq!(state.partial, "incomple");

        // Now the writer finishes that line and adds another.
        std::fs::write(&path, "complete\nincomplete\nnext\n").unwrap();
        let more_lines = read_new_lines(&path, &mut state);
        assert_eq!(more_lines, vec!["incomplete".to_string(), "next".to_string()]);
        assert!(state.partial.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_new_lines_resets_offset_when_file_shrinks() {
        let dir = temp_dir("shrunk");
        let path = dir.join("queue.log");
        std::fs::write(&path, "aaaaaaaaaa\nbbbbbbbbbb\n").unwrap();

        let mut state = TailState::default();
        let _ = read_new_lines(&path, &mut state);
        assert!(state.offset > 0);

        // Simulate external truncation/rotation.
        std::fs::write(&path, "short\n").unwrap();
        let lines = read_new_lines(&path, &mut state);
        assert_eq!(lines, vec!["short".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        #[derive(Serialize)]
        struct Inner {
            zebra: i32,
            apple: i32,
        }
        #[derive(Serialize)]
        struct Outer {
            zoo: Inner,
            aardvark: i32,
        }
        let value = Outer {
            zoo: Inner { zebra: 1, apple: 2 },
            aardvark: 3,
        };
        let json = canonical_json(&value).unwrap();
        // "aardvark" should now precede "zoo" (top level), and within
        // "zoo", "apple" should precede "zebra" — both alphabetical,
        // neither matching the struct's declared field order.
        let aardvark_pos = json.find("aardvark").unwrap();
        let zoo_pos = json.find("zoo").unwrap();
        assert!(aardvark_pos < zoo_pos);
        let apple_pos = json.find("apple").unwrap();
        let zebra_pos = json.find("zebra").unwrap();
        assert!(apple_pos < zebra_pos);
    }

    #[test]
    fn same_payload_signs_identically_regardless_of_field_order_in_source() {
        // Two structurally-identical-but-differently-ordered JSON blobs
        // parsed to serde_json::Value should sign the same once
        // canonical_json sorts their keys — this is the actual property
        // the whole scheme depends on.
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let a_canon = canonical_json(&a).unwrap();
        let b_canon = canonical_json(&b).unwrap();
        assert_eq!(a_canon, b_canon);
        assert_eq!(sign(&a_canon, "test-key"), sign(&b_canon, "test-key"));
    }
}
