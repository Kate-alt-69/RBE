//! Path resolution shared across the `.route`/`.module` system.
//!
//! **Everything here resolves relative to the compiled binary's own
//! directory, never the current working directory.** This matters in
//! practice: `backend.exe` (or the Linux binary) is meant to be
//! launched from anywhere — a shortcut, a service manager, a
//! different shell's CWD — and still find its sibling `api/` and
//! `module/` folders. CWD-relative resolution would break the moment
//! someone runs it from somewhere other than right next to those
//! folders. This is also exactly what `./` is documented to mean for
//! `.route`/`.module` imports (see `api/README.md`): "wherever the
//! compiled binary runs from," not "wherever the shell happened to be
//! `cd`'d to."

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

/// The default `/api/` folder — a sibling of the binary.
pub fn default_api_dir() -> PathBuf {
    binary_dir().join("api")
}

/// The default `/module/` folder — a sibling of the binary, and what
/// the `module&name` import shorthand resolves against (see
/// `parser::Parser::classify_bareword_import`).
pub fn default_module_dir() -> PathBuf {
    binary_dir().join("module")
}

/// Resolves a `.route`/`.module` custom import path (already
/// normalized to a `./`-prefixed form, whether written that way
/// directly or desugared from `module&name`) against the binary's
/// directory. `.module` extension is added if the path doesn't
/// already have one.
pub fn resolve_custom_import(root: &std::path::Path, raw_path: &str) -> PathBuf {
    let relative = raw_path.strip_prefix("./").unwrap_or(raw_path);
    let joined = root.join(relative);
    if joined.extension().is_some() {
        joined
    } else {
        joined.with_extension("module")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_to_given_root_not_cwd() {
        let root = PathBuf::from("/some/binary/dir");
        let resolved = resolve_custom_import(&root, "./module/storage");
        assert_eq!(
            resolved,
            PathBuf::from("/some/binary/dir/module/storage.module")
        );
    }

    #[test]
    fn leaves_existing_extension_alone() {
        let root = PathBuf::from("/some/binary/dir");
        let resolved = resolve_custom_import(&root, "./module/storage.module");
        assert_eq!(
            resolved,
            PathBuf::from("/some/binary/dir/module/storage.module")
        );
    }
}
