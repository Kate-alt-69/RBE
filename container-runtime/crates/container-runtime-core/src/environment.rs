use std::sync::{Arc, Mutex};

use environments::EnvironmentId;

use crate::execution::{ExecutionTask, WorkCost};
use crate::swamp::{Swamp, SwampSnapshot};
use crate::worker::Runner;

type Completion = Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static>;

pub struct EnvironmentRuntime {
    pub id: EnvironmentId,
    swamp_count: usize,
    workers_per_swamp: usize,
    runner: Runner,
    on_complete: Completion,
    swamps: Mutex<Vec<Arc<Swamp>>>,
}

#[derive(Debug, Clone)]
pub struct EnvironmentSnapshot {
    pub id: EnvironmentId,
    pub generation: u64,
    pub queued: usize,
    pub queued_cost: u64,
    pub swamps: Vec<SwampSnapshot>,
}

impl EnvironmentRuntime {
    pub fn new(id: EnvironmentId, swamp_count: usize, workers_per_swamp: usize, runner: Runner, on_complete: Completion) -> Self {
        let swamp_count = swamp_count.max(1);
        let workers_per_swamp = workers_per_swamp.max(1);
        let swamps = Self::build_swamps(swamp_count, workers_per_swamp, &runner, &on_complete);
        Self { id, swamp_count, workers_per_swamp, runner, on_complete, swamps: Mutex::new(swamps) }
    }

    fn build_swamps(swamp_count: usize, workers_per_swamp: usize, runner: &Runner, on_complete: &Completion) -> Vec<Arc<Swamp>> {
        (0..swamp_count).map(|swamp_id| Swamp::new(swamp_id, workers_per_swamp, Arc::clone(runner), Arc::clone(on_complete))).collect()
    }

    pub fn enqueue(&self, task: ExecutionTask) {
        let swamps = self.swamps.lock().expect("environment swamps poisoned");
        swamps.iter().min_by_key(|swamp| swamp.queued_cost()).expect("environment has no Swamps").enqueue(task);
    }

    pub fn cancel_queued_by_string(&self, id: &str) -> bool {
        let swamps = self.swamps.lock().expect("environment swamps poisoned");
        swamps.iter().any(|swamp| swamp.remove_execution_string(id))
    }

    pub fn restart(&self) -> Vec<ExecutionTask> {
        let mut swamps = self.swamps.lock().expect("environment swamps poisoned");
        let mut pending = Vec::new();
        for swamp in swamps.iter() { pending.extend(swamp.drain_all()); }
        *swamps = Self::build_swamps(self.swamp_count, self.workers_per_swamp, &self.runner, &self.on_complete);
        pending
    }

    pub fn rebalance(&self) {
        let swamps = self.swamps.lock().expect("environment swamps poisoned");
        if swamps.len() < 2 { return; }
        let snapshots = swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        let source = snapshots.iter().max_by_key(|snapshot| snapshot.queued_cost).unwrap();
        let target = snapshots.iter().min_by_key(|snapshot| snapshot.queued_cost).unwrap();
        if source.id == target.id || source.queued < 4 { return; }
        for task in swamps[source.id].drain(source.queued / 2) { swamps[target.id].enqueue(task); }
    }

    pub fn snapshot(&self) -> EnvironmentSnapshot {
        let swamps = self.swamps.lock().expect("environment swamps poisoned");
        let snapshots = swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        EnvironmentSnapshot {
            id: self.id,
            generation: 0,
            queued: snapshots.iter().map(|swamp| swamp.queued).sum(),
            queued_cost: snapshots.iter().map(|swamp| swamp.queued_cost).sum(),
            swamps: snapshots,
        }
    }

    pub fn worker_count(&self) -> usize { self.swamps.lock().expect("environment swamps poisoned").iter().map(|swamp| swamp.worker_count()).sum() }
    pub fn swamp_count(&self) -> usize { self.swamp_count }
}

#[allow(dead_code)]
fn _cost_hint(cost: WorkCost) -> u64 { cost.scalar() }
