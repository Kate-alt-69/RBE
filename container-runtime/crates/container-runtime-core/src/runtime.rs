use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{create_dir_all, read_to_string, OpenOptions};
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use environments::EnvironmentId;
use execution_engine::WasmExecutor;
use ipc_protocol::{read_worker_result, write_worker_input, WorkerResultFrame};
use resource_limits::ResourceLimits;
use sandbox_primitives::SandboxPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::ArtifactCache;
use crate::environment::{EnvironmentRuntime, EnvironmentSnapshot, EnvironmentStorage};
use crate::execution::{
    ExecutionId, ExecutionOutcome, ExecutionProvenance, ExecutionTask, WorkCost,
};
use crate::worker::{Canceller, Completion, Runner, WorkerState};

pub const DEFAULT_ENVIRONMENT_STORAGE_BYTES: u64 = 100 * 1024 * 1024;
const JOURNAL_MAX_BYTES: u64 = 32 * 1024 * 1024;
const RESULT_STORE_MAX_RECORDS: usize = 1024;
const RESULT_STORE_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct ResultStore {
    outcomes: HashMap<String, ExecutionOutcome>,
    order: VecDeque<String>,
    retained_bytes: usize,
}

#[derive(Default)]
struct ExecutionLifecycle {
    /// Submitted/recovered executions remain live until an outcome is published.
    /// Cancellation and completion serialize through this one lock, giving
    /// cancel() a real linearization point instead of racing worker snapshots.
    live: HashSet<String>,
    cancelled: HashSet<String>,
}

type SharedResults = Arc<(Mutex<ResultStore>, Condvar)>;
type SharedLifecycle = Arc<Mutex<ExecutionLifecycle>>;

fn outcome_retained_bytes(outcome: &ExecutionOutcome) -> usize {
    outcome
        .output
        .len()
        .saturating_add(outcome.error.as_ref().map_or(0, String::len))
        .saturating_add(64)
}

fn record_execution_outcome(results: &SharedResults, id: &str, outcome: ExecutionOutcome) {
    let (lock, changed) = &**results;
    let mut store = lock.lock().expect("execution result store poisoned");
    if let Some(previous) = store.outcomes.remove(id) {
        store.retained_bytes = store
            .retained_bytes
            .saturating_sub(outcome_retained_bytes(&previous));
        store.order.retain(|candidate| candidate != id);
    }
    store.retained_bytes = store
        .retained_bytes
        .saturating_add(outcome_retained_bytes(&outcome));
    store.outcomes.insert(id.to_string(), outcome);
    store.order.push_back(id.to_string());

    while store.outcomes.len() > RESULT_STORE_MAX_RECORDS
        || store.retained_bytes > RESULT_STORE_MAX_BYTES
    {
        let Some(oldest) = store.order.pop_front() else {
            break;
        };
        if let Some(removed) = store.outcomes.remove(&oldest) {
            store.retained_bytes = store
                .retained_bytes
                .saturating_sub(outcome_retained_bytes(&removed));
        }
    }
    drop(store);
    changed.notify_all();
}

fn journal_path() -> PathBuf {
    runtime_paths::binary_dir()
        .join("data")
        .join("container-runtime")
        .join("execution.journal")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEvent {
    kind: String,
    epoch_ns: u64,
    sequence: u64,
    environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capability_abi: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment_generation: Option<u64>,
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
        if let Some(parent) = path.parent() {
            let _ = create_dir_all(parent);
        }
        Arc::new(Self {
            path,
            lock: Mutex::new(()),
            io: atomic_io::AtomicIo::new(),
        })
    }

    fn append(&self, event: JournalEvent) {
        let _guard = self.lock.lock().expect("journal lock poisoned");
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
        if std::fs::metadata(&self.path)
            .map(|metadata| metadata.len() >= JOURNAL_MAX_BYTES)
            .unwrap_or(false)
        {
            self.compact_locked();
        }
    }

    fn recover(&self) -> (Vec<ExecutionTask>, u64) {
        let _guard = self.lock.lock().expect("journal lock poisoned");
        let Ok(contents) = read_to_string(&self.path) else {
            return (Vec::new(), 0);
        };
        let (pending, max_sequence) = pending_events(&contents);
        if contents.len() as u64 >= JOURNAL_MAX_BYTES {
            self.write_compacted(max_sequence, &pending);
        }
        let recovered = pending.into_iter().filter_map(event_to_task).collect();
        (recovered, max_sequence)
    }

    fn compact_locked(&self) {
        let Ok(contents) = read_to_string(&self.path) else {
            return;
        };
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
        let Ok((epoch_ns, sequence)) = parse_execution_id(execution_id) else {
            return;
        };
        self.append(JournalEvent {
            kind: "cancel".into(),
            epoch_ns,
            sequence,
            environment: "unknown".into(),
            runtime_image: None,
            source_id: None,
            capability_abi: None,
            environment_generation: None,
            artifact_hash: "unknown".into(),
            cpu: 0,
            memory: 0,
            io: 0,
            network: 0,
            work_ms: 0,
            limit_cpu_millis: 0,
            limit_memory_bytes: 0,
            limit_disk_bytes: 0,
            limit_network_bytes: 0,
            limit_max_processes: 0,
            limit_max_file_descriptors: 0,
            limit_wall_time_ms: 0,
        });
    }
}

fn pending_events(contents: &str) -> (Vec<JournalEvent>, u64) {
    let mut latest: HashMap<String, (JournalEvent, bool)> = HashMap::new();
    let mut max_sequence = 0u64;
    for line in contents.lines() {
        let Ok(event) = serde_json::from_str::<JournalEvent>(line) else {
            continue;
        };
        max_sequence = max_sequence.max(event.sequence);
        if event.kind == "checkpoint" {
            continue;
        }
        let id = format!("exec-{:016x}-{:016x}", event.epoch_ns, event.sequence);
        match event.kind.as_str() {
            "queued" => {
                latest.insert(id, (event, true));
            }
            "done" | "cancel" => {
                if let Some(entry) = latest.get_mut(&id) {
                    entry.1 = false;
                }
            }
            _ => {}
        }
    }
    let pending = latest
        .into_values()
        .filter_map(|(event, is_pending)| is_pending.then_some(event))
        .collect();
    (pending, max_sequence)
}

fn checkpoint_event(sequence: u64) -> JournalEvent {
    JournalEvent {
        kind: "checkpoint".into(),
        epoch_ns: 0,
        sequence,
        environment: "checkpoint".into(),
        runtime_image: None,
        source_id: None,
        capability_abi: None,
        environment_generation: None,
        artifact_hash: "checkpoint".into(),
        cpu: 0,
        memory: 0,
        io: 0,
        network: 0,
        work_ms: 0,
        limit_cpu_millis: 0,
        limit_memory_bytes: 0,
        limit_disk_bytes: 0,
        limit_network_bytes: 0,
        limit_max_processes: 0,
        limit_max_file_descriptors: 0,
        limit_wall_time_ms: 0,
    }
}

fn event_to_task(event: JournalEvent) -> Option<ExecutionTask> {
    let environment = parse_environment(&event.environment)?;
    let provenance_environment = event.environment.clone();
    let provenance = match (
        event.runtime_image,
        event.source_id,
        event.capability_abi,
        event.environment_generation,
    ) {
        (Some(runtime_image), Some(source_id), Some(capability_abi), Some(generation)) => {
            Some(ExecutionProvenance {
                runtime_image,
                source_id,
                capability_abi,
                environment: provenance_environment,
                generation,
            })
        }
        _ => None,
    };
    Some(ExecutionTask {
        id: ExecutionId::from_parts(event.epoch_ns, event.sequence),
        environment: environment.to_string(),
        provenance,
        artifact_hash: event.artifact_hash,
        declared_cost: WorkCost {
            cpu: event.cpu,
            memory: event.memory,
            io: event.io,
            network: event.network,
        },
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
        Self {
            general_environments: 5,
            swamps_per_environment: physical_core_count(),
            workers_per_swamp: 1,
            rebalance_interval_ms: 25,
        }
    }
}

pub struct Runtime {
    config: RuntimeConfig,
    next_execution: AtomicU64,
    next_general_environment: AtomicU64,
    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    global_queue_changed: Condvar,
    lifecycle: SharedLifecycle,
    generations: Mutex<HashMap<EnvironmentId, u64>>,
    environments: Vec<EnvironmentRuntime>,
    cache: Arc<ArtifactCache>,
    executor: Arc<WasmExecutor>,
    journal: Arc<Journal>,
    results: SharedResults,
    artifact_canceller: Option<Canceller>,
}

impl Runtime {
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        Self::build(config, None, None)
    }

    /// Build with an external Environment runner and hard-cancellation hook.
    pub fn new_with_runner(
        config: RuntimeConfig,
        artifact_runner: Runner,
        artifact_canceller: Canceller,
    ) -> Arc<Self> {
        Self::build(config, Some(artifact_runner), Some(artifact_canceller))
    }

    fn build(
        config: RuntimeConfig,
        artifact_runner: Option<Runner>,
        artifact_canceller: Option<Canceller>,
    ) -> Arc<Self> {
        let config = RuntimeConfig {
            general_environments: config
                .general_environments
                .clamp(1, EnvironmentId::GENERAL.len()),
            swamps_per_environment: config.swamps_per_environment.max(1),
            workers_per_swamp: config.workers_per_swamp.max(1),
            rebalance_interval_ms: config.rebalance_interval_ms.max(1),
        };
        let active_ids = active_environment_ids(config.general_environments);
        let manage_storage_locally = artifact_runner.is_none();
        let cache = Arc::new(ArtifactCache::default());
        let executor = Arc::new(WasmExecutor::new().expect("failed to initialize WASM executor"));
        let journal = Journal::open();
        let (recovered, max_sequence) = journal.recover();
        let lifecycle: SharedLifecycle = Arc::new(Mutex::new(ExecutionLifecycle::default()));
        let results: SharedResults = Arc::new((Mutex::new(ResultStore::default()), Condvar::new()));

        let runner: Runner = {
            let cache = Arc::clone(&cache);
            let lifecycle = Arc::clone(&lifecycle);
            Arc::new(move |task| {
                if is_cancelled(&lifecycle, task) {
                    return Err("execution cancelled before start".into());
                }
                let output = if cache.contains_artifact(&task.artifact_hash) {
                    match artifact_runner.as_ref() {
                        Some(runner) => runner(task)?,
                        None => run_isolated_worker(task, &lifecycle)?,
                    }
                } else if task.work_ms > 0 {
                    run_simulated_work(task, &lifecycle)?
                } else {
                    Vec::new()
                };
                if is_cancelled(&lifecycle, task) {
                    return Err("execution cancelled".into());
                }
                Ok(output)
            })
        };

        let completion: Completion = {
            let cache = Arc::clone(&cache);
            let lifecycle = Arc::clone(&lifecycle);
            let journal = Arc::clone(&journal);
            let results = Arc::clone(&results);
            Arc::new(move |task, elapsed_ms, result| {
                let succeeded = result.is_ok();
                let was_cancelled = {
                    let mut lifecycle = lifecycle.lock().expect("execution lifecycle poisoned");
                    let execution_id = task.id.to_string();
                    let was_cancelled = lifecycle.cancelled.remove(&execution_id);
                    lifecycle.live.remove(&execution_id);
                    was_cancelled
                };
                if succeeded && !was_cancelled {
                    cache.record(&task.artifact_hash, elapsed_ms, task.declared_cost);
                }
                let outcome = if was_cancelled {
                    ExecutionOutcome {
                        output: Vec::new(),
                        error: Some("execution cancelled".into()),
                        elapsed_ms,
                        cancelled: true,
                    }
                } else {
                    ExecutionOutcome {
                        output: result.as_ref().ok().cloned().unwrap_or_default(),
                        error: result.as_ref().err().cloned(),
                        elapsed_ms,
                        cancelled: false,
                    }
                };
                record_execution_outcome(&results, &task.id.to_string(), outcome);
                journal.append(JournalEvent {
                    kind: if was_cancelled {
                        "cancel".into()
                    } else {
                        "done".into()
                    },
                    epoch_ns: task.id.epoch_ns(),
                    sequence: task.id.sequence(),
                    environment: task.environment.clone(),
                    runtime_image: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.runtime_image.clone()),
                    source_id: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.source_id.clone()),
                    capability_abi: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.capability_abi),
                    environment_generation: task
                        .provenance
                        .as_ref()
                        .map(|provenance| provenance.generation),
                    artifact_hash: task.artifact_hash.clone(),
                    cpu: task.declared_cost.cpu,
                    memory: task.declared_cost.memory,
                    io: task.declared_cost.io,
                    network: task.declared_cost.network,
                    work_ms: elapsed_ms,
                    limit_cpu_millis: task.limits.cpu_millis,
                    limit_memory_bytes: task.limits.memory_bytes,
                    limit_disk_bytes: task.limits.disk_bytes,
                    limit_network_bytes: task.limits.network_bytes,
                    limit_max_processes: task.limits.max_processes,
                    limit_max_file_descriptors: task.limits.max_file_descriptors,
                    limit_wall_time_ms: task.limits.wall_time_ms,
                });
                if let Err(error) = result {
                    if !was_cancelled {
                        tracing::warn!(execution = %task.id, environment = %task.environment, "execution failed: {error}");
                    }
                }
            })
        };

        let environments = active_ids
            .iter()
            .copied()
            .map(|id| {
                EnvironmentRuntime::new(
                    id,
                    config.swamps_per_environment,
                    config.workers_per_swamp,
                    EnvironmentStorage {
                        limit_bytes: DEFAULT_ENVIRONMENT_STORAGE_BYTES,
                        ephemeral: true,
                    },
                    manage_storage_locally,
                    Arc::clone(&runner),
                    Arc::clone(&completion),
                )
            })
            .collect();
        let generations = active_ids.iter().copied().map(|id| (id, 0u64)).collect();
        let runtime = Arc::new(Self {
            config,
            next_execution: AtomicU64::new(max_sequence.saturating_add(1).max(1)),
            next_general_environment: AtomicU64::new(0),
            global_queue: Mutex::new(VecDeque::new()),
            global_queue_changed: Condvar::new(),
            lifecycle: Arc::clone(&lifecycle),
            generations: Mutex::new(generations),
            environments,
            cache,
            executor,
            journal,
            results,
            artifact_canceller,
        });

        for task in recovered {
            if let Some(environment) = parse_environment(&task.environment) {
                if runtime.has_environment(environment) {
                    runtime
                        .lifecycle
                        .lock()
                        .expect("execution lifecycle poisoned")
                        .live
                        .insert(task.id.to_string());
                    runtime
                        .global_queue
                        .lock()
                        .expect("global queue poisoned")
                        .push_back((environment, task));
                } else {
                    tracing::warn!(%environment, execution = %task.id, "recovered task belongs to a disabled environment; leaving it out of the live queue");
                }
            }
        }

        let weak = Arc::downgrade(&runtime);
        let interval = runtime.config.rebalance_interval_ms;
        thread::Builder::new()
            .name("rbe-runtime-scheduler".to_string())
            .spawn(move || {
                while let Some(runtime) = weak.upgrade() {
                    runtime.rebalance_once();
                    let has_queued_work = runtime
                        .snapshots()
                        .iter()
                        .any(|environment| environment.queued != 0);
                    if has_queued_work {
                        thread::sleep(Duration::from_millis(interval));
                        continue;
                    }
                    let queue = runtime.global_queue.lock().expect("global queue poisoned");
                    if queue.is_empty() {
                        drop(
                            runtime
                                .global_queue_changed
                                .wait_timeout(queue, Duration::from_secs(1))
                                .expect("global queue poisoned"),
                        );
                    }
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
            None,
            work_ms,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_with_policy(
        &self,
        environment: EnvironmentId,
        artifact_hash: impl Into<String>,
        cost: WorkCost,
        limits: ResourceLimits,
        sandbox: SandboxPolicy,
        provenance: Option<ExecutionProvenance>,
        work_ms: u64,
        payload: Vec<u8>,
    ) -> ExecutionId {
        let artifact_hash = artifact_hash.into();
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.lifecycle
            .lock()
            .expect("execution lifecycle poisoned")
            .live
            .insert(id.to_string());
        self.journal.append(JournalEvent {
            kind: "queued".into(),
            epoch_ns: id.epoch_ns(),
            sequence: id.sequence(),
            environment: environment.to_string(),
            runtime_image: provenance
                .as_ref()
                .map(|provenance| provenance.runtime_image.clone()),
            source_id: provenance
                .as_ref()
                .map(|provenance| provenance.source_id.clone()),
            capability_abi: provenance
                .as_ref()
                .map(|provenance| provenance.capability_abi),
            environment_generation: provenance.as_ref().map(|provenance| provenance.generation),
            artifact_hash: artifact_hash.clone(),
            cpu: cost.cpu,
            memory: cost.memory,
            io: cost.io,
            network: cost.network,
            work_ms,
            limit_cpu_millis: limits.cpu_millis,
            limit_memory_bytes: limits.memory_bytes,
            limit_disk_bytes: limits.disk_bytes,
            limit_network_bytes: limits.network_bytes,
            limit_max_processes: limits.max_processes,
            limit_max_file_descriptors: limits.max_file_descriptors,
            limit_wall_time_ms: limits.wall_time_ms,
        });
        self.global_queue
            .lock()
            .expect("global queue poisoned")
            .push_back((
                environment,
                ExecutionTask {
                    id,
                    environment: environment.to_string(),
                    provenance,
                    artifact_hash,
                    declared_cost: cost,
                    limits,
                    sandbox,
                    work_ms,
                    payload,
                },
            ));
        self.global_queue_changed.notify_one();
        id
    }

    /// Register immutable WASM under its canonical SHA-256 identity.
    /// Returns whether the exact artifact was already present.
    pub fn register_artifact(&self, artifact_hash: &str, wasm: Vec<u8>) -> Result<bool, String> {
        if artifact_hash.len() != 64
            || !artifact_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("artifact hash must be lowercase SHA-256 hexadecimal".into());
        }
        let computed = hex::encode(Sha256::digest(&wasm));
        if artifact_hash != computed {
            return Err(format!(
                "artifact SHA-256 mismatch: claimed {artifact_hash}, computed {computed}"
            ));
        }
        let already_present = self.cache.contains_artifact(artifact_hash);
        if !already_present {
            self.cache.put_artifact(computed, wasm);
        }
        Ok(already_present)
    }

    pub fn cancel(&self, execution_id: &str) -> bool {
        // Cancellation linearizes against completion here. If completion removes
        // `live` first, this call is too late and returns false. If cancellation
        // marks first, completion must publish a cancelled outcome.
        {
            let mut lifecycle = self.lifecycle.lock().expect("execution lifecycle poisoned");
            if !lifecycle.live.contains(execution_id) {
                return false;
            }
            lifecycle.cancelled.insert(execution_id.to_string());
        }

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

        // If dispatch has already left a queue, route cancellation into the
        // Environment supervisor. Its own pending-cancel table closes the race
        // before execution ownership is registered there.
        if !removed_queued {
            if let Some(canceller) = self.artifact_canceller.as_ref() {
                if let Err(error) = canceller(execution_id) {
                    tracing::warn!(
                        execution = execution_id,
                        "Environment hard-cancel failed: {error}"
                    );
                }
            }
        }

        self.journal.append_cancel_string(execution_id);
        if removed_queued {
            {
                let mut lifecycle = self.lifecycle.lock().expect("execution lifecycle poisoned");
                lifecycle.cancelled.remove(execution_id);
                lifecycle.live.remove(execution_id);
            }
            record_execution_outcome(
                &self.results,
                execution_id,
                ExecutionOutcome {
                    output: Vec::new(),
                    error: Some("execution cancelled before start".into()),
                    elapsed_ms: 0,
                    cancelled: true,
                },
            );
        }
        true
    }

    pub fn wait_for_result(
        &self,
        execution_id: &str,
        timeout: Duration,
    ) -> Option<ExecutionOutcome> {
        let timeout = timeout.max(Duration::from_millis(1));
        let started = Instant::now();
        let (lock, changed) = &*self.results;
        let mut store = lock.lock().expect("execution result store poisoned");
        loop {
            if let Some(outcome) = store.outcomes.get(execution_id) {
                return Some(outcome.clone());
            }
            let remaining = timeout.checked_sub(started.elapsed())?;
            let (next, wait) = changed
                .wait_timeout(store, remaining)
                .expect("execution result store poisoned");
            store = next;
            if wait.timed_out() {
                return store.outcomes.get(execution_id).cloned();
            }
        }
    }

    pub fn restart_environment(&self, id: EnvironmentId) -> usize {
        let Some(environment) = self.environment(id) else {
            return 0;
        };
        let pending = environment.restart();
        let count = pending.len();
        if count != 0 {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            for task in pending {
                queue.push_back((id, task));
            }
            drop(queue);
            self.global_queue_changed.notify_one();
        }
        *self
            .generations
            .lock()
            .expect("generation table poisoned")
            .entry(id)
            .or_default() += 1;
        count
    }

    pub fn environment_generation(&self, id: EnvironmentId) -> u64 {
        self.generations
            .lock()
            .expect("generation table poisoned")
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    /// Resolve the logical `general` profile inside Controller authority. The
    /// caller receives an exact Environment binding and never participates in
    /// generation selection. Payment is deliberately outside this pool.
    pub fn select_general_environment(&self) -> EnvironmentId {
        let sequence = self
            .next_general_environment
            .fetch_add(1, Ordering::Relaxed);
        general_environment_for_sequence(self.config.general_environments, sequence)
    }

    pub fn rebalance_once(&self) {
        let pending = {
            let mut queue = self.global_queue.lock().expect("global queue poisoned");
            queue.drain(..).collect::<Vec<_>>()
        };
        for (environment, task) in pending {
            if let Some(runtime) = self.environment(environment) {
                runtime.enqueue(task);
            } else {
                tracing::warn!(%environment, execution = %task.id, "dropping live dispatch for disabled environment");
            }
        }
        for environment in &self.environments {
            environment.rebalance();
        }
    }

    pub fn is_idle(&self) -> bool {
        if self.global_queue_len() != 0 {
            return false;
        }
        self.snapshots().iter().all(|environment| {
            environment.queued == 0
                && environment.swamps.iter().all(|swamp| {
                    swamp.queued == 0
                        && swamp.workers.iter().all(|worker| {
                            matches!(worker.state, WorkerState::Idle | WorkerState::Stopped)
                        })
                })
        })
    }

    fn environment(&self, id: EnvironmentId) -> Option<&EnvironmentRuntime> {
        self.environments
            .iter()
            .find(|environment| environment.id == id)
    }

    pub fn has_environment(&self, id: EnvironmentId) -> bool {
        self.environment(id).is_some()
    }
    pub fn environment_storage(
        &self,
        id: EnvironmentId,
    ) -> Option<Arc<crate::storage::EnvironmentStorageManager>> {
        self.environment(id).and_then(EnvironmentRuntime::storage)
    }
    pub fn global_queue_len(&self) -> usize {
        self.global_queue
            .lock()
            .expect("global queue poisoned")
            .len()
    }
    pub fn cache(&self) -> Arc<ArtifactCache> {
        Arc::clone(&self.cache)
    }
    pub fn snapshots(&self) -> Vec<EnvironmentSnapshot> {
        self.environments
            .iter()
            .map(|environment| {
                let mut snapshot = environment.snapshot();
                snapshot.generation = self.environment_generation(snapshot.id);
                snapshot
            })
            .collect()
    }
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
    pub fn wasm_executor(&self) -> Arc<WasmExecutor> {
        Arc::clone(&self.executor)
    }
}

fn active_environment_ids(general_count: usize) -> Vec<EnvironmentId> {
    let mut ids =
        EnvironmentId::GENERAL[..general_count.clamp(1, EnvironmentId::GENERAL.len())].to_vec();
    ids.push(EnvironmentId::Payment);
    ids
}

fn general_environment_for_sequence(general_count: usize, sequence: u64) -> EnvironmentId {
    let count = general_count.clamp(1, EnvironmentId::GENERAL.len());
    EnvironmentId::GENERAL[(sequence % count as u64) as usize]
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
                if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let topology = entry.path().join("topology");
                let package = std::fs::read_to_string(topology.join("physical_package_id")).ok();
                let core = std::fs::read_to_string(topology.join("core_id")).ok();
                if let (Some(package), Some(core)) = (package, core) {
                    cores.insert((package.trim().to_string(), core.trim().to_string()));
                }
            }
        }
        if !cores.is_empty() {
            return cores.len();
        }
    }
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
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

fn parse_execution_id(value: &str) -> Result<(u64, u64), ()> {
    let mut parts = value.strip_prefix("exec-").ok_or(())?.split('-');
    let epoch_ns = u64::from_str_radix(parts.next().ok_or(())?, 16).map_err(|_| ())?;
    let sequence = u64::from_str_radix(parts.next().ok_or(())?, 16).map_err(|_| ())?;
    Ok((epoch_ns, sequence))
}

fn is_cancelled(lifecycle: &SharedLifecycle, task: &ExecutionTask) -> bool {
    lifecycle
        .lock()
        .expect("execution lifecycle poisoned")
        .cancelled
        .contains(&task.id.to_string())
}

fn run_simulated_work(
    task: &ExecutionTask,
    lifecycle: &SharedLifecycle,
) -> Result<Vec<u8>, String> {
    let started = Instant::now();
    let work = Duration::from_millis(task.work_ms);
    let timeout = Duration::from_millis(task.limits.wall_time_ms.max(1));
    loop {
        if is_cancelled(lifecycle, task) {
            return Err("execution cancelled".into());
        }
        let elapsed = started.elapsed();
        if elapsed >= work {
            return Ok(Vec::new());
        }
        if elapsed >= timeout {
            return Err(format!(
                "execution timed out after {} ms",
                task.limits.wall_time_ms
            ));
        }
        thread::sleep(Duration::from_millis(10).min(work.saturating_sub(elapsed)));
    }
}

fn run_isolated_worker(
    task: &ExecutionTask,
    lifecycle: &SharedLifecycle,
) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let artifact = &task.artifact_hash;
    let fuel = task.limits.cpu_millis.saturating_mul(10_000).max(1_000_000);
    let memory = task.limits.memory_bytes.max(64 * 1024);
    let mut command = std::process::Command::new(exe);
    command.args([
        "--worker",
        "--artifact",
        artifact,
        "--fuel",
        &fuel.to_string(),
        "--memory",
        &memory.to_string(),
    ]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;

    let write_result = (|| -> Result<(), String> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "isolated worker stdin pipe is unavailable".to_string())?;
        write_worker_input(&mut stdin, &task.payload).map_err(|e| e.to_string())?;
        drop(stdin);
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "failed to send invocation data to isolated worker: {error}"
        ));
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "isolated worker stdout pipe is unavailable".to_string())?;
    let result_reader = thread::Builder::new()
        .name(format!("rbe-worker-result-{}", task.id))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            read_worker_result(&mut reader)
        })
        .map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to start isolated worker result reader: {e}")
        })?;

    let started = Instant::now();
    let timeout = Duration::from_millis(task.limits.wall_time_ms.max(1));

    loop {
        if is_cancelled(lifecycle, task) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = result_reader.join();
            return Err("execution cancelled".into());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = result_reader.join();
            return Err(format!(
                "execution timed out after {} ms",
                task.limits.wall_time_ms
            ));
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let frame = result_reader
                    .join()
                    .map_err(|_| "isolated worker result reader panicked".to_string())?
                    .map_err(|e| {
                        format!("isolated worker returned an invalid result frame: {e}")
                    })?;
                if !status.success() {
                    return Err(format!("isolated worker exited with status {status}"));
                }
                return match frame {
                    WorkerResultFrame::Success(output) => Ok(output),
                    WorkerResultFrame::Error(message) => Err(message),
                };
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn journal_event(with_provenance: bool) -> JournalEvent {
        JournalEvent {
            kind: "queued".into(),
            epoch_ns: 7,
            sequence: 9,
            environment: "general-1".into(),
            runtime_image: with_provenance.then(|| "ab".repeat(32)),
            source_id: with_provenance.then(|| "route:api/me".into()),
            capability_abi: with_provenance.then_some(1),
            environment_generation: with_provenance.then_some(4),
            artifact_hash: "cd".repeat(32),
            cpu: 1,
            memory: 2,
            io: 3,
            network: 4,
            work_ms: 0,
            limit_cpu_millis: 100,
            limit_memory_bytes: 1024,
            limit_disk_bytes: 1024,
            limit_network_bytes: 1024,
            limit_max_processes: 1,
            limit_max_file_descriptors: 16,
            limit_wall_time_ms: 1000,
        }
    }

    #[test]
    fn journal_recovery_preserves_execution_provenance() {
        let task = event_to_task(journal_event(true)).expect("recover task");
        let provenance = task.provenance.expect("recover provenance");
        assert_eq!(provenance.runtime_image, "ab".repeat(32));
        assert_eq!(provenance.source_id, "route:api/me");
        assert_eq!(provenance.environment, "general-1");
        assert_eq!(provenance.generation, 4);
    }

    #[test]
    fn legacy_journal_recovery_remains_unattributed() {
        let task = event_to_task(journal_event(false)).expect("recover legacy task");
        assert!(task.provenance.is_none());
    }

    #[test]
    fn general_profile_round_robins_only_configured_general_environments() {
        assert_eq!(
            general_environment_for_sequence(3, 0),
            EnvironmentId::General1
        );
        assert_eq!(
            general_environment_for_sequence(3, 1),
            EnvironmentId::General2
        );
        assert_eq!(
            general_environment_for_sequence(3, 2),
            EnvironmentId::General3
        );
        assert_eq!(
            general_environment_for_sequence(3, 3),
            EnvironmentId::General1
        );
        for sequence in 0..32 {
            assert_ne!(
                general_environment_for_sequence(5, sequence),
                EnvironmentId::Payment
            );
        }
    }

    #[test]
    fn general_profile_respects_single_environment_configuration() {
        for sequence in 0..8 {
            assert_eq!(
                general_environment_for_sequence(1, sequence),
                EnvironmentId::General1
            );
        }
    }
}
