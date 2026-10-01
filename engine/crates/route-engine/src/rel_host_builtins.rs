//! Shared metadata for host-backed REL builtins.
//!
//! These definitions intentionally contain no execution logic. They let RELC,
//! analyzers, and host adapters agree on names/role policy while keeping actual
//! authority in Backend/Container.

use crate::source_registry::RelSourceKind;

pub const WORKSPACE_BUILTIN: &str = "workspace";
pub const SCRIPT_BUILTIN: &str = "script";
pub const ARCHIVE_BUILTIN: &str = "archive";

pub const WORKSPACE_FUNCTIONS: &[&str] = &[
    "temp",
    "construct",
    "fetch",
    "copy",
    "step",
    "after",
    "run",
];

pub const SCRIPT_FUNCTIONS: &[&str] = &[
    "run",
    "runPyPy",
    "run_pypy",
    "runRust",
    "run_rust",
];

pub const ARCHIVE_FUNCTIONS: &[&str] = &[
    "list",
    "read",
    "extract",
    "create",
    "replace",
    "remove",
];

pub fn is_host_builtin(name: &str) -> bool {
    matches!(name, WORKSPACE_BUILTIN | SCRIPT_BUILTIN | ARCHIVE_BUILTIN)
}

pub fn function_exists(module: &str, function: &str) -> bool {
    match module {
        WORKSPACE_BUILTIN => WORKSPACE_FUNCTIONS.contains(&function),
        SCRIPT_BUILTIN => SCRIPT_FUNCTIONS.contains(&function),
        ARCHIVE_BUILTIN => ARCHIVE_FUNCTIONS.contains(&function),
        _ => false,
    }
}

/// Host-backed filesystem/process capabilities are never legal in pure Field
/// REL. Server REL may declare them, although execution of Server helper calls
/// remains gated by the Server host bridge.
pub fn allowed_for_role(module: &str, kind: RelSourceKind) -> bool {
    is_host_builtin(module) && kind != RelSourceKind::Field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_rel_never_receives_host_builtins() {
        for name in [WORKSPACE_BUILTIN, SCRIPT_BUILTIN, ARCHIVE_BUILTIN] {
            assert!(!allowed_for_role(name, RelSourceKind::Field));
            assert!(allowed_for_role(name, RelSourceKind::Module));
            assert!(allowed_for_role(name, RelSourceKind::Service));
            assert!(allowed_for_role(name, RelSourceKind::Route));
            assert!(allowed_for_role(name, RelSourceKind::Server));
        }
    }
}
