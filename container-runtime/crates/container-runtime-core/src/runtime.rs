use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{create_dir_all, read_to_string, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use execution_engine::{ExecutionLimits, WasmExecutor};
use environments::EnvironmentId;
use resource_limits::ResourceLimits;
use sandbox_primitives::SandboxPolicy;
use serde::{Deserialize, Serialize};

use crate::cache::ArtifactCache;
use crate::environment::{EnvironmentRuntime, EnvironmentSnapshot};
use crate::execution::{ExecutionId, ExecutionTask, WorkCost};
use crate::worker::Runner;

const JOURNAL_PATH: &str = "./data/container-runtime/execution.journal";

#[derive(Debug, Serialize, Deserialize)]
struct JournalEvent {
    kind: String,
    epoch_ns: u64,
    sequence: u64,
    environment: String,
    artifact_hash: String,
    cpu: u64,
    memory: u64,
    io: u64,
    network: u64,
    work_ms: u64,
}

struct Journal {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Journal {
    fn open() -> Arc<Self> {
        let path = PathBuf::from(JOURNAL_PATH);
        if let Some(parent) = path.parent() { let _ = create_dir_all(parent); }
        Arc::new(Self { path, lock: Mutex::new(()) })
    }

    fn append(&self, event: JournalEvent) {
        let _guard = self.lock.lock().expect("journal lock poisoned");
        let Ok(line) = serde_json::to_string(&event) else { return; };
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) else { return; };
        let _ = writeln!(file, "{line}");
    }

    fn recover(&self) -> (Vec<ExecutionTask>, u64) {
        let Ok(contents) = read_to_string(&self.path) else { return (Vec::new(), 0); };
        let mut latest: HashMap<String, (JournalEvent, bool)> = HashMap::new();
        let mut max_sequence = 0;
        for line in contents.lines() {
            let Ok(event) = serde_json::from_str::<JournalEvent>(line) else { continue; };
            max_sequence = max_sequence.max(event.sequence);
            let id = format!("exec-{:016x}-{:016x}", event.epoch_ns, event.sequence);
            match event.kind.as_str() {
                "queued" => { latest.insert(id, (event, true)); }
                "done" | "cancel" => {
                    if let Some(entry) = latest.get_mut(&id) { entry.1 = false; }
                }
                _ => {}
            }
        }

        let mut recovered = Vec::new();
        for (_, (event, pending)) in latest {
            if !pending { continue; }
            let Some(environment) = parse_environment(&event.environment) else { continue; };
            recovered.push(ExecutionTask {
                id: ExecutionId::from_parts(event.epoch_ns, event.sequence),
                environment: environment.to_string(),
                artifact_hash: event.artifact_hash,
                declared_cost: WorkCost { cpu: event.cpu, memory: event.memory, io: event.io, network: event.network },
                limits: ResourceLimits::default(),
                sandbox: SandboxPolicy::default(),
                work_ms: event.work_ms,
                payload: Vec::new(),
            });
        }
        (recovered, max_sequence)
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub swamps_per_environment: usize,
    pub workers_per_swamp: usize,
    pub rebalance_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self { swamps_per_environment: thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8), workers_per_swamp: 1, rebalance_interval_ms: 25 }
    }
}

pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    cancelled: Mutex<HashSet<String>>,
    generations: Mutex<HashMap<EnvironmentId, u64>>,
    environments: Vec<EnvironmentRuntime>,
    cache: Arc<ArtifactCache>,
    executor: Arc<WasmExecutor>,
    journal: Arc<Journal>,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        let config = RuntimeConfig {
            swamps_per_environment: config.swamps_per_environment.max(1),
            workers_per_swamp: config.workers_per_swamp.max(1),
            rebalance_interval_ms: config.rebalance_interval_ms.max(1),
        };
        let cache = Arc::new(ArtifactCache::default());
        let executor = Arc::new(WasmExecutor::new().expect("failed to initialize WASM executor"));
        let journal = Journal::open();
        let (recovered, max_sequence) = journal.recover();

        let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));
        let runner: Runner = {
            let cache = Arc::clone(&cache);
            let executor = Arc::clone(&executor);
            let cancelled = Arc::clone(&cancelled);
            Arc::new(move |task| {
                if cancelled.lock().expect("cancel table poisoned").contains(&task.id.to_string()) { return Err("execution cancelled before start".into()); }
                if let Some(wasm) = cache.artifact(&task.artifact_hash) {
                    let fuel = task.limits.cpu_millis.max(1).saturating_mul(10_000);
                    let result = executor.execute(&wasm, ExecutionLimits { fuel, max_memory_bytes: task.limits.memory_bytes }).map_err(|error| error.to_string())?;
                    if result.exit_code != 0 { return Err(format!("WASM exited with status {}", result.exit_code)); }
                } else if task.work_ms > 0 {
                    thread::sleep(Duration::from_millis(task.work_ms));
                }
                if cancelled.lock().expect("cancel table poisoned").contains(&task.id.to_string()) { return Err("execution cancelled".into()); }
                Ok(())
            })
        };

        let completion: Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static> = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            let journal = Arc::clone(&journal);
            Arc::new(move |task, elapsed_ms, result| {
                cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                let was_cancelled = cancelled.lock().expect("cancel table poisoned").remove(&task.id.to_string());
                journal.append(JournalEvent {
                    kind: if was_cancelled { "cancel".into() } else { "done".into() },
                    epoch_ns: task.id.epoch_ns(),
                    sequence: task.id.sequence(),
                    environment: task.environment.clone(),
                    artifact_hash: task.artifact_hash.clone(),
                    cpu: task.declared_cost.cpu,
                    memory: task.declared_cost.memory,
                    io: task.declared_cost.io,
                    network: task.declared_cost.network,
                    work_ms: elapsed_ms,
                });
                if let Err(error) = result { if !was_cancelled { tracing::warn!(execution = %task.id, environment = %task.environment, "execution failed: {error}"); } }
            })
        };

        let environments = EnvironmentId::ALL.into_iter().map(|id| EnvironmentRuntime::new(id, config.swamps_per_environment, config.workers_per_swamp, Arc::clone(&runner), Arc::clone(&completion))).collect();
        let generations = EnvironmentId::ALL.into_iter().map(|id| (id, 0u64)).collect();
        let next_execution = max_sequence.saturating_add(1).max(1);
        let runtime = Arc::new(Self {
            config,
            next_execution: AtomicU64::new(next_execution),
            global_queue: Mutex::new(VecDeque::new()),
            cancelled: Mutex::new(HashSet::new()),
            generations,
            environments,
            cache,
            executor,
            journal,
        });

        for task in recovered { if let Some(environment) = parse_environment(&task.environment) { runtime.global_queue.lock().expect("global queue poisoned").push_back((environment, task)); } }

        let weak = Arc::downgrade(&runtime);
        let interval = runtime.config.rebalance_interval_ms;
        thread::Builder::new().name("rbe-runtime-scheduler".to_string()).spawn(move || {
            while let Some(runtime) = weak.upgrade() {
                runtime.rebalance_once();
                thread::sleep(Duration::from_millis(interval));
            }
        }).expect("failed to start runtime scheduler");
        runtime
    }

    pub fn submit(&self, environment: EnvironmentId, artifact_hash: impl Into<String>, cost: WorkCost, work_ms: u64) -> ExecutionId {
        self.submit_with_policy(environment, artifact_hash, cost, ResourceLimits::default(), SandboxPolicy::default(), work_ms, Vec::new())
    }

    pub fn submit_with_policy(&self, environment: EnvironmentId, artifact_hash: impl Into<String>, cost: WorkCost, limits: ResourceLimits, sandbox: SandboxPolicy, work_ms: u64, payload: Vec<u8>) -> ExecutionId {
        let artifact_hash = artifact_hash.into();
        if !payload.is_empty() { self.cache.put_artifact(artifact_hash.clone(), payload); }
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.journal.append(JournalEvent { kind: "queued".into(), epoch_ns: id.epoch_ns(), sequence: id.sequence(), environment: environment.to_string(), artifact_hash: artifact_hash.clone(), cpu: cost.cpu, memory: cost.memory, io: cost.io, network: cost.network, work_ms });
        self.global_queue.lock().expect("global queue poisoned").push_back((environment, ExecutionTask { id, environment: environment.to_string(), artifact_hash, declared_cost: cost, limits, sandbox, work_ms, payload: Vec::new() }));
        id
    }

    pub fn register_artifact(&self, artifact_hash: impl Into<String>, wasm: Vec<u8>) { self.cache.put_artifact(artifact_hash, wasm); }

    pub fn cancel(&self, execution_id: &str) -> bool {
        let mut found = false;
        {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            let before = queue.len();
            queue.retain(|(_, task)| task.id.to_string() != execution_id);
            found |= before != queue.len();
        }
        for environment in &self.environments { found |= environment.cancel_queued_by_string(execution_id); }
        if found { self.journal.append_cancel_string(execution_id); return true; }
        self.cancelled.lock().expect("cancel table poisoned").insert(execution_id.to_string());
        self.journal.append_cancel_string(execution_id);
        true
    }

    pub fn restart_environment(&self, id: EnvironmentId) -> usize {
        let pending = self.environment(id).restart();
        let count = pending.len();
        if count != 0 { let mut queue = self.global_queue.lock().expect("global queue poisoned"); for task in pending { queue.push_back((id, task)); } }
        *self.generations.lock().expect("generation table poisoned").entry(id).or_default() += 1;
        count
    }

    pub fn environment_generation(&self, id: EnvironmentId) -> u64 { self.generations.lock().expect("generation table poisoned").get(&id).copied().unwrap_or(0) }

    pub fn rebalance_once(&self) {
        let pending = { let mut queue = self.global_queue.lock().expect("global queue poisoned"); queue.drain(..).collect::<Vec<_>>() };
        for (environment, task) in pending { self.environment(environment).enqueue(task); }
        for environment in &self.environments { environment.rebalance(); }
    }

    fn environment(&self, id: EnvironmentId) -> &EnvironmentRuntime { self.environments.iter().find(|environment| environment.id == id).expect("configured Environment is missing from runtime") }
    pub fn global_queue_len(&self) -> usize { self.global_queue.lock().expect("global queue poisoned").len() }
    pub fn cache(&self) -> Arc<ArtifactCache> { Arc::clone(&self.cache) }
    pub fn snapshots(&self) -> Vec<EnvironmentSnapshot> { self.environments.iter().map(|environment| { let mut snapshot = environment.snapshot(); snapshot.generation = self.environment_generation(snapshot.id); snapshot }).collect() }
    pub fn config(&self) -> &RuntimeConfig { &self.config }
    pub fn wasm_executor(&self) -> Arc<WasmExecutor> { Arc::clone(&self.executor) }
}

impl Journal {
    fn append_cancel_string(&self, execution_id: &str) {
        let Ok((epoch, sequence)) = parse_execution_id(execution_id) else { return; };
        self.append(JournalEvent { kind: "cancel".into(), epoch_ns: epoch, sequence, environment: "unknown".into(), artifact_hash: "unknown".into(), cpu: 0, memory: 0, io: 0, network: 0, work_ms: 0 });
    }
}

fn parse_execution_id(value: &str) -> Result<(u64, u64), ()> {
    let value = value.strip_prefix("exec-").ok_or(())?;
    let (epoch, sequence) = value.split_once('-').ok_or(())?;
    Ok((u64::from_str_radix(epoch, 16).map_err(|_| ())?, u64::from_str_radix(sequence, 16).map_err(|_| ())?))
}

fn parse_environment(value: &str) -> Option<EnvironmentId> {
    match value {
        "general-1" => Some(EnvironmentId::General1),
        "general-2" => Some(EnvironmentId::General2),
        "general-3" => Some(EnvironmentId::General3),
        "general-4" => Some(EnvironmentId::General4),
        "general-5" => Some(EnvironmentId::General5),
        "payment" => Some(EnvironmentId::Payment),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_ids_are_unique() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 1, workers_per_swamp: 1, rebalance_interval_ms: 10 });
        assert_ne!(runtime.submit(EnvironmentId::General1, "a", WorkCost::default(), 0), runtime.submit(EnvironmentId::General1, "b", WorkCost::default(), 0));
    }

    #[test]
    fn every_environment_gets_its_own_swamp_pool() {
        let runtime = Runtime::new(RuntimeConfig { swamps_per_environment: 2, workers_per_swamp: 1, rebalance_interval_ms: 1000 });
        assert_eq!(runtime.snapshots().len(), 6);
    }

    #[test]
    fn execution_id_round_trips() {
        let id = ExecutionId::from_parts(123, 456);
        let parsed = parse_execution_id(&id.to_string()).unwrap();
        assert_eq!(parsed, (123, 456));
    }
}
