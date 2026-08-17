//! The embedded `container` binary extraction layer.
//!
//! `build.rs` embeds the standalone container binary when the build
//! orchestration has produced one. The backend extracts it to a cache path
//! and `container_process` owns the separate-process startup/lifetime.

use std::path::{Path, PathBuf};

static EMBEDDED_CONTAINER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded_container_bin"));

pub fn is_available() -> bool { !EMBEDDED_CONTAINER_BIN.is_empty() }

pub fn extract_if_needed(io: &atomic_io::AtomicIo, cache_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !is_available() { return Ok(None); }

    let file_name = if cfg!(windows) { "container.exe" } else { "container" };
    let dest = cache_dir.join(file_name);
    let needs_write = match io.read(&dest) {
        Ok(existing) => existing != EMBEDDED_CONTAINER_BIN,
        Err(_) => true,
    };

    if needs_write {
        io.write_atomic(&dest, EMBEDDED_CONTAINER_BIN)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)?;
        }
        tracing::info!(path = %dest.display(), bytes = EMBEDDED_CONTAINER_BIN.len(), "extracted embedded container binary");
    } else {
        tracing::debug!(path = %dest.display(), "embedded container binary already up to date on disk");
    }

    Ok(Some(dest))
}
