from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


cache = Path("container-runtime/crates/container-runtime-core/src/cache.rs")
replace_once(
    cache,
    '''use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};''',
    '''use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};''',
    "cache integrity imports",
)
replace_once(
    cache,
    '''fn profile_dir() -> PathBuf {
    runtime_data_dir().join("profiles")
}
''',
    '''fn profile_dir() -> PathBuf {
    runtime_data_dir().join("profiles")
}

/// Verify that executable bytes still match their content-addressed identity.
/// This check is intentionally shared with the standalone worker so disk cache
/// contents are never trusted merely because their filename looks correct.
pub fn artifact_sha256_matches(artifact_hash: &str, wasm: &[u8]) -> bool {
    artifact_hash.len() == 64
        && artifact_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        && hex::encode(Sha256::digest(wasm)) == artifact_hash
}

fn quarantine_corrupt_artifact(path: &Path, artifact_hash: &str) {
    let quarantine = artifact_dir().join("quarantine");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    if fs::create_dir_all(&quarantine).is_ok() {
        let target = quarantine.join(format!(
            "{artifact_hash}.corrupt-{}-{stamp}.wasm",
            std::process::id()
        ));
        if fs::rename(path, target).is_ok() {
            return;
        }
    }
    // A corrupt executable must never remain at the authoritative hash path.
    let _ = fs::remove_file(path);
}
''',
    "cache integrity helpers",
)
replace_once(
    cache,
    '''    pub fn put_artifact(&self, artifact_hash: impl Into<String>, wasm: Vec<u8>) {
        let artifact_hash = artifact_hash.into();
        self.artifacts
            .lock()
            .expect("artifact cache poisoned")
            .insert(artifact_hash.clone(), wasm.clone());
        if valid_artifact_name(&artifact_hash) {
            let dir = artifact_dir();
            if fs::create_dir_all(&dir).is_ok() {
                let _ = self
                    .io
                    .write_atomic(&dir.join(format!("{artifact_hash}.wasm")), &wasm);
            }
        }
    }

    pub fn contains_artifact(&self, artifact_hash: &str) -> bool {
        if self
            .artifacts
            .lock()
            .expect("artifact cache poisoned")
            .contains_key(artifact_hash)
        {
            return true;
        }
        valid_artifact_name(artifact_hash)
            && artifact_dir()
                .join(format!("{artifact_hash}.wasm"))
                .is_file()
    }

    pub fn artifact(&self, artifact_hash: &str) -> Option<Vec<u8>> {
        if let Some(bytes) = self
            .artifacts
            .lock()
            .expect("artifact cache poisoned")
            .get(artifact_hash)
            .cloned()
        {
            return Some(bytes);
        }
        if !valid_artifact_name(artifact_hash) {
            return None;
        }
        let bytes = fs::read(artifact_dir().join(format!("{artifact_hash}.wasm"))).ok()?;
        self.artifacts
            .lock()
            .expect("artifact cache poisoned")
            .insert(artifact_hash.to_string(), bytes.clone());
        Some(bytes)
    }''',
    '''    pub fn put_artifact(
        &self,
        artifact_hash: impl Into<String>,
        wasm: Vec<u8>,
    ) -> Result<(), String> {
        let artifact_hash = artifact_hash.into();
        if !artifact_sha256_matches(&artifact_hash, &wasm) {
            return Err("artifact bytes do not match their SHA-256 identity".into());
        }
        let dir = artifact_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| format!("create artifact cache directory: {error}"))?;
        self.io
            .write_atomic(&dir.join(format!("{artifact_hash}.wasm")), &wasm)
            .map_err(|error| format!("persist artifact atomically: {error}"))?;
        self.artifacts
            .lock()
            .expect("artifact cache poisoned")
            .insert(artifact_hash, wasm);
        Ok(())
    }

    pub fn contains_artifact(&self, artifact_hash: &str) -> bool {
        if artifact_hash.len() != 64
            || !artifact_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return false;
        }
        let path = artifact_dir().join(format!("{artifact_hash}.wasm"));
        let Ok(bytes) = fs::read(&path) else {
            self.artifacts
                .lock()
                .expect("artifact cache poisoned")
                .remove(artifact_hash);
            return false;
        };
        if !artifact_sha256_matches(artifact_hash, &bytes) {
            self.artifacts
                .lock()
                .expect("artifact cache poisoned")
                .remove(artifact_hash);
            quarantine_corrupt_artifact(&path, artifact_hash);
            return false;
        }
        self.artifacts
            .lock()
            .expect("artifact cache poisoned")
            .insert(artifact_hash.to_string(), bytes);
        true
    }

    pub fn artifact(&self, artifact_hash: &str) -> Option<Vec<u8>> {
        if !self.contains_artifact(artifact_hash) {
            return None;
        }
        self.artifacts
            .lock()
            .expect("artifact cache poisoned")
            .get(artifact_hash)
            .cloned()
    }''',
    "durable verified artifact cache",
)
# Add deterministic helper-level integrity tests without depending on the
# process-global runtime data directory.
text = cache.read_text(encoding="utf-8")
text += '''

#[cfg(test)]
mod integrity_tests {
    use super::*;

    #[test]
    fn artifact_identity_detects_mutated_bytes() {
        let bytes = b"rbe-artifact";
        let hash = hex::encode(Sha256::digest(bytes));
        assert!(artifact_sha256_matches(&hash, bytes));
        assert!(!artifact_sha256_matches(&hash, b"rbe-artifacu"));
        assert!(!artifact_sha256_matches("not-a-sha256", bytes));
    }
}
'''
cache.write_text(text, encoding="utf-8")

runtime = Path("container-runtime/crates/container-runtime-core/src/runtime.rs")
replace_once(
    runtime,
    '''        if !already_present {
            self.cache.put_artifact(computed, wasm);
        }
        Ok(already_present)''',
    '''        if !already_present {
            self.cache.put_artifact(computed, wasm)?;
        }
        Ok(already_present)''',
    "durable registration failure propagation",
)

lib = Path("container-runtime/crates/container-runtime-core/src/lib.rs")
replace_once(
    lib,
    '''pub use cache::{ArtifactCache, ExecutionProfile};''',
    '''pub use cache::{artifact_sha256_matches, ArtifactCache, ExecutionProfile};''',
    "artifact verifier export",
)

worker = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    worker,
    '''use container_runtime_core::{
    CapabilityBroker, EnvironmentId, EnvironmentRegistry, ExecutionProvenance, Runtime,
    RuntimeConfig, WorkCost,
};''',
    '''use container_runtime_core::{
    artifact_sha256_matches, CapabilityBroker, EnvironmentId, EnvironmentRegistry,
    ExecutionProvenance, Runtime, RuntimeConfig, WorkCost,
};''',
    "worker verifier import",
)
replace_once(
    worker,
    '''    let wasm =
        fs::read(path).map_err(|e| anyhow::anyhow!("worker: failed to read artifact: {e}"))?;
    let fuel = value_after(args, "--fuel")''',
    '''    let wasm =
        fs::read(path).map_err(|e| anyhow::anyhow!("worker: failed to read artifact: {e}"))?;
    if !artifact_sha256_matches(&artifact, &wasm) {
        anyhow::bail!("worker: artifact SHA-256 integrity check failed");
    }
    let fuel = value_after(args, "--fuel")''',
    "worker independent artifact verification",
)

doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Those bytes are the eligible payload for Container artifact registration.''',
    '''For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Those bytes are the eligible payload for Container artifact registration. Artifact persistence is durable-or-fail, cached WASM is SHA-256 verified again when reloaded from disk, corrupt hash-path files are quarantined/removed, and the isolated worker independently re-hashes bytes immediately before execution.''',
    "artifact integrity documentation",
)
