use std::collections::HashMap;
use std::sync::Mutex;

use crate::execution::WorkCost;

#[derive(Debug, Clone, Default)]
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
        if self.samples == 0 { 0.0 } else { self.total_ms as f64 / self.samples as f64 }
    }
}

#[derive(Debug, Default)]
pub struct ArtifactCache {
    profiles: Mutex<HashMap<String, ExecutionProfile>>,
}

impl ArtifactCache {
    pub fn record(&self, artifact_hash: &str, elapsed_ms: u64, declared_cost: WorkCost) {
        let mut profiles = self.profiles.lock().expect("artifact cache poisoned");
        profiles.entry(artifact_hash.to_string()).or_default().record(elapsed_ms, declared_cost);
    }

    pub fn profile(&self, artifact_hash: &str) -> Option<ExecutionProfile> {
        self.profiles.lock().expect("artifact cache poisoned").get(artifact_hash).cloned()
    }

    pub fn len(&self) -> usize {
        self.profiles.lock().expect("artifact cache poisoned").len()
    }
}
