//! Build script: optionally embeds a pre-built `container-bin` binary
//! into this crate at compile time, so `build.ps1`'s combined build can
//! ship ONE distributable `backend(.exe)` file that contains both the
//! main engine AND the sandboxed container binary's bytes — while
//! still keeping the container as a genuinely separate OS process at
//! *runtime* (see `engine/README.md`'s "non-negotiable exception" —
//! this embeds bytes at build time, it does not merge the processes).
//! `container_embed.rs` is what extracts those bytes back out to disk
//! and spawns them as a real child process later.
//!
//! Looks for a pre-built container binary at the path in the
//! `RBE_CONTAINER_BIN_PATH` environment variable, which `build.ps1`
//! sets when doing a combined build (see that script's comments for
//! the required build order: `container-bin` must be compiled BEFORE
//! this crate, since its output is what gets embedded here). If the
//! env var isn't set, or doesn't point at a real file, this writes an
//! empty placeholder instead of failing the build — plain `cargo build
//! -p backend` (normal day-to-day engine development, no PowerShell
//! orchestration involved) still works fine with no container
//! embedded. `container_embed::is_available()` checks the embedded
//! length at runtime and `main.rs` degrades gracefully either way.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_BIN_PATH");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is always set by cargo for build scripts");
    let dest = Path::new(&out_dir).join("embedded_container_bin");

    let source = std::env::var("RBE_CONTAINER_BIN_PATH").ok().map(PathBuf::from);

    match source {
        Some(path) if path.is_file() => {
            println!("cargo:rerun-if-changed={}", path.display());
            std::fs::copy(&path, &dest).unwrap_or_else(|e| {
                panic!(
                    "backend/build.rs: RBE_CONTAINER_BIN_PATH was set to {} but copying it into \
                     OUT_DIR failed: {e}",
                    path.display()
                )
            });
            println!("cargo:warning=backend: embedding container binary from {}", path.display());
        }
        Some(path) => {
            // Env var set but doesn't point at a real file — almost
            // certainly a build.ps1 orchestration mistake (wrong path,
            // or container-bin wasn't actually built first). Warn
            // loudly rather than silently shipping an engine binary
            // with no container support and no explanation why.
            println!(
                "cargo:warning=backend: RBE_CONTAINER_BIN_PATH={} does not exist — building \
                 WITHOUT an embedded container binary",
                path.display()
            );
            write_placeholder(&dest);
        }
        None => {
            // Normal standalone `cargo build -p backend` — expected,
            // not a warning.
            write_placeholder(&dest);
        }
    }
}

fn write_placeholder(dest: &Path) {
    std::fs::write(dest, []).unwrap_or_else(|e| {
        panic!("backend/build.rs: failed to write embedded-container placeholder: {e}")
    });
}
