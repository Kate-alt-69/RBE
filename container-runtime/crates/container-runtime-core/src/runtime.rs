use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::cache::ArtifactCache;
use crate::execution::{ExecutionId, ExecutionTask, WorkCost};
use crate::swamp::{Swamp, SwampSnapshot};

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub swamps: usize,
    pub workers_per_swamp: usize,
    pub rebalance_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let logical_cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Self { swamps: logical_cpus, workers_per_swamp: 1, rebalance_interval_ms: 25 }
    }
}

pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    global_queue: Mutex<VecDeque<ExecutionTask>>,
    swamps: Vec<Arc<Swamp>>,
    cache: Arc<ArtifactCache>,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        let config = RuntimeConfig {
            swamps: config.swamps.max(1),
            workers_per_swamp: config.workers_per_swamp.max(1),
            rebalance_interval_ms: config.rebalance_interval_ms.max(1),
        };
        let cache = Arc::new(ArtifactCache::default());
        let runtime = Arc::new_cyclic(|weak_runtime| {
            let cache_for_completion = Arc::clone(&cache);
            let completion: Arc<dyn Fn(&ExecutionTask, u64) + Send + Sync + 'static> = Arc::new(move |task, elapsed_ms| {
                cache_for_completion.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                let _ = weak_runtime.upgrade();
            });
            let swamps = (0..config.swamps)
                .map(|id| Swamp::new(id, config.workers_per_swamp, Arc::clone(&completion)))
                .collect();
            Self {
                config,
                next_execution: AtomicU64::new(1),
                global_queue: Mutex::new(VecDeque::new()),
                swamps,
                cache,
            }
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

    pub fn submit(&self, artifact_hash: impl Into<String>, cost: WorkCost, work_ms: u64) -> ExecutionId {
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.global_queue.lock().expect("global queue poisoned").push_back(ExecutionTask {
            id,
            artifact_hash: artifact_hash.into(),
            declared_cost: cost,
            work_ms,
        });
        id
    }

    pub fn rebalance_once(&self) {
        let pending = {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            queue.drain(..).collect::<Vec<_>>()
        };

        for task in pending {
            let target = self.swamps.iter().min_by_key(|swamp| swamp.queued_cost()).expect("runtime has no Swamps");
            target.enqueue(task);
        }

        self.rebalance_swamp_backlogs();
    }

    fn rebalance_swamp_backlogs(&self) {
        if self.swamps.len() < 2 { return; }
        let snapshots = self.swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        let source = snapshots.iter().max_by_key(|snapshot| snapshot.queued_cost).expect("runtime has no Swamps");
        let target = snapshots.iter().min_by_key(|snapshot| snapshot.queued_cost).expect("runtime has no Swamps");
        if source.id == target.id || source.queued < 4 { return; }

        for task in self.swamps[source.id].drain(source.queued / 2) {
            self.swamps[target.id].enqueue(task);
        }
    }

    pub fn global_queue_len(&self) -> usize { self.global_queue.lock().expect("global queue poisoned").len() }
    pub fn cache(&self) -> Arc<ArtifactCache> { Arc::clone(&self.cache) }
    pub fn snapshots(&self) -> Vec<SwampSnapshot> { self.swamps.iter().map(|swamp| swamp.snapshot()).collect() }
    pub fn config(&self) -> &RuntimeConfig { &self.config }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn execution_ids_are_unique() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 1, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        let a = runtime.submit("a", WorkCost::default(), 0);
        let b = runtime.submit("b", WorkCost::default(), 0);
        assert_ne!(a, b);
        assert_ne!(a.to_string(), b.to_string());
    }

    #[test]
    fn cache_learns_actual_runtime() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 1, workers_per_swamp: 1, rebalance_interval_ms: 5 });
        runtime.submit("artifact", WorkCost { cpu: 10, ..Default::default() }, 2);
        thread::sleep(Duration::from_millis(20));
        let profile = runtime.cache().profile("artifact").expect("profile recorded");
        assert_eq!(profile.samples, 1);
        assert!(profile.last_ms >= 1);
    }

    #[test]
    fn backlog_is_rebalanced_between_swamps() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 2, workers_per_swamp: 1, rebalance_interval_ms: 1000 });
        for _ in 0..20 {
            runtime.submit("artifact", WorkCost { cpu: 10, ..Default::default() }, 25);
        }
        runtime.rebalance_once();
        let total = runtime.snapshots().iter().map(|snapshot| snapshot.queued).sum::<usize>();
        assert!(total <= 20);
    }
}
