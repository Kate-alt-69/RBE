use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution::WorkCost;

fn runtime_data_dir() -> PathBuf {
    runtime_paths::binary_dir()
        .join("data")
        .join("container-runtime")
}
fn artifact_dir() -> PathBuf {
    runtime_data_dir().join("artifacts")
}
fn profile_dir() -> PathBuf {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub samples: u64,
    pub total_ms: u64,
    pub last_ms: u64,
    pub max_ms: u64,
    pub declared_cost: WorkCost,
}

impl ExecutionProfile {
    pub fn record(&mut self, elapsed_ms: u64, declared_cost: WorkCost) {
        self.samples = self.samples.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(elapsed_ms);
        self.last_ms = elapsed_ms;
        self.max_ms = self.max_ms.max(elapsed_ms);
        self.declared_cost = declared_cost;
    }
    pub fn average_ms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.total_ms as f64 / self.samples as f64
        }
    }
}

pub struct ArtifactCache {
    profiles: Mutex<HashMap<String, ExecutionProfile>>,
    artifacts: Mutex<HashMap<String, Vec<u8>>>,
    io: atomic_io::AtomicIo,
}

impl Default for ArtifactCache {
    fn default() -> Self {
        let io = atomic_io::AtomicIo::new();
        let mut profiles = HashMap::new();
        if let Ok(entries) = fs::read_dir(profile_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some(hash) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !valid_artifact_name(hash) {
                    continue;
                }
                let Some(profile) = io
                    .read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<ExecutionProfile>(&bytes).ok())
                else {
                    continue;
                };
                profiles.insert(hash.to_string(), profile);
            }
        }
        Self {
            profiles: Mutex::new(profiles),
            artifacts: Mutex::new(HashMap::new()),
            io,
        }
    }
}

impl ArtifactCache {
    pub fn record(&self, artifact_hash: &str, elapsed_ms: u64, declared_cost: WorkCost) {
        if !valid_artifact_name(artifact_hash) {
            return;
        }
        let profile = {
            let mut profiles = self.profiles.lock().expect("artifact cache poisoned");
            let profile = profiles.entry(artifact_hash.to_string()).or_default();
            profile.record(elapsed_ms, declared_cost);
            profile.clone()
        };
        if let Ok(bytes) = serde_json::to_vec(&profile) {
            let dir = profile_dir();
            if fs::create_dir_all(&dir).is_ok() {
                let _ = self
                    .io
                    .write_atomic(&dir.join(format!("{artifact_hash}.json")), &bytes);
            }
        }
    }

    pub fn profile(&self, artifact_hash: &str) -> Option<ExecutionProfile> {
        self.profiles
            .lock()
            .expect("artifact cache poisoned")
            .get(artifact_hash)
            .cloned()
    }

    pub fn profiles(&self) -> Vec<(String, ExecutionProfile)> {
        let mut profiles = self
            .profiles
            .lock()
            .expect("artifact cache poisoned")
            .iter()
            .map(|(hash, profile)| (hash.clone(), profile.clone()))
            .collect::<Vec<_>>();
        profiles.sort_by(|a, b| a.0.cmp(&b.0));
        profiles
    }

    pub fn put_artifact(
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
    }

    pub fn len(&self) -> usize {
        self.profiles.lock().expect("artifact cache poisoned").len()
    }
    pub fn is_empty(&self) -> bool {
        self.profiles
            .lock()
            .expect("artifact cache poisoned")
            .is_empty()
    }
    pub fn artifact_count(&self) -> usize {
        let memory = self
            .artifacts
            .lock()
            .expect("artifact cache poisoned")
            .len();
        let disk = fs::read_dir(artifact_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry.path().extension().and_then(|value| value.to_str()) == Some("wasm")
                    })
                    .count()
            })
            .unwrap_or(0);
        memory.max(disk)
    }
}

fn valid_artifact_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

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
