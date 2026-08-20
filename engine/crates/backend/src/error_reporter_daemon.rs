//! Separate-process error reporter.
//!
//! The daemon tails the shared issue queue, signs each unique issue, and writes
//! a bounded signed report log. Its queue cursor is persisted so scheduled
//! process refreshes resume instead of replaying the whole queue. A bounded set
//! of recently signed issue IDs is also rebuilt from the report log at startup,
//! making refresh idempotent even if queue compaction changed byte offsets.

use std::collections::{BTreeMap, HashSet, VecDeque};
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
const RECENT_ID_CAPACITY: usize = 5_000;
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
    duplicate_count: u64,
    dropped_count: u64,
    last_error_message: Option<String>,
}

#[derive(Default)]
struct TailState {
    offset: u64,
    partial: String,
}

struct RecentIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentIds {
    fn new() -> Self {
        Self { set: HashSet::new(), order: VecDeque::new() }
    }

    fn contains(&self, id: &str) -> bool { self.set.contains(id) }

    fn insert(&mut self, id: String) {
        if !self.set.insert(id.clone()) { return; }
        self.order.push_back(id);
        while self.order.len() > RECENT_ID_CAPACITY {
            if let Some(oldest) = self.order.pop_front() { self.set.remove(&oldest); }
        }
    }
}

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

    let pid = std::process::id();
    let started_at_ms = now_unix_ms();
    let mut tail = TailState { offset: saved_queue_offset(&status_path, &queue_path), partial: String::new() };
    let mut recent_ids = load_recent_ids(&reports_path);
    let mut processed_count = 0u64;
    let mut duplicate_count = 0u64;
    let mut dropped_count = 0u64;
    let mut last_error_message: Option<String> = None;

    tracing::info!(
        pid,
        queue_path = %queue_path.display(),
        reports_path = %reports_path.display(),
        resume_offset = tail.offset,
        recent_signed_ids = recent_ids.set.len(),
        poll_interval_ms,
        "error-reporter daemon started"
    );

    let mut poll_interval = tokio::time::interval(Duration::from_millis(poll_interval_ms));
    let mut status_interval = tokio::time::interval(STATUS_FLUSH_INTERVAL);

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                for line in read_new_lines(&queue_path, &mut tail) {
                    match serde_json::from_str::<QueueEntry>(&line) {
                        Ok(entry) => {
                            if recent_ids.contains(&entry.id) {
                                duplicate_count = duplicate_count.saturating_add(1);
                                continue;
                            }
                            let id = entry.id.clone();
                            match sign_and_append(&io, &reports_path, entry, pid, &signing_key) {
                                Ok(()) => {
                                    processed_count = processed_count.saturating_add(1);
                                    recent_ids.insert(id);
                                    compact_reports_file(&io, &reports_path);
                                }
                                Err(error) => {
                                    dropped_count = dropped_count.saturating_add(1);
                                    last_error_message = Some(format!("failed to write signed report: {error:#}"));
                                }
                            }
                        }
                        Err(error) => {
                            dropped_count = dropped_count.saturating_add(1);
                            last_error_message = Some(format!("malformed queue entry: {error}"));
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
                    duplicate_count,
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

    write_status(&io, &status_path, &StatusReport {
        ok: true,
        service: "error-reporter-daemon",
        pid,
        launched_as_separate_process,
        started_at_ms,
        updated_at_ms: now_unix_ms(),
        queue_offset: tail.offset,
        processed_count,
        duplicate_count,
        dropped_count,
        last_error_message,
    });
    Ok(())
}

fn saved_queue_offset(status_path: &Path, queue_path: &Path) -> u64 {
    let saved = std::fs::read_to_string(status_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value.get("queue_offset").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    let queue_len = std::fs::metadata(queue_path).map(|metadata| metadata.len()).unwrap_or(0);
    if saved <= queue_len { saved } else { 0 }
}

fn load_recent_ids(reports_path: &Path) -> RecentIds {
    let mut ids = RecentIds::new();
    let Ok(raw) = std::fs::read_to_string(reports_path) else { return ids; };
    for line in raw.lines().rev().take(RECENT_ID_CAPACITY).collect::<Vec<_>>().into_iter().rev() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler"); };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

fn read_new_lines(queue_path: &Path, state: &mut TailState) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(queue_path) else { return Vec::new(); };
    let Ok(metadata) = file.metadata() else { return Vec::new(); };
    let file_len = metadata.len();

    if file_len < state.offset {
        tracing::warn!(old_offset = state.offset, file_len, "error-reporter queue shrank; resetting cursor");
        state.offset = 0;
        state.partial.clear();
    }
    if file_len == state.offset { return Vec::new(); }
    if file.seek(SeekFrom::Start(state.offset)).is_err() { return Vec::new(); }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() { return Vec::new(); }
    let Ok(text) = std::str::from_utf8(&buf) else {
        tracing::warn!("error-reporter queue chunk was not valid UTF-8; retrying next poll");
        return Vec::new();
    };
    state.offset = state.offset.saturating_add(buf.len() as u64);

    let combined = std::mem::take(&mut state.partial) + text;
    let ends_with_newline = combined.ends_with('\n');
    let parts = combined.split('\n').collect::<Vec<_>>();
    let last_index = parts.len().saturating_sub(1);
    let mut lines = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index == last_index && !ends_with_newline {
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
        submitted_by: SubmittedBy { pid: entry.pid, ppid: entry.ppid },
        reported_by: ReportedBy {
            service: "error-reporter-daemon",
            pid: daemon_pid,
            processed_at_ms,
            processed_iso: iso_from_ms(processed_at_ms),
        },
    };
    let canonical = canonical_json(&payload)?;
    let signature = Signature { algo: "hmac-sha256", value: sign(&canonical, signing_key) };
    let signed = SignedIssueRecord { payload, signature };
    let mut line = serde_json::to_string(&signed)?;
    line.push('\n');
    io.append_locked(reports_path, line.as_bytes())?;
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let mut json = serde_json::to_value(value)?;
    sort_keys(&mut json);
    Ok(serde_json::to_string(&json)?)
}

fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for value in map.values_mut() { sort_keys(value); }
            let sorted: BTreeMap<String, serde_json::Value> = std::mem::take(map).into_iter().collect();
            *map = sorted.into_iter().collect();
        }
        serde_json::Value::Array(items) => {
            for value in items { sort_keys(value); }
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
        if from_env.len() >= 16 { return Ok(from_env); }
    }
    let key_path = admin_dir.join("error-reporter.key");
    if let Ok(existing) = std::fs::read_to_string(&key_path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 16 { return Ok(trimmed.to_string()); }
    }

    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let generated = hex::encode(bytes);
    io.write_atomic(&key_path, generated.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&key_path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&key_path, permissions)?;
    }
    Ok(generated)
}

fn compact_reports_file(io: &atomic_io::AtomicIo, path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else { return; };
    if metadata.len() < MAX_REPORT_BYTES { return; }
    let Ok(raw) = std::fs::read_to_string(path) else { return; };
    let lines = raw.lines().filter(|line| !line.is_empty()).collect::<Vec<_>>();
    if lines.len() <= MAX_REPORT_LINES { return; }
    let compacted = lines[lines.len() - MAX_REPORT_LINES..].join("\n") + "\n";
    let _ = io.write_atomic(path, compacted.as_bytes());
}

fn write_status(io: &atomic_io::AtomicIo, path: &Path, status: &StatusReport) {
    if let Ok(json) = serde_json::to_string_pretty(status) { let _ = io.write_atomic(path, json.as_bytes()); }
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_millis() as u64).unwrap_or(0)
}

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
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
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
    fn persisted_offset_is_used_only_when_it_fits_current_queue() {
        let dir = temp_dir("offset");
        let queue = dir.join("queue.log");
        let status = dir.join("status.json");
        std::fs::write(&queue, "abcdef").unwrap();
        std::fs::write(&status, r#"{"queue_offset":4}"#).unwrap();
        assert_eq!(saved_queue_offset(&status, &queue), 4);
        std::fs::write(&status, r#"{"queue_offset":99}"#).unwrap();
        assert_eq!(saved_queue_offset(&status, &queue), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_ids_are_bounded_and_deduplicate() {
        let mut ids = RecentIds::new();
        ids.insert("same".into());
        ids.insert("same".into());
        assert_eq!(ids.order.len(), 1);
        for index in 0..(RECENT_ID_CAPACITY + 10) { ids.insert(format!("id-{index}")); }
        assert!(ids.order.len() <= RECENT_ID_CAPACITY);
    }

    #[test]
    fn read_new_lines_buffers_partial_tail() {
        let dir = temp_dir("partial");
        let path = dir.join("queue.log");
        std::fs::write(&path, "one\ntw").unwrap();
        let mut state = TailState::default();
        assert_eq!(read_new_lines(&path, &mut state), vec!["one".to_string()]);
        assert_eq!(state.partial, "tw");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(read_new_lines(&path, &mut state), vec!["two".to_string(), "three".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let value: serde_json::Value = serde_json::from_str(r#"{"z":{"b":1,"a":2},"a":3}"#).unwrap();
        let canonical = canonical_json(&value).unwrap();
        assert!(canonical.find("\"a\":3").unwrap() < canonical.find("\"z\"").unwrap());
        assert!(canonical.find("\"a\":2").unwrap() < canonical.find("\"b\":1").unwrap());
    }
}
