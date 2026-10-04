//! Non-authoritative per-Task execution telemetry.
//!
//! Profiles are keyed by `(RuntimeImage, TaskOID)`, never by the backing WASM
//! artifact. They may influence scheduling hints but are never authority: a
//! missing, corrupt or unwritable profile cache cannot block valid execution.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::execution::WorkCost;

pub const TASK_PROFILE_VERSION: u16 = 1;
const LATENCY_BUCKET_MS: [u64; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_000, 2_000, 5_000, 10_000, 30_000, 60_000,
];

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TaskProfileKey {
    pub runtime_image_sha256: [u8; 32],
    pub task_oid: u16,
}

impl TaskProfileKey {
    pub const fn new(runtime_image_sha256: [u8; 32], task_oid: u16) -> Self {
        Self {
            runtime_image_sha256,
            task_oid,
        }
    }

    pub fn runtime_image_hex(self) -> String {
        hex::encode(self.runtime_image_sha256)
    }

    fn file_name(self) -> String {
        format!("{}-{}.json", self.runtime_image_hex(), self.task_oid)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskExecutionSample {
    pub elapsed_ms: u64,
    pub succeeded: bool,
    /// Observed resource high-water values for this execution. These are
    /// telemetry, not admission limits.
    pub resource_high_water: WorkCost,
    pub arena_high_water_bytes: u64,
    pub declared_cost: WorkCost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskExecutionProfile {
    pub version: u16,
    pub samples: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub total_ms: u64,
    pub last_ms: u64,
    pub max_ms: u64,
    pub latency_histogram: [u64; LATENCY_BUCKET_MS.len()],
    pub latency_overflow: u64,
    pub resource_high_water: WorkCost,
    pub arena_high_water_bytes: u64,
    pub declared_cost: WorkCost,
}

impl Default for TaskExecutionProfile {
    fn default() -> Self {
        Self {
            version: TASK_PROFILE_VERSION,
            samples: 0,
            succeeded: 0,
            failed: 0,
            total_ms: 0,
            last_ms: 0,
            max_ms: 0,
            latency_histogram: [0; LATENCY_BUCKET_MS.len()],
            latency_overflow: 0,
            resource_high_water: WorkCost::default(),
            arena_high_water_bytes: 0,
            declared_cost: WorkCost::default(),
        }
    }
}

impl TaskExecutionProfile {
    pub fn record(&mut self, sample: TaskExecutionSample) {
        self.samples = self.samples.saturating_add(1);
        if sample.succeeded {
            self.succeeded = self.succeeded.saturating_add(1);
        } else {
            self.failed = self.failed.saturating_add(1);
        }
        self.total_ms = self.total_ms.saturating_add(sample.elapsed_ms);
        self.last_ms = sample.elapsed_ms;
        self.max_ms = self.max_ms.max(sample.elapsed_ms);
        match LATENCY_BUCKET_MS
            .iter()
            .position(|upper| sample.elapsed_ms <= *upper)
        {
            Some(index) => {
                self.latency_histogram[index] =
                    self.latency_histogram[index].saturating_add(1);
            }
            None => self.latency_overflow = self.latency_overflow.saturating_add(1),
        }
        self.resource_high_water.cpu = self
            .resource_high_water
            .cpu
            .max(sample.resource_high_water.cpu);
        self.resource_high_water.memory = self
            .resource_high_water
            .memory
            .max(sample.resource_high_water.memory);
        self.resource_high_water.io = self
            .resource_high_water
            .io
            .max(sample.resource_high_water.io);
        self.resource_high_water.network = self
            .resource_high_water
            .network
            .max(sample.resource_high_water.network);
        self.arena_high_water_bytes = self
            .arena_high_water_bytes
            .max(sample.arena_high_water_bytes);
        self.declared_cost = sample.declared_cost;
    }

    pub fn average_ms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.total_ms as f64 / self.samples as f64
        }
    }

    /// Returns the upper bound of the histogram bucket containing p95.
    /// Overflow samples conservatively report a value above the largest bucket.
    pub fn p95_ms(&self) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let rank = self.samples.saturating_mul(95).saturating_add(99) / 100;
        let mut seen = 0u64;
        for (index, count) in self.latency_histogram.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return LATENCY_BUCKET_MS[index];
            }
        }
        LATENCY_BUCKET_MS
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Optional, non-authoritative Swamp cost hint. Runtime admission and
    /// capability policy must never trust this value as a security boundary.
    pub fn scheduler_hint(&self, fallback: WorkCost) -> WorkCost {
        if self.samples == 0 {
            return fallback;
        }
        WorkCost {
            cpu: fallback.cpu.max(self.declared_cost.cpu).max(self.p95_ms()),
            memory: fallback
                .memory
                .max(self.declared_cost.memory)
                .max(self.resource_high_water.memory)
                .max(self.arena_high_water_bytes),
            io: fallback
                .io
                .max(self.declared_cost.io)
                .max(self.resource_high_water.io),
            network: fallback
                .network
                .max(self.declared_cost.network)
                .max(self.resource_high_water.network),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTaskProfile {
    version: u16,
    key: TaskProfileKey,
    profile: TaskExecutionProfile,
}

pub struct TaskExecutionProfileStore {
    root: PathBuf,
    profiles: Mutex<BTreeMap<TaskProfileKey, TaskExecutionProfile>>,
    io: atomic_io::AtomicIo,
}

impl Default for TaskExecutionProfileStore {
    fn default() -> Self {
        Self::new(
            runtime_paths::binary_dir()
                .join("data")
                .join("container-runtime")
                .join("task-profiles"),
        )
    }
}

impl TaskExecutionProfileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let io = atomic_io::AtomicIo::new();
        let profiles = load_profiles(&root, &io);
        Self {
            root,
            profiles: Mutex::new(profiles),
            io,
        }
    }

    pub fn record(
        &self,
        key: TaskProfileKey,
        sample: TaskExecutionSample,
    ) -> TaskExecutionProfile {
        let profile = {
            let mut profiles = self.lock_profiles();
            let profile = profiles.entry(key).or_default();
            profile.record(sample);
            profile.clone()
        };
        // Telemetry persistence is best-effort by design. Execution has already
        // succeeded or failed independently of whether this write is possible.
        let _ = self.persist(key, &profile);
        profile
    }

    pub fn profile(&self, key: TaskProfileKey) -> Option<TaskExecutionProfile> {
        self.lock_profiles().get(&key).cloned()
    }

    pub fn profiles(&self) -> Vec<(TaskProfileKey, TaskExecutionProfile)> {
        self.lock_profiles()
            .iter()
            .map(|(key, profile)| (*key, profile.clone()))
            .collect()
    }

    pub fn scheduler_hint(&self, key: TaskProfileKey, fallback: WorkCost) -> WorkCost {
        self.profile(key)
            .map_or(fallback, |profile| profile.scheduler_hint(fallback))
    }

    fn persist(&self, key: TaskProfileKey, profile: &TaskExecutionProfile) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .map_err(|error| format!("create Task profile directory: {error}"))?;
        let record = PersistedTaskProfile {
            version: TASK_PROFILE_VERSION,
            key,
            profile: profile.clone(),
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|error| format!("serialize Task profile: {error}"))?;
        self.io
            .write_atomic(&self.root.join(key.file_name()), &bytes)
            .map_err(|error| format!("persist Task profile atomically: {error}"))
    }

    fn lock_profiles(&self) -> MutexGuard<'_, BTreeMap<TaskProfileKey, TaskExecutionProfile>> {
        match self.profiles.lock() {
            Ok(profiles) => profiles,
            Err(poisoned) => {
                // Profiles are non-authoritative telemetry. Recover from a
                // poisoned lock by discarding in-memory state, never by failing
                // an otherwise valid Task execution.
                let mut profiles = poisoned.into_inner();
                profiles.clear();
                self.profiles.clear_poison();
                profiles
            }
        }
    }
}

fn load_profiles(
    root: &Path,
    io: &atomic_io::AtomicIo,
) -> BTreeMap<TaskProfileKey, TaskExecutionProfile> {
    let mut profiles = BTreeMap::new();
    let Ok(entries) = fs::read_dir(root) else {
        return profiles;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(record) = io
            .read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedTaskProfile>(&bytes).ok())
        else {
            continue;
        };
        if record.version != TASK_PROFILE_VERSION
            || record.profile.version != TASK_PROFILE_VERSION
            || path.file_name().and_then(|name| name.to_str())
                != Some(record.key.file_name().as_str())
        {
            continue;
        }
        profiles.insert(record.key, record.profile);
    }
    profiles
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-task-profile-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn sample(elapsed_ms: u64, memory: u64) -> TaskExecutionSample {
        TaskExecutionSample {
            elapsed_ms,
            succeeded: true,
            resource_high_water: WorkCost {
                cpu: 2,
                memory,
                io: 3,
                network: 4,
            },
            arena_high_water_bytes: memory / 2,
            declared_cost: WorkCost {
                cpu: 1,
                memory: 1,
                io: 1,
                network: 1,
            },
        }
    }

    #[test]
    fn tasks_sharing_runtime_artifacts_keep_independent_profiles() {
        let root = temp_root("independent");
        let store = TaskExecutionProfileStore::new(&root);
        let a = TaskProfileKey::new([7; 32], 31_844);
        let b = TaskProfileKey::new([7; 32], 31_845);
        store.record(a, sample(4, 100));
        store.record(a, sample(8, 200));
        store.record(b, sample(64, 900));

        let a_profile = store.profile(a).unwrap();
        let b_profile = store.profile(b).unwrap();
        assert_eq!(a_profile.samples, 2);
        assert_eq!(b_profile.samples, 1);
        assert_eq!(a_profile.max_ms, 8);
        assert_eq!(b_profile.max_ms, 64);
        assert_eq!(a_profile.resource_high_water.memory, 200);
        assert_eq!(b_profile.resource_high_water.memory, 900);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn p95_and_scheduler_hint_use_observed_high_water_without_becoming_authority() {
        let mut profile = TaskExecutionProfile::default();
        for elapsed in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512] {
            profile.record(sample(elapsed, 4096));
        }
        assert_eq!(profile.p95_ms(), 512);
        let hint = profile.scheduler_hint(WorkCost::default());
        assert!(hint.cpu >= 512);
        assert!(hint.memory >= 4096);
    }

    #[test]
    fn corrupt_persistent_profile_is_ignored_and_does_not_block_recording() {
        let root = temp_root("corrupt");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(format!("{}-31844.json", "aa".repeat(32))), b"{broken").unwrap();

        let store = TaskExecutionProfileStore::new(&root);
        assert!(store.profiles().is_empty());
        let key = TaskProfileKey::new([0xaa; 32], 31_844);
        let profile = store.record(key, sample(9, 123));
        assert_eq!(profile.samples, 1);
        assert_eq!(store.profile(key).unwrap().samples, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn valid_profile_round_trips_across_store_restart() {
        let root = temp_root("restart");
        let key = TaskProfileKey::new([3; 32], 31_900);
        {
            let store = TaskExecutionProfileStore::new(&root);
            store.record(key, sample(17, 777));
        }
        let restored = TaskExecutionProfileStore::new(&root)
            .profile(key)
            .expect("profile should restore");
        assert_eq!(restored.samples, 1);
        assert_eq!(restored.last_ms, 17);
        assert_eq!(restored.resource_high_water.memory, 777);
        let _ = fs::remove_dir_all(root);
    }
}
