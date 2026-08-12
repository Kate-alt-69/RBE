//! In-process async supervisor.
//!
//! Implements migration-plan §7: instead of the Node Bootstrap Manager's
//! "spawn a child OS process per service" model, services here are
//! `tokio` tasks registered with this supervisor. A panicking or
//! error-returning task doesn't take the process down — it's restarted
//! with exponential backoff, same fault-isolation guarantee as the
//! subprocess model, none of the IPC overhead.
//!
//! Reminder (§3, non-negotiable): this is for *internal* services
//! (email, future bootstrap services). The container runtime is a real
//! separate process, on purpose, and never goes through this supervisor.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;

/// Top-level backend lifecycle state, per migration-plan §3.2 / the
/// handbook's "Backend Lifecycle" state machine. `main.rs`'s `boot()`
/// drives this forward; the health endpoint reads it back out via
/// [`Supervisor::subscribe_state`].
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

pub type TaskFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
pub type TaskFactory = Box<dyn Fn() -> TaskFuture + Send + Sync>;

/// Restart policy for a supervised task. Deliberately simple/hand-rolled
/// per §2.1 — reach for the `backoff` crate only if this needs to grow
/// jitter, per-error-type policies, etc.
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

/// The supervisor itself. Not `Clone` — hold one instance in `AppState`
/// and give out [`watch::Receiver<BackendState>`] handles to whoever
/// needs to observe state (e.g. the health-check route).
pub struct Supervisor {
    factories: HashMap<String, TaskFactory>,
    restart_states: HashMap<String, RestartState>,
    policy: RestartPolicy,
    join_set: JoinSet<TaskOutcome>,
    state_tx: watch::Sender<BackendState>,
    // A panicking task never gets to construct `TaskOutcome` (the panic
    // happens mid-future), so on the `JoinError` path all we get back
    // from `join_next_with_id` is a task `Id` — this map is how we turn
    // that back into a name we can look up in `factories`.
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
            state_tx,
            id_to_name: HashMap::new(),
        }
    }

    pub fn subscribe_state(&self) -> watch::Receiver<BackendState> {
        self.state_tx.subscribe()
    }

    pub fn set_state(&self, state: BackendState) {
        tracing::info!(?state, "backend state transition");
        // A closed channel (no receivers yet) is fine — ignore the error.
        let _ = self.state_tx.send(state);
    }

    /// Register a task and spawn it immediately. `factory` must be able
    /// to produce a *fresh* future on every call, since a future can
    /// only be polled to completion once — that's how restarts work.
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
        // No manual panic-catching here: `JoinSet` already reports a
        // panicking task as `Err(JoinError)` from `join_next_with_id()`
        // (see `run()` below) — that's the mechanism, not anything at
        // the future level.
        let abort_handle = self.join_set.spawn(async move {
            let result = fut.await;
            TaskOutcome {
                name: name_for_task,
                result,
            }
        });
        self.id_to_name.insert(abort_handle.id(), name);
    }

    /// Drives supervision forever (or until every task has permanently
    /// failed). Call this from `main.rs` after registering the initial
    /// set of services; later phases can extend this to also accept
    /// late registrations via a channel.
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
                // Clean exit — don't restart. Remove bookkeeping.
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
                self.factories.remove(&name);
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
            self.factories.insert(name.clone(), factory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

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

        // Bound the test run so it can't hang if restart logic breaks.
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor.run()).await;

        // Initial spawn + up to max_restarts retries.
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
