//! Build-time integrity binding for the standalone `container` dependency.
//!
//! The combined build compiles `container-bin` first and passes its exact
//! output through `RBE_CONTAINER_BIN_PATH`. This build script SHA-256 hashes
//! those exact bytes and generates a Rust constant inside `backend.exe`.
//!
//! Runtime startup then requires `dep/container(.exe)` to exist and to match
//! that compiled-in digest. No editable sidecar hash file is trusted.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_BIN_PATH");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is always set by cargo for build scripts");
    let embedded_dest = Path::new(&out_dir).join("embedded_container_bin");
    let integrity_dest = Path::new(&out_dir).join("container_integrity.rs");
    let source = std::env::var("RBE_CONTAINER_BIN_PATH").ok().map(PathBuf::from);

    let expected_hash = match source {
        Some(path) if path.is_file() => {
            println!("cargo:rerun-if-changed={}", path.display());

            // Keep the existing embedded artifact available to old build tooling,
            // but runtime startup no longer falls back to it.
            fs::copy(&path, &embedded_dest).unwrap_or_else(|err| {
                panic!(
                    "backend/build.rs: failed to copy container binary {} into OUT_DIR: {err}",
                    path.display()
                )
            });

            let hash = sha256_file(&path).unwrap_or_else(|err| {
                panic!(
                    "backend/build.rs: failed to SHA-256 container binary {}: {err}",
                    path.display()
                )
            });
            println!("cargo:warning=backend: binding container dependency SHA-256 {hash}");
            hash
        }
        Some(path) => {
            panic!(
                "backend/build.rs: RBE_CONTAINER_BIN_PATH was set to {} but the file does not exist — container dependency is required",
                path.display()
            );
        }
        None => {
            // Plain `cargo build -p backend` can still compile, but the resulting
            // backend will fail closed at startup because it has no bound container.
            String::new()
        }
    };

    let source_literal = format!("pub const EXPECTED_CONTAINER_SHA256: &str = \"{expected_hash}\";\n");
    fs::write(&integrity_dest, source_literal).unwrap_or_else(|err| {
        panic!("backend/build.rs: failed to write generated container integrity source: {err}")
    });
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}
