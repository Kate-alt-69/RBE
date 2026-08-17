use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::execution::{ExecutionId, ExecutionTask};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState { Idle, Running, Stopped }

#[derive(Debug, Clone, Copy)]
pub struct WorkerSnapshot {
    pub id: usize,
    pub state: WorkerState,
    pub completed: u64,
    pub failed: u64,
    pub total_ms: u64,
    pub current: Option<ExecutionId>,
}

struct WorkerCommand { task: ExecutionTask }

pub type Runner = Arc<dyn Fn(&ExecutionTask) -> Result<(), String> + Send + Sync + 'static>;
type Completion = Arc<dyn Fn(&ExecutionTask, u64, Result<(), String>) + Send + Sync + 'static>;

pub struct Worker {
    id: usize,
    tx: mpsc::Sender<WorkerCommand>,
    state: Arc<Mutex<WorkerState>>,
    current: Arc<Mutex<Option<ExecutionId>>>,
    completed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    total_ms: Arc<AtomicU64>,
}

impl Worker {
    pub fn new(id: usize, runner: Runner, on_complete: Completion) -> Self {
        let (tx, rx) = mpsc::channel::<WorkerCommand>();
        let state = Arc::new(Mutex::new(WorkerState::Idle));
        let current = Arc::new(Mutex::new(None));
        let completed = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let total_ms = Arc::new(AtomicU64::new(0));

        let thread_state = Arc::clone(&state);
        let thread_current = Arc::clone(&current);
        let thread_completed = Arc::clone(&completed);
        let thread_failed = Arc::clone(&failed);
        let thread_total_ms = Arc::clone(&total_ms);

        thread::Builder::new()
            .name(format!("rbe-worker-{id}"))
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    *thread_state.lock().expect("worker state poisoned") = WorkerState::Running;
                    *thread_current.lock().expect("worker current poisoned") = Some(command.task.id);

                    let started = Instant::now();
                    let result = runner(&command.task);
                    let elapsed_ms = started.elapsed().as_millis() as u64;

                    if result.is_ok() {
                        thread_completed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        thread_failed.fetch_add(1, Ordering::Relaxed);
                    }
                    thread_total_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
                    on_complete(&command.task, elapsed_ms, result);

                    *thread_current.lock().expect("worker current poisoned") = None;
                    *thread_state.lock().expect("worker state poisoned") = WorkerState::Idle;
                }

                *thread_state.lock().expect("worker state poisoned") = WorkerState::Stopped;
            })
            .expect("failed to start worker thread");

        Self { id, tx, state, current, completed, failed, total_ms }
    }

    pub fn is_idle(&self) -> bool {
        *self.state.lock().expect("worker state poisoned") == WorkerState::Idle
    }

    pub fn try_send(&self, task: ExecutionTask) -> bool {
        if !self.is_idle() { return false; }
        self.tx.send(WorkerCommand { task }).is_ok()
    }

    pub fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            id: self.id,
            state: *self.state.lock().expect("worker state poisoned"),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            total_ms: self.total_ms.load(Ordering::Relaxed),
            current: *self.current.lock().expect("worker current poisoned"),
        }
    }
}
