//! Issue categories and the keyword-based inference used when a
//! caller doesn't supply one explicitly — ported from the union of
//! `errorReporterClient.ts`'s `inferCategoryFromMessage` and
//! `errorReporterDaemon.ts`'s `inferCategory`.
//!
//! **One deliberate difference from the original:** those two
//! functions checked `security_error` and `node_runtime_error` in a
//! different order relative to each other (client: node_runtime_error
//! before security_error; daemon: security_error before
//! node_runtime_error) — an inconsistency in the original, not an
//! intentional design choice as far as I can tell reading it. Since
//! the client is what actually runs inference in normal operation
//! (the daemon's own inference is only a fallback for entries that
//! somehow reach the queue with a missing/invalid category), this
//! port uses ONE consistent order everywhere, matching the client's.
//! Replicating an apparent accident wouldn't be preserving a feature.
//!
//! Uses plain substring checks rather than the original's regexes —
//! no `regex` crate dependency for a handful of keyword checks, in
//! keeping with this project's "hand-roll it if it's small" rule.
//! This is a looser match than `\bword\b` boundaries (a message
//! containing "networked" would also match "network"), which is fine
//! here: this is a best-effort classification for a debugging/
//! monitoring log, not something anything downstream makes a security
//! or correctness decision based on.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCategory {
    TypescriptRuntimeError,
    NodeRuntimeError,
    /// Not one of the original Node backend's six categories — added
    /// because this crate now also serves the Rust backend's own panic
    /// hook (see [`crate::install_panic_hook`]), and a Rust panic is a
    /// meaningfully different failure mode from a JS `TypeError`;
    /// lumping it under `NodeRuntimeError` would actively mislead
    /// whoever's reading the signed log later.
    RustRuntimeError,
    NetworkError,
    SecurityError,
    OperationFailure,
    UnknownError,
}

pub(crate) fn infer(message: &str, stack: &str) -> IssueCategory {
    let merged = format!("{message} {stack}").to_lowercase();

    let any = |needles: &[&str]| needles.iter().any(|n| merged.contains(n));

    if any(&[
        "typescript",
        "ts-node",
        "tsc",
        "experimental-strip-types",
        "transpile",
    ]) {
        return IssueCategory::TypescriptRuntimeError;
    }
    if any(&["panicked", "panic"]) {
        return IssueCategory::RustRuntimeError;
    }
    if any(&[
        "econn", "enet", "dns", "socket", "network", "fetch", "axios", "timed out", "timeout",
    ]) {
        return IssueCategory::NetworkError;
    }
    if any(&[
        "typeerror",
        "referenceerror",
        "syntaxerror",
        "rangeerror",
        "err_",
    ]) {
        return IssueCategory::NodeRuntimeError;
    }
    if any(&["forbidden", "blocked", "denied", "security", "unauthor"]) {
        return IssueCategory::SecurityError;
    }
    if any(&["failed", "error", "exception", "reject"]) {
        return IssueCategory::OperationFailure;
    }

    IssueCategory::UnknownError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_typescript_before_anything_else() {
        assert_eq!(
            infer("TypeScript compilation failed with a network timeout", ""),
            IssueCategory::TypescriptRuntimeError,
            "typescript should win even when 'network' and 'failed' are also present"
        );
    }

    #[test]
    fn infers_network_from_common_keywords() {
        assert_eq!(infer("ECONNREFUSED talking to upstream", ""), IssueCategory::NetworkError);
        assert_eq!(infer("request timed out after 30s", ""), IssueCategory::NetworkError);
    }

    #[test]
    fn infers_node_runtime_error_from_js_error_names() {
        assert_eq!(
            infer("TypeError: cannot read property of undefined", ""),
            IssueCategory::NodeRuntimeError
        );
    }

    #[test]
    fn infers_security_from_keywords_when_no_earlier_category_matches() {
        assert_eq!(infer("request blocked: forbidden origin", ""), IssueCategory::SecurityError);
    }

    #[test]
    fn infers_rust_panic_before_operation_failure() {
        assert_eq!(
            infer("thread 'main' panicked at src/main.rs:10: index out of bounds", ""),
            IssueCategory::RustRuntimeError
        );
    }

    #[test]
    fn falls_back_to_operation_failure_then_unknown() {
        assert_eq!(infer("the operation failed", ""), IssueCategory::OperationFailure);
        assert_eq!(infer("something happened", ""), IssueCategory::UnknownError);
    }

    #[test]
    fn checks_stack_as_well_as_message() {
        assert_eq!(
            infer("generic wrapper error", "at Socket.connect (net.js:100)"),
            IssueCategory::NetworkError,
            "the network keyword only appears in the stack, not the message"
        );
    }
}
