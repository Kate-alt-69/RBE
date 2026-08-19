//! Build-time integrity binding for the standalone `container` dependency.
//!
//! The combined build compiles `container-bin` first and passes its exact
//! output through `RBE_CONTAINER_BIN_PATH`. This build script SHA-256 hashes
//! those exact bytes, binds a Git/build identifier and target triple, and
//! signs the complete statement with the release-only Ed25519 private key.
//!
//! The resulting digest, build ID, target, public key and signature are
//! compiled into backend.exe. Runtime startup never trusts an editable
//! sidecar integrity file and does not embed a second copy of container.exe.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_BIN_PATH");
    println!("cargo:rerun-if-env-changed=RBE_CONTAINER_SIGNING_PRIVATE_KEY");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is always set by cargo for build scripts");
    let integrity_dest = Path::new(&out_dir).join("container_integrity.rs");
    let source = std::env::var("RBE_CONTAINER_BIN_PATH").ok().map(PathBuf::from);
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());
    let build_id = build_id();

    let (expected_hash, public_key, signature) = match source {
        Some(path) if path.is_file() => {
            println!("cargo:rerun-if-changed={}", path.display());

            let hash = sha256_file(&path).unwrap_or_else(|err| {
                panic!("backend/build.rs: failed to SHA-256 container binary {}: {err}", path.display())
            });

            let private_key_hex = std::env::var("RBE_CONTAINER_SIGNING_PRIVATE_KEY").unwrap_or_else(|_| {
                panic!("backend/build.rs: RBE_CONTAINER_SIGNING_PRIVATE_KEY is required when building a packaged container backend")
            });
            let private_key_bytes = hex::decode(private_key_hex.trim()).unwrap_or_else(|err| {
                panic!("backend/build.rs: RBE_CONTAINER_SIGNING_PRIVATE_KEY must be 32-byte hex: {err}")
            });
            let private_key: [u8; 32] = private_key_bytes.try_into().unwrap_or_else(|_| {
                panic!("backend/build.rs: RBE_CONTAINER_SIGNING_PRIVATE_KEY must contain exactly 32 bytes (64 hex characters)")
            });

            let signing_key = SigningKey::from_bytes(&private_key);
            let public_key = signing_key.verifying_key();
            let statement = signing_statement(&hash, &build_id, &target);
            let signature = signing_key.sign(statement.as_bytes());

            println!("cargo:warning=backend: binding container SHA-256 {hash}, build_id {build_id}, target {target}");
            (
                hash,
                hex::encode(public_key.to_bytes()),
                hex::encode(signature.to_bytes()),
            )
        }
        Some(path) => {
            panic!("backend/build.rs: RBE_CONTAINER_BIN_PATH was set to {} but the file does not exist — container dependency is required", path.display());
        }
        None => {
            // Plain `cargo build -p backend` can still compile, but the resulting
            // backend fails closed at startup because it has no bound container.
            (String::new(), String::new(), String::new())
        }
    };

    let source_literal = format!(
        "pub const EXPECTED_CONTAINER_SHA256: &str = \"{expected_hash}\";\n\
         pub const CONTAINER_BUILD_ID: &str = \"{build_id}\";\n\
         pub const CONTAINER_TARGET: &str = \"{target}\";\n\
         pub const CONTAINER_PUBLIC_KEY_HEX: &str = \"{public_key}\";\n\
         pub const CONTAINER_SIGNATURE_HEX: &str = \"{signature}\";\n"
    );
    fs::write(&integrity_dest, source_literal).unwrap_or_else(|err| {
        panic!("backend/build.rs: failed to write generated container integrity source: {err}")
    });
}

fn build_id() -> String {
    if let Ok(value) = std::env::var("RBE_BUILD_ID") {
        if !value.trim().is_empty() {
            return value;
        }
    }

    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown-build".to_string())
}

fn signing_statement(hash: &str, build_id: &str, target: &str) -> String {
    format!(
        "RBE-CONTAINER-INTEGRITY-V1\nsha256={hash}\nbuild_id={build_id}\ntarget={target}\n"
    )
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    // 64 KiB — NOT the 1 MiB this used to be. A 1 MiB LOCAL ARRAY
    // reliably blows the default thread stack (Windows threads default
    // to a 1 MiB stack, so a single such buffer consumed the entire
    // budget on its own; this was the actual STATUS_STACK_OVERFLOW
    // crash during the build). 64 KiB needs no heap allocation at all
    // and is already plenty efficient for sequential file hashing —
    // the bottleneck is disk I/O either way, not read() call count.
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}
