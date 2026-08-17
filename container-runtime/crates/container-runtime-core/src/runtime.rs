use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use environments::EnvironmentId;
use resource_limits::ResourceLimits;
use sandbox_primitives::SandboxPolicy;

use crate::cache::ArtifactCache;
use crate::environment::{EnvironmentRuntime, EnvironmentSnapshot};
use crate::execution::{ExecutionId, ExecutionTask, WorkCost};

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub swamps_per_environment: usize,
    pub workers_per_swamp: usize,
    pub rebalance_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            swamps_per_environment: thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8),
            workers_per_swamp: 1,
            rebalance_interval_ms: 25,
        }
    }
}

pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    environments: Vec<EnvironmentRuntime>,
    cache: Arc<ArtifactCache>,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        let config = RuntimeConfig {
            swamps_per_environment: config.swamps_per_environment.max(1),
            workers_per_swamp: config.workers_per_swamp.max(1),
            rebalance_interval_ms: config.rebalance_interval_ms.max(1),
        };
        let cache = Arc::new(ArtifactCache::default());

        let completion: Arc<dyn Fn(&ExecutionTask, u64) + Send + Sync + 'static> = {
            let cache = Arc::clone(&cache);
            Arc::new(move |task, elapsed_ms| {
                cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
            })
        };

        let environments = EnvironmentId::ALL
            .into_iter()
            .map(|id| EnvironmentRuntime::new(id, config.swamps_per_environment, config.workers_per_swamp, Arc::clone(&completion)))
            .collect();

        let runtime = Arc::new(Self {
            config,
            next_execution: AtomicU64::new(1),
            global_queue: Mutex::new(VecDeque::new()),
            environments,
            cache,
        });

        let weak = Arc::downgrade(&runtime);
        let interval = runtime.config.rebalance_interval_ms;
        thread::Builder::new()
            .name("rbe-runtime-scheduler".to_string())
            .spawn(move || {
                while let Some(runtime) = weak.upgrade() {
                    runtime.rebalance_once();
                    thread::sleep(Duration::from_millis(interval));
                }
            })
            .expect("failed to start runtime scheduler");

        runtime
    }

    pub fn submit(
        &self,
        environment: EnvironmentId,
        artifact_hash: impl Into<String>,
        cost: WorkCost,
        work_ms: u64,
    ) -> ExecutionId {
        self.submit_with_policy(
            environment,
            artifact_hash,
            cost,
            ResourceLimits::default(),
            SandboxPolicy::default(),
            work_ms,
            Vec::new(),
        )
    }

    pub fn submit_with_policy(
        &self,
        environment: EnvironmentId,
        artifact_hash: impl Into<String>,
        cost: WorkCost,
        limits: ResourceLimits,
        sandbox: SandboxPolicy,
        work_ms: u64,
        payload: Vec<u8>,
    ) -> ExecutionId {
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.global_queue.lock().expect("global queue poisoned").push_back((
            environment,
            ExecutionTask {
                id,
                environment: environment.to_string(),
                artifact_hash: artifact_hash.into(),
                declared_cost: cost,
                limits,
                sandbox,
                work_ms,
                payload,
            },
        ));
        id
    }

    pub fn rebalance_once(&self) {
        let pending = {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            queue.drain(..).collect::<Vec<_>>()
        };

        for (environment, task) in pending {
            self.environment(environment).enqueue(task);
        }

        for environment in &self.environments {
            environment.rebalance();
        }
    }

    fn environment(&self, id: EnvironmentId) -> &EnvironmentRuntime {
        self.environments
            .iter()
            .find(|environment| environment.id == id)
            .expect("configured Environment is missing from runtime")
    }

    pub fn global_queue_len(&self) -> usize {
        self.global_queue.lock().expect("global queue poisoned").len()
    }

    pub fn cache(&self) -> Arc<ArtifactCache> { Arc::clone(&self.cache) }

    pub fn snapshots(&self) -> Vec<EnvironmentSnapshot> {
        self.environments.iter().map(EnvironmentRuntime::snapshot).collect()
    }

    pub fn config(&self) -> &RuntimeConfig { &self.config }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn execution_ids_are_unique() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 1, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        let a = runtime.submit(EnvironmentId::General1, "a", WorkCost::default(), 0);
        let b = runtime.submit(EnvironmentId::General1, "b", WorkCost::default(), 0);
        assert_ne!(a, b);
        assert_ne!(a.to_string(), b.to_string());
    }

    #[test]
    fn cache_learns_actual_runtime() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 1, workers_per_swamp: 1, rebalance_interval_ms: 5 });
        runtime.submit(EnvironmentId::General1, "artifact", WorkCost { cpu: 10, ..Default::default() }, 2);
        thread::sleep(Duration::from_millis(20));
        let profile = runtime.cache().profile("artifact").expect("profile recorded");
        assert_eq!(profile.samples, 1);
        assert!(profile.last_ms >= 1);
    }

    #[test]
    fn every_environment_gets_its_own_swamp_pool() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 2, workers_per_swamp: 1, rebalance_interval_ms: 1000 });
        let snapshots = runtime.snapshots();
        assert_eq!(snapshots.len(), 6);
        assert!(snapshots.iter().all(|snapshot| snapshot.swamps.len() == 2));
    }

    #[test]
    fn policy_is_attached_to_execution() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 1, workers_per_swamp: 1, rebalance_interval_ms: 1000 });
        let id = runtime.submit_with_policy(
            EnvironmentId::General1,
            "secure-artifact",
            WorkCost { cpu: 10, ..Default::default() },
            ResourceLimits::default(),
            SandboxPolicy::default(),
            0,
            b"payload".to_vec(),
        );
        assert!(id.to_string().starts_with("exec-"));
    }
}
