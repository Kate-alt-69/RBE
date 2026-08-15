//! The container binary bytes `build.rs` embedded, if any — see that
//! file's doc comment for exactly when this is/isn't populated.
//!
//! This module only extracts the bytes back to disk; it doesn't spawn
//! the container process. Nothing in this codebase spawns it yet —
//! that needs the IPC protocol (`container-runtime/crates/ipc-protocol`,
//! still a stub) to actually have something to hand the child process
//! over. This exists now so the embedding mechanism itself is real and
//! testable ahead of that, matching the original plan's design: "the
//! compiled container binary embedded... falling back to extracting to
//! `.cache/service/container.exe` only if needed."

use std::path::{Path, PathBuf};

static EMBEDDED_CONTAINER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded_container_bin"));

/// Whether `build.rs` actually embedded a container binary. `false`
/// for a plain `cargo build -p backend` with no `build.ps1`
/// orchestration — that's a normal, supported dev-workflow state, not
/// an error.
pub fn is_available() -> bool {
    !EMBEDDED_CONTAINER_BIN.is_empty()
}

/// Extracts the embedded container binary into `cache_dir` (creating
/// it if needed) unless a file already there is byte-identical —
/// re-extracting a multi-megabyte binary on every single boot would be
/// wasted I/O for the overwhelmingly common case where it hasn't
/// changed since last time. Returns the path it's available at, or
/// `None` if nothing was embedded (see [`is_available`]).
pub fn extract_if_needed(
    io: &atomic_io::AtomicIo,
    cache_dir: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    if !is_available() {
        return Ok(None);
    }

    let file_name = if cfg!(windows) { "container.exe" } else { "container" };
    let dest = cache_dir.join(file_name);

    let needs_write = match io.read(&dest) {
        Ok(existing) => existing != EMBEDDED_CONTAINER_BIN,
        Err(_) => true, // doesn't exist yet (or unreadable) — write it
    };

    if needs_write {
        io.write_atomic(&dest, EMBEDDED_CONTAINER_BIN)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)?.permissions();
            perms.set_mode(0o755); // extracted binary needs +x on unix — write_atomic's
                                    // temp-file-then-rename doesn't preserve an executable
                                    // bit that was never there to begin with.
            std::fs::set_permissions(&dest, perms)?;
        }

        tracing::info!(path = %dest.display(), bytes = EMBEDDED_CONTAINER_BIN.len(), "extracted embedded container binary");
    } else {
        tracing::debug!(path = %dest.display(), "embedded container binary already up to date on disk");
    }

    Ok(Some(dest))
}
