# error-client

Standalone crate — like `vault`/`atomic-io`, not a member of either
the `engine` or `container-runtime` workspace. This is the writer side
of a two-process issue-reporting system, ported from the original Node
backend's `errorReporterClient.ts` / `errorReporterDaemon.ts` pair.

**The two processes:**

- **Any process** (main engine, a container-runtime environment,
  eventually a WASM sandbox host) depends on this crate and calls
  `error_client::report_issue(...)`. It normalizes the input, dedupes
  near-identical reports from the same process within a short window,
  and appends one JSON line to `<admin_dir>/error-report.queue.log` via
  the shared `atomic-io` crate. Never panics, never returns an error to
  the caller — a failure to report an issue has nowhere further to go.
- **`backend.exe --er --launch`** — a genuinely separate OS process
  (see `engine/crates/backend/src/error_reporter_daemon.rs`), spawned
  as a child of the main engine at boot. Polls that same queue file,
  signs each new entry (HMAC-SHA256 over a stable-key-sorted canonical
  JSON encoding), and appends the signed record to
  `<admin_dir>/error-reports.log`, with a status file at
  `<admin_dir>/error-reporter-status.json`.

Both sides agree on the file paths and the `QueueEntry` wire format —
nothing else couples them. That's deliberate: it's what lets "report an
issue" be usable from anywhere in the system with almost no dependency
weight, while the actual signing/processing logic lives in exactly one
place.

See this crate's `lib.rs` doc comment for why this changed from an
earlier in-process (channel-based) version, and
`error_reporter_daemon.rs`'s doc comment for what's faithfully carried
over from the original Node daemon (including one known
characteristic — possible duplicate signed entries after a daemon
restart — inherited from the original design, not a new bug).
