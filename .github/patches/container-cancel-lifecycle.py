from pathlib import Path


def rep(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding='utf-8')
    found = text.count(old)
    if found != count:
        raise SystemExit(f'{path}: expected {count} anchors, found {found}: {old[:120]!r}')
    p.write_text(text.replace(old, new, count), encoding='utf-8')

runtime = 'container-runtime/crates/container-runtime-core/src/runtime.rs'

rep(runtime,
'''#[derive(Default)]
struct ResultStore {
    outcomes: HashMap<String, ExecutionOutcome>,
    order: VecDeque<String>,
    retained_bytes: usize,
}

type SharedResults = Arc<(Mutex<ResultStore>, Condvar)>;
''',
'''#[derive(Default)]
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
''')

rep(runtime,
'''    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    global_queue_changed: Condvar,
    cancelled: Arc<Mutex<HashSet<String>>>,
    generations: Mutex<HashMap<EnvironmentId, u64>>,
''',
'''    global_queue: Mutex<VecDeque<(EnvironmentId, ExecutionTask)>>,
    global_queue_changed: Condvar,
    lifecycle: SharedLifecycle,
    generations: Mutex<HashMap<EnvironmentId, u64>>,
''')

rep(runtime,
'''        let journal = Journal::open();
        let (recovered, max_sequence) = journal.recover();
        let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));
        let results: SharedResults = Arc::new((Mutex::new(ResultStore::default()), Condvar::new()));

        let runner: Runner = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            Arc::new(move |task| {
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled before start".into());
                }
                let output = if cache.contains_artifact(&task.artifact_hash) {
                    match artifact_runner.as_ref() {
                        Some(runner) => runner(task)?,
                        None => run_isolated_worker(task, &cancelled)?,
                    }
                } else if task.work_ms > 0 {
                    run_simulated_work(task, &cancelled)?
                } else {
                    Vec::new()
                };
                if is_cancelled(&cancelled, task) {
                    return Err("execution cancelled".into());
                }
                Ok(output)
            })
        };

        let completion: Completion = {
            let cache = Arc::clone(&cache);
            let cancelled = Arc::clone(&cancelled);
            let journal = Arc::clone(&journal);
            let results = Arc::clone(&results);
            Arc::new(move |task, elapsed_ms, result| {
                let succeeded = result.is_ok();
                let was_cancelled = cancelled
                    .lock()
                    .expect("cancel table poisoned")
                    .remove(&task.id.to_string());
''',
'''        let journal = Journal::open();
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
''')

rep(runtime,
'''            global_queue: Mutex::new(VecDeque::new()),
            global_queue_changed: Condvar::new(),
            cancelled: Arc::clone(&cancelled),
            generations: Mutex::new(generations),
''',
'''            global_queue: Mutex::new(VecDeque::new()),
            global_queue_changed: Condvar::new(),
            lifecycle: Arc::clone(&lifecycle),
            generations: Mutex::new(generations),
''')

rep(runtime,
'''                if runtime.has_environment(environment) {
                    runtime
                        .global_queue
                        .lock()
                        .expect("global queue poisoned")
                        .push_back((environment, task));
''',
'''                if runtime.has_environment(environment) {
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
''')

rep(runtime,
'''        let artifact_hash = artifact_hash.into();
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.journal.append(JournalEvent {
''',
'''        let artifact_hash = artifact_hash.into();
        let id = ExecutionId::new(self.next_execution.fetch_add(1, Ordering::Relaxed));
        self.lifecycle
            .lock()
            .expect("execution lifecycle poisoned")
            .live
            .insert(id.to_string());
        self.journal.append(JournalEvent {
''')

old_cancel = '''    pub fn cancel(&self, execution_id: &str) -> bool {
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
            environment.swamps.iter().any(|swamp| {
                swamp.workers.iter().any(|worker| {
                    worker
                        .current
                        .map(|id| id.to_string() == execution_id)
                        .unwrap_or(false)
                })
            })
        });

        if running {
            self.cancelled
                .lock()
                .expect("cancel table poisoned")
                .insert(execution_id.to_string());
            if let Some(canceller) = self.artifact_canceller.as_ref() {
                if let Err(error) = canceller(execution_id) {
                    tracing::warn!(
                        execution = execution_id,
                        "Environment hard-cancel failed: {error}"
                    );
                }
            }
        }
        if removed_queued || running {
            self.journal.append_cancel_string(execution_id);
            if removed_queued && !running {
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
        } else {
            false
        }
    }
'''
new_cancel = '''    pub fn cancel(&self, execution_id: &str) -> bool {
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
'''
rep(runtime, old_cancel, new_cancel)

rep(runtime,
'''fn is_cancelled(cancelled: &Arc<Mutex<HashSet<String>>>, task: &ExecutionTask) -> bool {
    cancelled
        .lock()
        .expect("cancel table poisoned")
        .contains(&task.id.to_string())
}

fn run_simulated_work(
    task: &ExecutionTask,
    cancelled: &Arc<Mutex<HashSet<String>>>,
) -> Result<Vec<u8>, String> {
''',
'''fn is_cancelled(lifecycle: &SharedLifecycle, task: &ExecutionTask) -> bool {
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
''')
rep(runtime, '        if is_cancelled(cancelled, task) {', '        if is_cancelled(lifecycle, task) {', count=2)
rep(runtime,
'''fn run_isolated_worker(
    task: &ExecutionTask,
    cancelled: &Arc<Mutex<HashSet<String>>>,
) -> Result<Vec<u8>, String> {
''',
'''fn run_isolated_worker(
    task: &ExecutionTask,
    lifecycle: &SharedLifecycle,
) -> Result<Vec<u8>, String> {
''')
# The isolated-worker loop has the remaining cancellation probe.
rep(runtime, '        if is_cancelled(cancelled, task) {', '        if is_cancelled(lifecycle, task) {')

# Supervisor execution ownership must be released even when write_frame fails.
ep = 'container-runtime/crates/container-bin/src/environment_process.rs'
rep(ep,
'''        write_frame(&mut stream, &request)
            .map_err(|error| format!("send Environment execution: {error}"))?;
        let result = read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
            .map_err(|error| format!("read Environment execution result: {error}"))
            .and_then(|response| match response {
                ChildResponse::Finished {
                    request_id: returned,
                    output,
                } if returned == request_id => Ok(output),
                ChildResponse::Error {
                    request_id: Some(returned),
                    code,
                    message,
                } if returned == request_id => Err(format!("{code}: {message}")),
                _ => Err("Environment process returned a mismatched response".into()),
            });
''',
'''        let result = (|| -> Result<Vec<u8>, String> {
            write_frame(&mut stream, &request)
                .map_err(|error| format!("send Environment execution: {error}"))?;
            read_typed::<ChildResponse, _>(&mut BufReader::new(stream))
                .map_err(|error| format!("read Environment execution result: {error}"))
                .and_then(|response| match response {
                    ChildResponse::Finished {
                        request_id: returned,
                        output,
                    } if returned == request_id => Ok(output),
                    ChildResponse::Error {
                        request_id: Some(returned),
                        code,
                        message,
                    } if returned == request_id => Err(format!("{code}: {message}")),
                    _ => Err("Environment process returned a mismatched response".into()),
                })
        })();
''')

# The child also owns worker cleanup. Put all post-registration paths behind one
# result boundary so missing pipes/result-reader failures cannot leave ghost IDs.
rep(ep,
'''    if take_cancelled(cancelled, execution_id)? {
        let _ = child.kill();
        let _ = child.wait();
        active_executions
            .lock()
            .map_err(|_| "Environment active execution table poisoned".to_string())?
            .remove(execution_id);
        return Err("execution cancelled".into());
    }
    let mut worker_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Environment worker stdin unavailable".to_string())?;
    if let Err(error) = write_worker_input(&mut worker_stdin, input) {
        let _ = child.kill();
        let _ = child.wait();
        active_executions
            .lock()
            .map_err(|_| "Environment active execution table poisoned".to_string())?
            .remove(execution_id);
        let _ = take_cancelled(cancelled, execution_id);
        return Err(format!("write Environment worker input: {error}"));
    }
    drop(worker_stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Environment worker stdout unavailable".to_string())?;
    let reader = thread::Builder::new()
        .name("rbe-env-worker-result".into())
        .spawn(move || read_worker_result(&mut BufReader::new(stdout)))
        .map_err(|error| error.to_string())?;
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(timeout_ms.max(1));
    loop {
        if is_cancelled(cancelled, execution_id)? {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            active_executions
                .lock()
                .map_err(|_| "Environment active execution table poisoned".to_string())?
                .remove(execution_id);
            let _ = take_cancelled(cancelled, execution_id);
            return Err("execution cancelled".into());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            active_executions
                .lock()
                .map_err(|_| "Environment active execution table poisoned".to_string())?
                .remove(execution_id);
            return Err(format!(
                "Environment worker timed out after {timeout_ms} ms"
            ));
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => {
                let frame = reader
                    .join()
                    .map_err(|_| "Environment worker result reader panicked".to_string())?
                    .map_err(|error| format!("invalid Environment worker result: {error}"))?;
                active_executions
                    .lock()
                    .map_err(|_| "Environment active execution table poisoned".to_string())?
                    .remove(execution_id);
                let _ = take_cancelled(cancelled, execution_id);
                if !status.success() {
                    return Err(format!("Environment worker exited with status {status}"));
                }
                return match frame {
                    WorkerResultFrame::Success(output) => Ok(output),
                    WorkerResultFrame::Error(message) => Err(message),
                };
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
''',
'''    let result = (|| -> Result<Vec<u8>, String> {
        if take_cancelled(cancelled, execution_id)? {
            let _ = child.kill();
            let _ = child.wait();
            return Err("execution cancelled".into());
        }
        let mut worker_stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Environment worker stdin unavailable".to_string())?;
        if let Err(error) = write_worker_input(&mut worker_stdin, input) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("write Environment worker input: {error}"));
        }
        drop(worker_stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Environment worker stdout unavailable".to_string())?;
        let reader = thread::Builder::new()
            .name("rbe-env-worker-result".into())
            .spawn(move || read_worker_result(&mut BufReader::new(stdout)))
            .map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms.max(1));
        loop {
            if is_cancelled(cancelled, execution_id)? {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("execution cancelled".into());
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!(
                    "Environment worker timed out after {timeout_ms} ms"
                ));
            }
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(status) => {
                    let frame = reader
                        .join()
                        .map_err(|_| "Environment worker result reader panicked".to_string())?
                        .map_err(|error| format!("invalid Environment worker result: {error}"))?;
                    if !status.success() {
                        return Err(format!("Environment worker exited with status {status}"));
                    }
                    return match frame {
                        WorkerResultFrame::Success(output) => Ok(output),
                        WorkerResultFrame::Error(message) => Err(message),
                    };
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
    })();
    active_executions
        .lock()
        .map_err(|_| "Environment active execution table poisoned".to_string())?
        .remove(execution_id);
    let _ = take_cancelled(cancelled, execution_id);
    result
''')
