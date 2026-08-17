use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::execution::ExecutionTask;
use crate::worker::{Runner, Worker, WorkerSnapshot};

#[derive(Debug, Clone)]
pub struct SwampSnapshot {
    pub id: usize,
    pub queued: usize,
    pub queued_cost: u64,
    pub completed: u64,
    pub failed: u64,
    pub throughput_per_sec: f64,
    pub workers: Vec<WorkerSnapshot>,
}

pub struct Swamp {
    id: usize,
    queue: Arc<Mutex<VecDeque<ExecutionTask>>>,
    started: Instant,
    workers: Vec<Worker>,
}

impl Swamp {
    pub fn new(
        id: usize,
        worker_count: usize,
        runner: Runner,
        on_complete: Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static>,
    ) -> Arc<Self> {
        let workers = (0..worker_count.max(1)).map(|worker_id| Worker::new(worker_id, Arc::clone(&runner), Arc::clone(&on_complete))).collect::<Vec<_>>();
        let swamp = Arc::new(Self { id, queue: Arc::new(Mutex::new(VecDeque::new())), started: Instant::now(), workers });
        Self::start_dispatcher(&swamp);
        swamp
    }

    pub fn enqueue(&self, task: ExecutionTask) { self.queue.lock().expect("swamp queue poisoned").push_back(task); }
    pub fn drain(&self, count: usize) -> Vec<ExecutionTask> {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        let drain_count = count.min(queue.len());
        queue.drain(..drain_count).collect()
    }
    pub fn drain_all(&self) -> Vec<ExecutionTask> { self.queue.lock().expect("swamp queue poisoned").drain(..).collect() }

    pub fn remove_execution_string(&self, id: &str) -> bool {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        let before = queue.len();
        queue.retain(|task| task.id.to_string() != id);
        before != queue.len()
    }

    pub fn queued(&self) -> usize { self.queue.lock().expect("swamp queue poisoned").len() }
    pub fn queued_cost(&self) -> u64 { self.queue.lock().expect("swamp queue poisoned").iter().map(|task| task.declared_cost.scalar()).sum() }

    pub fn snapshot(&self) -> SwampSnapshot {
        let workers = self.workers.iter().map(Worker::snapshot).collect::<Vec<_>>();
        let completed = workers.iter().map(|worker| worker.completed).sum::<u64>();
        let failed = workers.iter().map(|worker| worker.failed).sum::<u64>();
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        SwampSnapshot { id: self.id, queued: self.queued(), queued_cost: self.queued_cost(), completed, failed, throughput_per_sec: completed as f64 / elapsed, workers }
    }

    fn start_dispatcher(swamp: &Arc<Self>) {
        let weak = Arc::downgrade(swamp);
        thread::Builder::new().name(format!("rbe-swamp-{}", swamp.id)).spawn(move || loop {
            let Some(swamp) = weak.upgrade() else { break };
            let mut progressed = false;
            for worker in &swamp.workers {
                if !worker.is_idle() { continue; }
                let task = swamp.queue.lock().expect("swamp queue poisoned").pop_front();
                let Some(task) = task else { break; };
                if worker.try_send(task.clone()) { progressed = true; } else { swamp.queue.lock().expect("swamp queue poisoned").push_front(task); }
            }
            if !progressed { thread::sleep(Duration::from_millis(1)); }
        }).expect("failed to start Swamp dispatcher");
    }

    pub fn worker_count(&self) -> usize { self.workers.len() }
}
