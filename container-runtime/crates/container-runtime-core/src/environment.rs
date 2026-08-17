use std::sync::Arc;

use environments::EnvironmentId;

use crate::execution::{ExecutionTask, WorkCost};
use crate::swamp::{Swamp, SwampSnapshot};
use crate::worker::Runner;

pub struct EnvironmentRuntime {
    pub id: EnvironmentId,
    swamps: Vec<Arc<Swamp>>,
}

#[derive(Debug, Clone)]
pub struct EnvironmentSnapshot {
    pub id: EnvironmentId,
    pub queued: usize,
    pub queued_cost: u64,
    pub swamps: Vec<SwampSnapshot>,
}

impl EnvironmentRuntime {
    pub fn new(
        id: EnvironmentId,
        swamp_count: usize,
        workers_per_swamp: usize,
        runner: Runner,
        on_complete: Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static>,
    ) -> Self {
        let swamps = (0..swamp_count.max(1))
            .map(|swamp_id| Swamp::new(swamp_id, workers_per_swamp, Arc::clone(&runner), Arc::clone(&on_complete)))
            .collect();
        Self { id, swamps }
    }

    pub fn enqueue(&self, task: ExecutionTask) {
        let target = self.swamps.iter().min_by_key(|swamp| swamp.queued_cost()).expect("environment has no Swamps");
        target.enqueue(task);
    }

    pub fn rebalance(&self) {
        if self.swamps.len() < 2 { return; }
        let snapshots = self.swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        let source = snapshots.iter().max_by_key(|snapshot| snapshot.queued_cost).unwrap();
        let target = snapshots.iter().min_by_key(|snapshot| snapshot.queued_cost).unwrap();
        if source.id == target.id || source.queued < 4 { return; }
        for task in self.swamps[source.id].drain(source.queued / 2) {
            self.swamps[target.id].enqueue(task);
        }
    }

    pub fn snapshot(&self) -> EnvironmentSnapshot {
        let swamps = self.swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        EnvironmentSnapshot {
            id: self.id,
            queued: swamps.iter().map(|swamp| swamp.queued).sum(),
            queued_cost: swamps.iter().map(|swamp| swamp.queued_cost).sum(),
            swamps,
        }
    }

    pub fn worker_count(&self) -> usize { self.swamps.iter().map(|swamp| swamp.worker_count()).sum() }
    pub fn swamp_count(&self) -> usize { self.swamps.len() }
}

#[allow(dead_code)]
fn _cost_hint(cost: WorkCost) -> u64 { cost.scalar() }
