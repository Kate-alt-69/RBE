use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{create_dir_all, read_to_string, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use environments::EnvironmentId;
use execution_engine::WasmExecutor;
use resource_limits::ResourceLimits;
use sandbox_primitives::SandboxPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::ArtifactCache;
use crate::environment::{EnvironmentRuntime, EnvironmentSnapshot, EnvironmentStorage};
use crate::execution::{ExecutionId, ExecutionTask, WorkCost};
use crate::worker::{Runner, WorkerState};

const DEFAULT_ENVIRONMENT_STORAGE_BYTES: u64 = 100 * 1024 * 1024;
const JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;

fn journal_path() -> PathBuf {
    runtime_paths::binary_dir().join("data").join("container-runtime").join("execution.journal")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    limit_cpu_millis: u64,
    limit_memory_bytes: u64,
    limit_disk_bytes: u64,
    limit_network_bytes: u64,
    limit_max_processes: u32,
    limit_max_file_descriptors: u32,
    limit_wall_time_ms: u64,
}

struct Journal {
    path: PathBuf,
    lock: Mutex<()>,
    io: atomic_io::AtomicIo,
}

impl Journal {
    fn open() -> Arc<Self> {
        let path = journal_path();
        if let Some(parent) = path.parent() { let _ = create_dir_all(parent); }
        Arc::new(Self { path, lock: Mutex::new(()), io: atomic_io::AtomicIo::new() })
    }

    fn append(&self, event: JournalEvent) {
        let _guard = self.lock.lock().expect("journal lock poisoned");
        let Ok(line) = serde_json::to_string(&event) else { return; };
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
        if std::fs::metadata(&self.path).map(|metadata| metadata.len() >= JOURNAL_MAX_BYTES).unwrap_or(false) {
            self.compact_locked();
        }
    }

    fn recover(&self) -> (Vec<ExecutionTask>, u64) {
        let _guard = self.lock.lock().expect("journal lock poisoned");
        let Ok(contents) = read_to_string(&self.path) else { return (Vec::new(), 0); };
        let (pending, max_sequence) = pending_events(&contents);
        if contents.len() as u64 >= JOURNAL_MAX_BYTES {
            self.write_compacted(max_sequence, &pending);
        }
        let recovered = pending.into_iter().filter_map(event_to_task).collect();
        (recovered, max_sequence)
    }

    fn compact_locked(&self) {
        let Ok(contents) = read_to_string(&self.path) else { return; };
        let (pending, max_sequence) = pending_events(&contents);
        self.write_compacted(max_sequence, &pending);
    }

    fn write_compacted(&self, max_sequence: u64, pending: &[JournalEvent]) {
        let checkpoint = checkpoint_event(max_sequence);
        let mut output = String::new();
        if let Ok(line) = serde_json::to_string(&checkpoint) {
            output.push_str(&line);
            output.push('\n');
        }
        for event in pending {
            if let Ok(line) = serde_json::to_string(event) {
                output.push_str(&line);
                output.push('\n');
            }
        }
        let _ = self.io.write_atomic(&self.path, output.as_bytes());
    }

    fn append_cancel_string(&self, execution_id: &str) {
        let Ok((epoch_ns, sequence)) = parse_execution_id(execution_id) else { return; };
        self.append(JournalEvent {
            kind: "cancel".into(), epoch_ns, sequence,
            environment: "unknown".into(), artifact_hash: "unknown".into(),
            cpu: 0, memory: 0, io: 0, network: 0, work_ms: 0,
            limit_cpu_millis: 0, limit_memory_bytes: 0, limit_disk_bytes: 0,
            limit_network_bytes: 0, limit_max_processes: 0, limit_max_file_descriptors: 0,
            limit_wall_time_ms: 0,
        });
    }
}

fn pending_events(contents: &str) -> (Vec<JournalEvent>, u64) {
    let mut latest: HashMap<String, (JournalEvent, bool)> = HashMap::new();
    let mut max_sequence = 0u64;
    for line in contents.lines() {
        let Ok(event) = serde_json::from_str::<JournalEvent>(line) else { continue; };
        max_sequence = max_sequence.max(event.sequence);
        if event.kind == "checkpoint" { continue; }
        let id = format!("exec-{:016x}-{:016x}", event.epoch_ns, event.sequence);
        match event.kind.as_str() {
            "queued" => { latest.insert(id, (event, true)); }
            "done" | "cancel" => {
                if let Some(entry) = latest.get_mut(&id) { entry.1 = false; }
            }
            _ => {}
        }
    }
    let pending = latest.into_values().filter_map(|(event, is_pending)| is_pending.then_some(event)).collect();
    (pending, max_sequence)
}

fn checkpoint_event(sequence: u64) -> JournalEvent {
    JournalEvent {
        kind: "checkpoint".into(), epoch_ns: 0, sequence,
        environment: "checkpoint".into(), artifact_hash: "checkpoint".into(),
        cpu: 0, memory: 0, io: 0, network: 0, work_ms: 0,
        limit_cpu_millis: 0, limit_memory_bytes: 0, limit_disk_bytes: 0,
        limit_network_bytes: 0, limit_max_processes: 0, limit_max_file_descriptors: 0,
        limit_wall_time_ms: 0,
    }
}

fn event_to_task(event: JournalEvent) -> Option<ExecutionTask> {
    let environment = parse_environment(&event.environment)?;
    Some(ExecutionTask {
        id: ExecutionId::from_parts(event.epoch_ns, event.sequence),
        environment: environment.to_string(),
        artifact_hash: event.artifact_hash,
        declared_cost: WorkCost { cpu: event.cpu, memory: event.memory, io: event.io, network: event.network },
        limits: ResourceLimits {
            cpu_millis: event.limit_cpu_millis,
            memory_bytes: event.limit_memory_bytes,
            disk_bytes: event.limit_disk_bytes,
            network_bytes: event.limit_network_bytes,
            max_processes: event.limit_max_processes,
            max_file_descriptors: event.limit_max_file_descriptors,
            wall_time_ms: event.limit_wall_time_ms,
        },
        sandbox: SandboxPolicy::default(),
        work_ms: event.work_ms,
        payload: Vec::new(),
    })
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub general_environments: usize,
    pub swamps_per_environment: usize,
    pub workers_per_swamp: usize,
    pub rebalance_interval_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self { general_environments: 5, swamps_per_environment: physical_core_count(), workers_per_swamp: 1, rebalance_interval_ms: 25 }
    }
}

pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    generations: Mutex<HashMap<EnvironmentId, u64>>,
    environments: Vec<EnvironmentRuntime>,
    cache: Arc<ArtifactCache>,
    executor: Arc<WasmExecutor>,
    journal: Arc<Journal>,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        let config = RuntimeConfig {
            general_environments: config.general_environments.clamp(1, EnvironmentId::GENERAL.len()),
            swamps_per_environment: config.swamps_per_environment.max(1),
            workers_per_swamp: config.workers_per_swamp.max(1),
            rebalance_interval_ms: config.rebalance_interval_ms.max(1),
        };
        let active_ids = active_environment_ids(config.general_environments);
        let cache = Arc::new(ArtifactCache::default());
        let executor = Arc::new(WasmExecutor::new().expect("failed to initialize WASM executor"));
        let journal = Journal::open();
        let (recovered, max_sequence) = journal.recover();
        let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));

        let runner: Runner = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            Arc::new(move |task| {
                if is_cancelled(&cancelled, task) { return Err("execution cancelled before start".into()); }
                if cache.contains_artifact(&task.artifact_hash) {
                    run_isolated_worker(task, &cancelled)?;
                } else if task.work_ms > 0 {
                    run_simulated_work(task, &cancelled)?;
                }
                if is_cancelled(&cancelled, task) { return Err("execution cancelled".into()); }
                Ok(())
            })
        };

        let completion: Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static> = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            let journal = Arc::clone(&journal);
            Arc::new(move |task, elapsed_ms, result| {
                let succeeded = result.is_ok();
                let was_cancelled = cancelled.lock().expect("cancel table poisoned").remove(&task.id.to_string());
                if succeeded && !was_cancelled {
                    cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                }
                journal.append(JournalEvent {
                    kind: if was_cancelled { "cancel".into() } else { "done".into() },
                    epoch_ns: task.id.epoch_ns(), sequence: task.id.sequence(), environment: task.environment.clone(), artifact_hash: task.artifact_hash.clone(),
                    cpu: task.declared_cost.cpu, memory: task.declared_cost.memory, io: task.declared_cost.io, network: task.declared_cost.network, work_ms: elapsed_ms,
                    limit_cpu_millis: task.limits.cpu_millis, limit_memory_bytes: task.limits.memory_bytes, limit_disk_bytes: task.limits.disk_bytes,
                    limit_network_bytes: task.limits.network_bytes, limit_max_processes: task.limits.max_processes, limit_max_file_descriptors: task.limits.max_file_descriptors,
                    limit_wall_time_ms: task.limits.wall_time_ms,
                });
                if let Err(error) = result {
                    if !was_cancelled { tracing::warn!(execution = %task.id, environment = %task.environment, "execution failed: {error}"); }
                }
            })
        };

        let environments = active_ids.iter().copied().map(|id| EnvironmentRuntime::new(
            id,
            config.swamps_per_environment,
            config.workers_per_swamp,
            EnvironmentStorage { limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES, ephemeral: true },
            Arc::clone(&runner),
            Arc::clone(&completion),
        )).collect();
        let generations = active_ids.iter().copied().map(|id| (id, 0u64)).collect();
        let runtime = Arc::new(Self {
            config,
            next_execution: AtomicU64::new(max_sequence.saturating_add(1).max(1)),
            global_queue: Mutex::new(VecDeque::new()),
            cancelled: Arc::clone(&cancelled),
            generations: Mutex::new(generations),
            environments,
            cache,
            executor,
            journal,
        });

        for task in recovered {
            if let Some(environment) = parse_environment(&task.environment) {
                if runtime.has_environment(environment) {
                    runtime.global_queue.lock().expect("global queue poisoned").push_back((environment, task));
                } else {
                    tracing::warn!(%environment, execution = %task.id, "recovered task belongs to a disabled environment; leaving it out of the live queue");
                }
            }
        }

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
        let claimed_artifact_hash = artifact_hash.into();
        let artifact_hash = if payload.is_empty() {
            claimed_artifact_hash
        } else {
            let computed = hex::encode(Sha256::digest(&payload));
            if !claimed_artifact_hash.is_empty() && claimed_artifact_hash != computed {
                tracing::warn!(claimed = %claimed_artifact_hash, computed = %computed, "artifact hash did not match payload; using content hash");
            }
            self.cache.put_artifact(computed.clone(), payload);
            computed
        };
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.journal.append(JournalEvent {
            kind: "queued".into(), epoch_ns: id.epoch_ns(), sequence: id.sequence(), environment: environment.to_string(), artifact_hash: artifact_hash.clone(),
            cpu: cost.cpu, memory: cost.memory, io: cost.io, network: cost.network, work_ms,
            limit_cpu_millis: limits.cpu_millis, limit_memory_bytes: limits.memory_bytes, limit_disk_bytes: limits.disk_bytes,
            limit_network_bytes: limits.network_bytes, limit_max_processes: limits.max_processes, limit_max_file_descriptors: limits.max_file_descriptors,
            limit_wall_time_ms: limits.wall_time_ms,
        });
        self.global_queue.lock().expect("global queue poisoned").push_back((environment, ExecutionTask { id, environment: environment.to_string(), artifact_hash, declared_cost: cost, limits, sandbox, work_ms, payload: Vec::new() }));
        id
    }

    pub fn register_artifact(&self, artifact_hash: impl Into<String>, wasm: Vec<u8>) {
        let claimed = artifact_hash.into();
        let computed = hex::encode(Sha256::digest(&wasm));
        if !claimed.is_empty() && claimed != computed {
            tracing::warn!(claimed = %claimed, computed = %computed, "registered artifact hash did not match content; storing by content hash only");
        }
        self.cache.put_artifact(computed, wasm);
    }

    pub fn cancel(&self, execution_id: &str) -> bool {
        let mut removed_queued = false;
        {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            let before = queue.len();
            queue.retain(|(_, task)| task.id.to_string() != execution_id);
            removed_queued |= before != queue.len();
        }
        for environment in &self.environments {
            removed_queued |= environment.cancel_queued_by_string(execution_id);
        }

        let running = self.snapshots().iter().any(|environment| {
            environment.swamps.iter().any(|swamp| swamp.workers.iter().any(|worker| {
                worker.current.map(|id| id.to_string() == execution_id).unwrap_or(false)
            }))
        });

        if running {
            self.cancelled.lock().expect("cancel table poisoned").insert(execution_id.to_string());
        }
        if removed_queued || running {
            self.journal.append_cancel_string(execution_id);
            true
        } else {
            false
        }
    }

    pub fn restart_environment(&self, id: EnvironmentId) -> usize {
        let Some(environment) = self.environment(id) else { return 0; };
        let pending = environment.restart();
        let count = pending.len();
        if count != 0 {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            for task in pending { queue.push_back((id, task)); }
        }
        *self.generations.lock().expect("generation table poisoned").entry(id).or_default() += 1;
        count
    }

    pub fn environment_generation(&self, id: EnvironmentId) -> u64 {
        self.generations.lock().expect("generation table poisoned").get(&id).copied().unwrap_or(0)
    }

    pub fn rebalance_once(&self) {
        let pending = { let mut queue = self.global_queue.lock().expect("global queue poisoned"); queue.drain(..).collect::<Vec<_>>() };
        for (environment, task) in pending {
            if let Some(runtime) = self.environment(environment) { runtime.enqueue(task); }
            else { tracing::warn!(%environment, execution = %task.id, "dropping live dispatch for disabled environment"); }
        }
        for environment in &self.environments { environment.rebalance(); }
    }

    pub fn is_idle(&self) -> bool {
        if self.global_queue_len() != 0 { return false; }
        self.snapshots().iter().all(|environment| {
            environment.queued == 0 && environment.swamps.iter().all(|swamp| {
                swamp.queued == 0 && swamp.workers.iter().all(|worker| matches!(worker.state, WorkerState::Idle | WorkerState::Stopped))
            })
        })
    }

    fn environment(&self, id: EnvironmentId) -> Option<&EnvironmentRuntime> {
        self.environments.iter().find(|environment| environment.id == id)
    }

    pub fn has_environment(&self, id: EnvironmentId) -> bool { self.environment(id).is_some() }
    pub fn global_queue_len(&self) -> usize { self.global_queue.lock().expect("global queue poisoned").len() }
    pub fn cache(&self) -> Arc<ArtifactCache> { Arc::clone(&self.cache) }
    pub fn snapshots(&self) -> Vec<EnvironmentSnapshot> {
        self.environments.iter().map(|environment| {
            let mut snapshot = environment.snapshot();
            snapshot.generation = self.environment_generation(snapshot.id);
            snapshot
        }).collect()
    }
    pub fn config(&self) -> &RuntimeConfig { &self.config }
    pub fn wasm_executor(&self) -> Arc<WasmExecutor> { Arc::clone(&self.executor) }
}

fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {
    let mut ids = EnvironmentId::GENERAL[..general_count.clamp(1, EnvironmentId::GENERAL.len())].to_vec();
    ids.push(EnvironmentId::Payment);
    ids
}

fn physical_core_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        use std::collections::BTreeSet;
        let mut cores = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) { continue; }
                let topology = entry.path().join("topology");
                let package = std::fs::read_to_string(topology.join("physical_package_id")).ok();
                let core = std::fs::read_to_string(topology.join("core_id")).ok();
                if let (Some(package), Some(core)) = (package, core) { cores.insert((package.trim().to_string(), core.trim().to_string())); }
            }
        }
        if !cores.is_empty() { return cores.len(); }
    }
    std::thread::available_parallelism().map(|value| value.get()).unwrap_or(1)
}

fn parse_environment(value: &str) -> Option<EnvironmentId> {
    match value {
        "general-1" => Some(EnvironmentId::General1), "general-2" => Some(EnvironmentId::General2), "general-3" => Some(EnvironmentId::General3),
        "general-4" => Some(EnvironmentId::General4), "general-5" => Some(EnvironmentId::General5), "payment" => Some(EnvironmentId::Payment), _ => None,
    }
}

fn parse_execution_id(value: &str) -> Result<(u64, u64), ()> {
    let mut parts = value.strip_prefix("exec-").ok_or(())?.split('-');
    let epoch_ns = u64::from_str_radix(parts.next().ok_or(())?, 16).map_err(|_| ())?;
    let sequence = u64::from_str_radix(parts.next().ok_or(())?, 16).map_err(|_| ())?;
    Ok((epoch_ns, sequence))
}

fn is_cancelled(cancelled: &Arc<Mutex<HashSet<String>>>, task: &ExecutionTask) -> bool {
    cancelled.lock().expect("cancel table poisoned").contains(&task.id.to_string())
}

fn run_simulated_work(task: &ExecutionTask, cancelled: &Arc<Mutex<HashSet<String>>>) -> Result<(), String> {
    let started = Instant::now();
    let work = Duration::from_millis(task.work_ms);
    let timeout = Duration::from_millis(task.limits.wall_time_ms.max(1));
    loop {
        if is_cancelled(cancelled, task) { return Err("execution cancelled".into()); }
        let elapsed = started.elapsed();
        if elapsed >= work { return Ok(()); }
        if elapsed >= timeout { return Err(format!("execution timed out after {} ms", task.limits.wall_time_ms)); }
        thread::sleep(Duration::from_millis(10).min(work.saturating_sub(elapsed)));
    }
}

fn run_isolated_worker(task: &ExecutionTask, cancelled: &Arc<Mutex<HashSet<String>>>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let artifact = &task.artifact_hash;
    let fuel = task.limits.cpu_millis.saturating_mul(10_000).max(1_000_000);
    let memory = task.limits.memory_bytes.max(64 * 1024);
    let mut command = std::process::Command::new(exe);
    command.args(["--worker", "--artifact", artifact, "--fuel", &fuel.to_string(), "--memory", &memory.to_string()]);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let started = Instant::now();
    let timeout = Duration::from_millis(task.limits.wall_time_ms.max(1));

    loop {
        if is_cancelled(cancelled, task) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("execution cancelled".into());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("execution timed out after {} ms", task.limits.wall_time_ms));
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("isolated worker exited with status {status}")),
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}
