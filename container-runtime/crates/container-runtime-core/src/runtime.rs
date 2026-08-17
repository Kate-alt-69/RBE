use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExecutionId(u64);

impl ExecutionId {
    fn next(sequence: &AtomicU64) -> Self {
        Self(sequence.fetch_add(1, Ordering::Relaxed))
    }
    pub fn get(self) -> u64 { self.0 }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WorkCost {
    pub cpu: u64,
    pub memory: u64,
    pub io: u64,
    pub network: u64,
}

impl WorkCost {
    pub fn scalar(self) -> u64 {
        self.cpu.saturating_add(self.memory)
            .saturating_add(self.io)
            .saturating_add(self.network)
            .max(1)
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionTask {
    pub id: ExecutionId,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    pub work_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionProfile {
    pub samples: u64,
    pub total_ms: u64,
    pub last_ms: u64,
}

impl ExecutionProfile {
    fn record(&mut self, elapsed_ms: u64) {
        self.samples = self.samples.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(elapsed_ms);
        self.last_ms = elapsed_ms;
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
    pub fn record(&self, artifact_hash: &str, elapsed_ms: u64) {
        let mut profiles = self.profiles.lock().expect("artifact cache poisoned");
        profiles.entry(artifact_hash.to_string()).or_default().record(elapsed_ms);
    }
    pub fn profile(&self, artifact_hash: &str) -> Option<ExecutionProfile> {
        self.profiles.lock().expect("artifact cache poisoned").get(artifact_hash).cloned()
    }
    pub fn len(&self) -> usize { self.profiles.lock().expect("artifact cache poisoned").len() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState { Idle, Running, Stopped }

struct WorkerCommand { task: ExecutionTask }

pub struct WorkerSnapshot {
    pub id: usize,
    pub state: WorkerState,
    pub completed: u64,
    pub total_ms: u64,
    pub current: Option<ExecutionId>,
}

struct Worker {
    id: usize,
    tx: mpsc::Sender<WorkerCommand>,
    state: Arc<Mutex<WorkerState>>,
    current: Arc<Mutex<Option<ExecutionId>>>,
    completed: Arc<AtomicU64>,
    total_ms: Arc<AtomicU64>,
}

impl Worker {
    fn new(id: usize, on_complete: Arc<dyn Fn(&ExecutionTask, u64) + Send + Sync + 'static>) -> Self {
        let (tx, rx) = mpsc::channel::<WorkerCommand>();
        let state = Arc::new(Mutex::new(WorkerState::Idle));
        let current = Arc::new(Mutex::new(None));
        let completed = Arc::new(AtomicU64::new(0));
        let total_ms = Arc::new(AtomicU64::new(0));
        let thread_state = Arc::clone(&state);
        let thread_current = Arc::clone(&current);
        let thread_completed = Arc::clone(&completed);
        let thread_total_ms = Arc::clone(&total_ms);

        thread::Builder::new().name(format!("rbe-worker-{id}")).spawn(move || {
            while let Ok(command) = rx.recv() {
                *thread_state.lock().expect("worker state poisoned") = WorkerState::Running;
                *thread_current.lock().expect("worker current poisoned") = Some(command.task.id);
                let started = Instant::now();
                if command.task.work_ms > 0 { thread::sleep(Duration::from_millis(command.task.work_ms)); }
                let elapsed_ms = started.elapsed().as_millis() as u64;
                thread_completed.fetch_add(1, Ordering::Relaxed);
                thread_total_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
                on_complete(&command.task, elapsed_ms);
                *thread_current.lock().expect("worker current poisoned") = None;
                *thread_state.lock().expect("worker state poisoned") = WorkerState::Idle;
            }
            *thread_state.lock().expect("worker state poisoned") = WorkerState::Stopped;
        }).expect("failed to start worker thread");

        Self { id, tx, state, current, completed, total_ms }
    }
    fn is_idle(&self) -> bool { *self.state.lock().expect("worker state poisoned") == WorkerState::Idle }
    fn try_send(&self, task: ExecutionTask) -> bool {
        if !self.is_idle() { return false; }
        self.tx.send(WorkerCommand { task }).is_ok()
    }
    fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            id: self.id,
            state: *self.state.lock().expect("worker state poisoned"),
            completed: self.completed.load(Ordering::Relaxed),
            total_ms: self.total_ms.load(Ordering::Relaxed),
            current: *self.current.lock().expect("worker current poisoned"),
        }
    }
}

pub struct SwampSnapshot {
    pub id: usize,
    pub queued: usize,
    pub completed: u64,
    pub throughput_per_sec: f64,
    pub workers: Vec<WorkerSnapshot>,
}

pub struct Swamp {
    id: usize,
    queue: Arc<Mutex<VecDeque<ExecutionTask>>>,
    workers: Vec<Worker>,
}

impl Swamp {
    fn new(id: usize, worker_count: usize, on_complete: Arc<dyn Fn(&ExecutionTask, u64) + Send + Sync + 'static>) -> Arc<Self> {
        let workers = (0..worker_count.max(1)).map(|worker_id| Worker::new(worker_id, Arc::clone(&on_complete))).collect();
        let swamp = Arc::new(Self { id, queue: Arc::new(Mutex::new(VecDeque::new())), workers });
        Self::start_dispatcher(&swamp);
        swamp
    }
    fn start_dispatcher(swamp: &Arc<Self>) {
        let weak = Arc::downgrade(swamp);
        thread::Builder::new().name(format!("rbe-swamp-{}", swamp.id)).spawn(move || loop {
            let Some(swamp) = weak.upgrade() else { break };
            let mut dispatched = false;
            for worker in &swamp.workers {
                if !worker.is_idle() { continue; }
                let task = swamp.queue.lock().expect("swamp queue poisoned").pop_front();
                let Some(task) = task else { break };
                if worker.try_send(task.clone()) { dispatched = true; }
                else { swamp.queue.lock().expect("swamp queue poisoned").push_front(task); }
            }
            if !dispatched { thread::sleep(Duration::from_millis(1)); }
        }).expect("failed to start swamp dispatcher");
    }
    fn enqueue(&self, task: ExecutionTask) { self.queue.lock().expect("swamp queue poisoned").push_back(task); }
    fn drain(&self, count: usize) -> Vec<ExecutionTask> {
        let mut queue = self.queue.lock().expect("swamp queue poisoned");
        queue.drain(..count.min(queue.len())).collect()
    }
    fn queued(&self) -> usize { self.queue.lock().expect("swamp queue poisoned").len() }
    fn snapshot(&self) -> SwampSnapshot {
        let workers = self.workers.iter().map(Worker::snapshot).collect::<Vec<_>>();
        let completed = workers.iter().map(|worker| worker.completed).sum::<u64>();
        let total_ms = workers.iter().map(|worker| worker.total_ms).sum::<u64>();
        let throughput_per_sec = if total_ms == 0 { 0.0 } else { completed as f64 / (total_ms as f64 / 1000.0) };
        SwampSnapshot { id: self.id, queued: self.queued(), completed, throughput_per_sec, workers }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub swamps: usize,
    pub workers_per_swamp: usize,
    pub rebalance_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let logical_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
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
        let config = RuntimeConfig { swamps: config.swamps.max(1), workers_per_swamp: config.workers_per_swamp.max(1), ..config };
        let cache = Arc::new(ArtifactCache::default());
        Arc::new_cyclic(|_| {
            let cache_for_completion = Arc::clone(&cache);
            let completion: Arc<dyn Fn(&ExecutionTask, u64) + Send + Sync> = Arc::new(move |task, elapsed_ms| {
                cache_for_completion.record(&task.artifact_hash, elapsed_ms);
            });
            let swamps = (0..config.swamps).map(|id| Swamp::new(id, config.workers_per_swamp, Arc::clone(&completion))).collect();
            Self { config, next_execution: AtomicU64::new(1), global_queue: Mutex::new(VecDeque::new()), swamps, cache }
        })
    }
    pub fn submit(&self, artifact_hash: impl Into<String>, cost: WorkCost, work_ms: u64) -> ExecutionId {
        let id = ExecutionId::next(&self.next_execution);
        self.global_queue.lock().expect("global queue poisoned").push_back(ExecutionTask { id, artifact_hash: artifact_hash.into(), declared_cost: cost, work_ms });
        id
    }
    pub fn dispatch_pending(&self) {
        let pending = self.global_queue.lock().expect("global queue poisoned").drain(..).collect::<Vec<_>>();
        for task in pending {
            let target = self.swamps.iter().min_by_key(|swamp| swamp.queued()).expect("runtime has no swamps");
            target.enqueue(task);
        }
    }
    pub fn rebalance_once(&self) {
        self.dispatch_pending();
        if self.swamps.len() < 2 { return; }
        let snapshots = self.swamps.iter().map(|swamp| swamp.snapshot()).collect::<Vec<_>>();
        let slowest = snapshots.iter().max_by_key(|snapshot| snapshot.queued).expect("runtime has no swamps");
        let fastest = snapshots.iter().max_by(|a, b| a.throughput_per_sec.partial_cmp(&b.throughput_per_sec).unwrap_or(std::cmp::Ordering::Equal)).expect("runtime has no swamps");
        if slowest.id == fastest.id || slowest.queued < 4 { return; }
        for task in self.swamps[slowest.id].drain(slowest.queued / 2) { self.swamps[fastest.id].enqueue(task); }
    }
    pub fn global_queue_len(&self) -> usize { self.global_queue.lock().expect("global queue poisoned").len() }
    pub fn cache(&self) -> Arc<ArtifactCache> { Arc::clone(&self.cache) }
    pub fn snapshots(&self) -> Vec<SwampSnapshot> { self.swamps.iter().map(|swamp| swamp.snapshot()).collect() }
    pub fn config(&self) -> &RuntimeConfig { &self.config }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_ids_are_unique() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 1, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        assert_ne!(runtime.submit("a", WorkCost::default(), 0), runtime.submit("b", WorkCost::default(), 0));
    }
    #[test]
    fn cache_learns_actual_runtime() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 1, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        runtime.submit("artifact", WorkCost { cpu: 10, ..Default::default() }, 3);
        runtime.dispatch_pending();
        thread::sleep(Duration::from_millis(15));
        let profile = runtime.cache().profile("artifact").expect("profile recorded");
        assert_eq!(profile.samples, 1);
        assert!(profile.last_ms >= 1);
    }
    #[test]
    fn backlog_can_move_between_swamps() {
        let runtime = Runtime::new(RuntimeConfig { swamps: 2, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        for _ in 0..20 { runtime.submit("artifact", WorkCost::default(), 50); }
        runtime.dispatch_pending();
        runtime.rebalance_once();
        let total = runtime.snapshots().iter().map(|snapshot| snapshot.queued).sum::<usize>();
        assert!(total <= 20);
    }
}
