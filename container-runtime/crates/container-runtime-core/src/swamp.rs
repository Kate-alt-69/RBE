use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::execution::ExecutionTask;
use crate::worker::{Completion, Runner, Worker, WorkerSnapshot};

struct DispatchSignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

#[derive(Default)]
struct SwampQueue {
    tasks: VecDeque<ExecutionTask>,
    cost: u64,
}

impl DispatchSignal {
    fn new() -> Self { Self { generation: Mutex::new(0), changed: Condvar::new() } }

    fn notify(&self) {
        let mut generation = self.generation.lock().expect("dispatch signal poisoned");
        *generation = generation.wrapping_add(1);
        self.changed.notify_one();
    }

    fn generation(&self) -> u64 { *self.generation.lock().expect("dispatch signal poisoned") }

    fn wait_for_change(&self, observed: u64) -> u64 {
        let generation = self.generation.lock().expect("dispatch signal poisoned");
        if *generation != observed { return *generation; }
        let (generation, _) = self.changed.wait_timeout(generation, Duration::from_secs(1)).expect("dispatch signal poisoned");
        *generation
    }
}

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
    queue: Arc<Mutex<SwampQueue>>,
    dispatch_signal: Arc<DispatchSignal>,
    started: Instant,
    workers: Vec<Worker>,
}

impl Swamp {
    pub fn new(
        id: usize,
        worker_count: usize,
        runner: Runner,
        on_complete: Completion,
    ) -> Arc<Self> {
        let dispatch_signal = Arc::new(DispatchSignal::new());
        let workers = (0..worker_count.max(1)).map(|worker_id| {
            let signal = Arc::clone(&dispatch_signal);
            Worker::new(worker_id, Arc::clone(&runner), Arc::clone(&on_complete), Arc::new(move || signal.notify()))
        }).collect::<Vec<_>>();
        let swamp = Arc::new(Self { id, queue: Arc::new(Mutex::new(SwampQueue::default())), dispatch_signal, started: Instant::now(), workers });
        Self::start_dispatcher(&swamp);
        swamp
    }

    pub fn enqueue(&self, task: ExecutionTask) {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        queue.cost = queue.cost.saturating_add(task.declared_cost.scalar());
        queue.tasks.push_back(task);
        drop(queue);
        self.dispatch_signal.notify();
    }
    pub fn drain(&self, count: usize) -> Vec<ExecutionTask> {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        let drain_count = count.min(queue.tasks.len());
        let drained = queue.tasks.drain(..drain_count).collect::<Vec<_>>();
        let drained_cost = drained.iter().fold(0u64, |cost, task| cost.saturating_add(task.declared_cost.scalar()));
        queue.cost = queue.cost.saturating_sub(drained_cost);
        drained
    }
    pub fn drain_all(&self) -> Vec<ExecutionTask> {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        queue.cost = 0;
        queue.tasks.drain(..).collect()
    }

    pub fn remove_execution_string(&self, id: &str) -> bool {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        let before = queue.tasks.len();
        queue.tasks.retain(|task| task.id.to_string() != id);
        if before == queue.tasks.len() { return false; }
        queue.cost = queue.tasks.iter().fold(0u64, |cost, task| cost.saturating_add(task.declared_cost.scalar()));
        true
    }

    fn queue_stats(&self) -> (usize, u64) {
        let queue = self.queue.lock().expect("swamp queue poisoned");
        (queue.tasks.len(), queue.cost)
    }

    pub fn queued_cost(&self) -> u64 { self.queue.lock().expect("swamp queue poisoned").cost }

    pub fn snapshot(&self) -> SwampSnapshot {
        let (queued, queued_cost) = self.queue_stats();
        let workers = self.workers.iter().map(Worker::snapshot).collect::<Vec<_>>();
        let completed = workers.iter().map(|worker| worker.completed).sum::<u64>();
        let failed = workers.iter().map(|worker| worker.failed).sum::<u64>();
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        SwampSnapshot { id: self.id, queued, queued_cost, completed, failed, throughput_per_sec: completed as f64 / elapsed, workers }
    }

    fn start_dispatcher(swamp: &Arc<Self>) {
        let weak = Arc::downgrade(swamp);
        let signal = Arc::clone(&swamp.dispatch_signal);
        thread::Builder::new().name(format!("rbe-swamp-{}", swamp.id)).spawn(move || {
            let mut observed = signal.generation();
            loop {
                let Some(swamp) = weak.upgrade() else { break };
                let mut progressed = false;
                for worker in &swamp.workers {
                    if !worker.is_idle() { continue; }
                    let task = {
                        let mut queue = swamp.queue.lock().expect("swamp queue poisoned");
                        let task = queue.tasks.pop_front();
                        if let Some(task) = &task {
                            queue.cost = queue.cost.saturating_sub(task.declared_cost.scalar());
                        }
                        task
                    };
                    let Some(task) = task else { break; };
                    match worker.try_send(task) {
                        None => progressed = true,
                        Some(task) => {
                            let mut queue = swamp.queue.lock().expect("swamp queue poisoned");
                            queue.cost = queue.cost.saturating_add(task.declared_cost.scalar());
                            queue.tasks.push_front(task);
                        }
                    }
                }
                drop(swamp);
                if progressed { observed = signal.generation(); }
                else { observed = signal.wait_for_change(observed); }
            }
        }).expect("failed to start Swamp dispatcher");
    }

    pub fn worker_count(&self) -> usize { self.workers.len() }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use resource_limits::ResourceLimits;
    use sandbox_primitives::SandboxPolicy;

    use super::Swamp;
    use crate::execution::{ExecutionId, ExecutionTask, WorkCost};
    use crate::worker::{Completion, Runner};

    fn task(sequence: u64, cost: u64) -> ExecutionTask {
        ExecutionTask {
            id: ExecutionId::from_parts(1, sequence),
            environment: "general-1".into(),
            artifact_hash: "test".into(),
            declared_cost: WorkCost { cpu: cost, ..WorkCost::default() },
            limits: ResourceLimits::default(),
            sandbox: SandboxPolicy::default(),
            work_ms: 0,
            payload: Vec::new(),
        }
    }

    #[test]
    fn queued_task_dispatches_when_worker_becomes_available() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let runner: Runner = {
            let gate = Arc::clone(&gate);
            Arc::new(move |task| {
                if task.id.sequence() == 1 {
                    let (lock, changed) = &*gate;
                    let released = lock.lock().expect("gate poisoned");
                    let _guard = changed.wait_while(released, |released| !*released).expect("gate poisoned");
                }
                Ok(())
            })
        };
        let completed = Arc::new((Mutex::new(0usize), Condvar::new()));
        let completion: Completion = {
            let completed = Arc::clone(&completed);
            Arc::new(move |_, _, _| {
                let (lock, changed) = &*completed;
                *lock.lock().expect("completion count poisoned") += 1;
                changed.notify_all();
            })
        };
        let swamp = Swamp::new(0, 1, runner, completion);

        swamp.enqueue(task(1, 1));
        let first_started_by = Instant::now() + Duration::from_secs(1);
        while swamp.snapshot().workers[0].current.is_none() && Instant::now() < first_started_by {
            std::thread::yield_now();
        }
        assert_eq!(swamp.snapshot().workers[0].current, Some(ExecutionId::from_parts(1, 1)));
        swamp.enqueue(task(2, 7));
        let snapshot = swamp.snapshot();
        assert_eq!(snapshot.queued, 1);
        assert_eq!(snapshot.queued_cost, 7);

        let (lock, changed) = &*gate;
        *lock.lock().expect("gate poisoned") = true;
        changed.notify_all();

        let (lock, changed) = &*completed;
        let completed = lock.lock().expect("completion count poisoned");
        let (completed, _) = changed.wait_timeout_while(completed, Duration::from_secs(1), |count| *count < 2).expect("completion count poisoned");
        assert_eq!(*completed, 2);
        let snapshot = swamp.snapshot();
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.queued_cost, 0);
    }
}
