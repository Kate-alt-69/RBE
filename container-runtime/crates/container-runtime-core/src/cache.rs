use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::execution::WorkCost;

fn runtime_data_dir() -> PathBuf {
    runtime_paths::binary_dir().join("data").join("container-runtime")
}
fn artifact_dir() -> PathBuf { runtime_data_dir().join("artifacts") }
fn profile_dir() -> PathBuf { runtime_data_dir().join("profiles") }

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
    pub fn average_ms(&self) -> f64 { if self.samples == 0 { 0.0 } else { self.total_ms as f64 / self.samples as f64 } }
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
                if path.extension().and_then(|value| value.to_str()) != Some("json") { continue; }
                let Some(hash) = path.file_stem().and_then(|value| value.to_str()) else { continue; };
                if !valid_artifact_name(hash) { continue; }
                let Some(profile) = io.read(&path).ok().and_then(|bytes| serde_json::from_slice::<ExecutionProfile>(&bytes).ok()) else { continue; };
                profiles.insert(hash.to_string(), profile);
            }
        }
        Self { profiles: Mutex::new(profiles), artifacts: Mutex::new(HashMap::new()), io }
    }
}

impl ArtifactCache {
    pub fn record(&self, artifact_hash: &str, elapsed_ms: u64, declared_cost: WorkCost) {
        if !valid_artifact_name(artifact_hash) { return; }
        let profile = {
            let mut profiles = self.profiles.lock().expect("artifact cache poisoned");
            let profile = profiles.entry(artifact_hash.to_string()).or_default();
            profile.record(elapsed_ms, declared_cost);
            profile.clone()
        };
        if let Ok(bytes) = serde_json::to_vec(&profile) {
            let dir = profile_dir();
            if fs::create_dir_all(&dir).is_ok() {
                let _ = self.io.write_atomic(&dir.join(format!("{artifact_hash}.json")), &bytes);
            }
        }
    }

    pub fn profile(&self, artifact_hash: &str) -> Option<ExecutionProfile> {
        self.profiles.lock().expect("artifact cache poisoned").get(artifact_hash).cloned()
    }

    pub fn profiles(&self) -> Vec<(String, ExecutionProfile)> {
        let mut profiles = self.profiles.lock().expect("artifact cache poisoned").iter().map(|(hash, profile)| (hash.clone(), profile.clone())).collect::<Vec<_>>();
        profiles.sort_by(|a, b| a.0.cmp(&b.0));
        profiles
    }

    pub fn put_artifact(&self, artifact_hash: impl Into<String>, wasm: Vec<u8>) {
        let artifact_hash = artifact_hash.into();
        self.artifacts.lock().expect("artifact cache poisoned").insert(artifact_hash.clone(), wasm.clone());
        if valid_artifact_name(&artifact_hash) {
            let dir = artifact_dir();
            if fs::create_dir_all(&dir).is_ok() {
                let _ = self.io.write_atomic(&dir.join(format!("{artifact_hash}.wasm")), &wasm);
            }
        }
    }

    pub fn contains_artifact(&self, artifact_hash: &str) -> bool {
        if self.artifacts.lock().expect("artifact cache poisoned").contains_key(artifact_hash) {
            return true;
        }
        valid_artifact_name(artifact_hash) && artifact_dir().join(format!("{artifact_hash}.wasm")).is_file()
    }

    pub fn artifact(&self, artifact_hash: &str) -> Option<Vec<u8>> {
        if let Some(bytes) = self.artifacts.lock().expect("artifact cache poisoned").get(artifact_hash).cloned() { return Some(bytes); }
        if !valid_artifact_name(artifact_hash) { return None; }
        let bytes = fs::read(artifact_dir().join(format!("{artifact_hash}.wasm"))).ok()?;
        self.artifacts.lock().expect("artifact cache poisoned").insert(artifact_hash.to_string(), bytes.clone());
        Some(bytes)
    }

    pub fn len(&self) -> usize { self.profiles.lock().expect("artifact cache poisoned").len() }
    pub fn is_empty(&self) -> bool { self.profiles.lock().expect("artifact cache poisoned").is_empty() }
    pub fn artifact_count(&self) -> usize {
        let memory = self.artifacts.lock().expect("artifact cache poisoned").len();
        let disk = fs::read_dir(artifact_dir()).map(|entries| entries.flatten().filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("wasm")).count()).unwrap_or(0);
        memory.max(disk)
    }
}

fn valid_artifact_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
