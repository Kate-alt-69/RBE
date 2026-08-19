//! **Everything that needs "where does this process's data/cache live"
//! resolves relative to the compiled binary's own directory, never the
//! current working directory.** This is the one, single implementation
//! of that rule — see the docs on [`binary_dir`] for the full
//! reasoning (it was originally written as a doc comment on
//! `route_engine::paths`, then duplicated by hand into `container-bin`
//! when that process needed the same thing, and was about to be
//! duplicated a THIRD time into `security::timing` for the request-
//! audit log path).
//!
//! **That duplication is exactly the bug this crate fixes.** Every
//! copy was correct in isolation, but each one only fixes CWD-
//! relativity for the paths that particular file happens to build.
//! The moment any one call site got missed — or, worse, a brand new
//! subsystem hardcoded `"./data/admin/..."` directly instead of
//! reaching for the existing helper (exactly what happened with the
//! new `request.queue.log` path and the new `vault-process` daemon's
//! default `--data-dir`) — every OTHER process's `./data/admin`-style
//! path could point somewhere else entirely, depending on each
//! process's own CWD when it happened to be launched. That's the
//! actual, concrete failure mode: the transpiler cache "not working"
//! was never a caching-logic bug — it was every fresh boot resolving
//! `.cache/backend` to a *different* location than the boot before it,
//! so the hash-based staleness check never got a chance to see its own
//! previous output. Same root cause breaks the vault ACL file, the
//! error-reporter queue two separate processes need to agree on, and
//! now the request audit log — one shared crate, used everywhere,
//! is what actually closes this off instead of hoping every future
//! call site remembers to get it right by hand.

use std::path::PathBuf;

/// The directory the running binary lives in. Falls back to `.` (the
/// CWD) only if `current_exe()` itself fails, which is rare (some
/// unusual sandboxed/stripped environments) — better to degrade to
/// the old CWD-relative behavior than to fail boot entirely over a
/// path-resolution nicety.
pub fn binary_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The conventional shared admin directory (`<binary_dir>/data/admin`)
/// — vault's ACL/fallback store, the error-reporter queue and signed
/// log, the request audit log, all live here by convention. Exposed as
/// its own function (not just left as "call `binary_dir().join(...)`
/// yourself") specifically so every one of those subsystems resolves
/// the exact same `PathBuf` rather than four independently-correct-
/// looking `.join("data").join("admin")` call sites that could drift
/// if the convention ever changes.
pub fn default_admin_dir() -> PathBuf {
    binary_dir().join("data").join("admin")
}
