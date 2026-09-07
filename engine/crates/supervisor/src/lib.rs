//! In-process async supervisor.
//!
//! Internal trusted tasks remain Tokio tasks. User-authored `.service` files are
//! separate OS processes owned by `service-runtime`; both systems share the same
//! top-level backend lifecycle.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    Created,
    Initializing,
    ConfigurationLoaded,
    ServicesStarting,
    Ready,
    Running,
    Maintenance,
    ShutdownRequested,
    Stopping,
    Stopped,
}

/// Cloneable lifecycle control plane. `Supervisor::run()` consumes the mutable
/// supervisor task state, while boot/shutdown code retains this handle and can
/// continue publishing truthful lifecycle transitions.
#[derive(Clone)]
pub struct Lifecycle {
    state_tx: watch::Sender<BackendState>,
}

impl Lifecycle {
    pub fn subscribe(&self) -> watch::Receiver<BackendState> {
        self.state_tx.subscribe()
    }

    pub fn set(&self, state: BackendState) {
        if *self.state_tx.borrow() == state {
            return;
        }
        tracing::info!(?state, "backend state transition");
        let _ = self.state_tx.send(state);
    }

    pub fn current(&self) -> BackendState {
        *self.state_tx.borrow()
    }
}

pub type TaskFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
pub type TaskFactory = Box<dyn Fn() -> TaskFuture + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_restarts: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            max_restarts: 10,
        }
    }
}

struct TaskOutcome {
    name: String,
    result: anyhow::Result<()>,
}

struct RestartState {
    attempts: u32,
    next_backoff: Duration,
}

pub struct Supervisor {
    factories: HashMap<String, TaskFactory>,
    restart_states: HashMap<String, RestartState>,
    policy: RestartPolicy,
    join_set: JoinSet<TaskOutcome>,
    lifecycle: Lifecycle,
    id_to_name: HashMap<tokio::task::Id, String>,
}

impl Supervisor {
    pub fn new(policy: RestartPolicy) -> Self {
        let (state_tx, _rx) = watch::channel(BackendState::Created);
        Self {
            factories: HashMap::new(),
            restart_states: HashMap::new(),
            policy,
            join_set: JoinSet::new(),
            lifecycle: Lifecycle { state_tx },
            id_to_name: HashMap::new(),
        }
    }

    pub fn lifecycle(&self) -> Lifecycle {
        self.lifecycle.clone()
    }

    pub fn subscribe_state(&self) -> watch::Receiver<BackendState> {
        self.lifecycle.subscribe()
    }

    pub fn set_state(&self, state: BackendState) {
        self.lifecycle.set(state);
    }

    pub fn register(&mut self, name: impl Into<String>, factory: TaskFactory) {
        let name = name.into();
        self.spawn_task(name.clone(), &factory);
        self.factories.insert(name.clone(), factory);
        self.restart_states.insert(
            name,
            RestartState {
                attempts: 0,
                next_backoff: self.policy.initial_backoff,
            },
        );
    }

    fn spawn_task(&mut self, name: String, factory: &TaskFactory) {
        let fut = factory();
        let name_for_task = name.clone();
        let abort_handle = self.join_set.spawn(async move {
            let result = fut.await;
            TaskOutcome {
                name: name_for_task,
                result,
            }
        });
        self.id_to_name.insert(abort_handle.id(), name);
    }

    pub async fn run(&mut self) {
        while let Some(join_result) = self.join_set.join_next_with_id().await {
            let (name, failure_reason): (String, Option<String>) = match join_result {
                Ok((id, outcome)) => {
                    self.id_to_name.remove(&id);
                    let reason = match &outcome.result {
                        Ok(()) => {
                            tracing::info!(task = %outcome.name, "task exited cleanly");
                            None
                        }
                        Err(err) => Some(format!("returned error: {err:#}")),
                    };
                    (outcome.name, reason)
                }
                Err(join_err) => {
                    let id = join_err.id();
                    let name = self
                        .id_to_name
                        .remove(&id)
                        .unwrap_or_else(|| "<unknown task>".to_string());
                    let reason = if join_err.is_panic() {
                        "panicked".to_string()
                    } else {
                        format!("cancelled/join error: {join_err}")
                    };
                    (name, Some(reason))
                }
            };

            let Some(reason) = failure_reason else {
                self.factories.remove(&name);
                self.restart_states.remove(&name);
                continue;
            };

            tracing::warn!(task = %name, reason = %reason, "task failed");

            let factory = match self.factories.remove(&name) {
                Some(factory) => factory,
                None => {
                    tracing::error!(task = %name, "no factory registered to restart task");
                    continue;
                }
            };

            let restart_state = self
                .restart_states
                .get_mut(&name)
                .expect("restart state must exist alongside factory");

            if restart_state.attempts >= self.policy.max_restarts {
                tracing::error!(
                    task = %name,
                    attempts = restart_state.attempts,
                    "task exceeded max restart attempts, giving up permanently"
                );
                self.restart_states.remove(&name);
                continue;
            }

            let backoff = restart_state.next_backoff;
            restart_state.attempts += 1;
            restart_state.next_backoff =
                (restart_state.next_backoff * 2).min(self.policy.max_backoff);

            tracing::info!(task = %name, delay = ?backoff, attempt = restart_state.attempts, "restarting task after backoff");
            tokio::time::sleep(backoff).await;

            self.spawn_task(name.clone(), &factory);
            self.factories.insert(name, factory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn lifecycle_handle_survives_supervisor_move() {
        let supervisor = Supervisor::new(RestartPolicy::default());
        let lifecycle = supervisor.lifecycle();
        let receiver = lifecycle.subscribe();
        lifecycle.set(BackendState::Running);
        assert_eq!(*receiver.borrow(), BackendState::Running);
    }

    #[tokio::test]
    async fn restarts_a_panicking_task_and_eventually_gives_up() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let mut supervisor = Supervisor::new(RestartPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
            max_restarts: 3,
        });

        supervisor.register(
            "flaky",
            Box::new(move || {
                let attempts = attempts_clone.clone();
                Box::pin(async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    panic!("simulated crash");
                })
            }),
        );

        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor.run()).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn clean_exit_does_not_restart() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let mut supervisor = Supervisor::new(RestartPolicy::default());
        supervisor.register(
            "one_shot",
            Box::new(move || {
                let attempts = attempts_clone.clone();
                Box::pin(async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
        );

        let _ = tokio::time::timeout(Duration::from_millis(200), supervisor.run()).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
