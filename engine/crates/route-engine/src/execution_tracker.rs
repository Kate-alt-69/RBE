//! Runtime invocation/recursion tracking for REL.
//!
//! Symbol cycles are legal. This tracker only aborts budget violations or a
//! wait on a value that is still being produced by the same invocation chain.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::dependency_graph::SymbolId;
use crate::server_policy::RecursionPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvocationId(u64);

impl InvocationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct InvocationSnapshot {
    pub id: InvocationId,
    pub parent_id: Option<InvocationId>,
    pub symbol: SymbolId,
    pub arguments_fingerprint: u64,
    pub depth: u64,
    pub operation_count: u64,
    pub waiting_for: Option<String>,
    pub producing: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationError {
    Unknown(InvocationId),
    DepthExceeded { limit: u64 },
    RepeatedSymbolDepthExceeded { symbol: String, limit: u64 },
    OperationBudgetExceeded { limit: u64 },
    CyclicComputation { key: String, producer: InvocationId },
}

impl fmt::Display for InvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(formatter, "unknown REL invocation {}", id.get()),
            Self::DepthExceeded { limit } => {
                write!(formatter, "REL recursion depth exceeded {limit}")
            }
            Self::RepeatedSymbolDepthExceeded { symbol, limit } => write!(
                formatter,
                "REL repeated-symbol recursion depth for `{symbol}` exceeded {limit}"
            ),
            Self::OperationBudgetExceeded { limit } => {
                write!(formatter, "REL invocation operation budget exceeded {limit}")
            }
            Self::CyclicComputation { key, producer } => write!(
                formatter,
                "cyclic computation: value `{key}` is still being produced by invocation {} in the current dependency chain",
                producer.get()
            ),
        }
    }
}

impl std::error::Error for InvocationError {}

#[derive(Default)]
struct TrackerState {
    invocations: BTreeMap<InvocationId, InvocationSnapshot>,
    producers: BTreeMap<String, InvocationId>,
}

pub struct InvocationTracker {
    policy: RecursionPolicy,
    next_id: AtomicU64,
    state: Mutex<TrackerState>,
}

impl InvocationTracker {
    pub fn new(policy: RecursionPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            next_id: AtomicU64::new(1),
            state: Mutex::new(TrackerState::default()),
        })
    }

    pub fn begin(
        self: &Arc<Self>,
        parent_id: Option<InvocationId>,
        symbol: SymbolId,
        arguments_fingerprint: u64,
    ) -> Result<InvocationGuard, InvocationError> {
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        let depth = match parent_id {
            Some(parent) => state
                .invocations
                .get(&parent)
                .ok_or(InvocationError::Unknown(parent))?
                .depth
                .saturating_add(1),
            None => 1,
        };
        if depth > self.policy.max_depth {
            return Err(InvocationError::DepthExceeded {
                limit: self.policy.max_depth,
            });
        }

        let repeated = repeated_symbol_depth(&state, parent_id, &symbol)? + 1;
        if repeated > self.policy.repeated_symbol_depth {
            return Err(InvocationError::RepeatedSymbolDepthExceeded {
                symbol: format!("{}::{}", symbol.source, symbol.name),
                limit: self.policy.repeated_symbol_depth,
            });
        }

        let id = InvocationId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.invocations.insert(
            id,
            InvocationSnapshot {
                id,
                parent_id,
                symbol,
                arguments_fingerprint,
                depth,
                operation_count: 0,
                waiting_for: None,
                producing: BTreeSet::new(),
            },
        );
        drop(state);
        Ok(InvocationGuard {
            tracker: self.clone(),
            id,
            finished: false,
        })
    }

    pub fn operation(&self, id: InvocationId) -> Result<u64, InvocationError> {
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        let invocation = state
            .invocations
            .get_mut(&id)
            .ok_or(InvocationError::Unknown(id))?;
        invocation.operation_count = invocation.operation_count.saturating_add(1);
        if invocation.operation_count > self.policy.operation_budget {
            return Err(InvocationError::OperationBudgetExceeded {
                limit: self.policy.operation_budget,
            });
        }
        Ok(invocation.operation_count)
    }

    pub fn producing(&self, id: InvocationId, key: impl Into<String>) -> Result<(), InvocationError> {
        let key = key.into();
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        if !state.invocations.contains_key(&id) {
            return Err(InvocationError::Unknown(id));
        }
        state.producers.insert(key.clone(), id);
        state
            .invocations
            .get_mut(&id)
            .expect("checked above")
            .producing
            .insert(key);
        Ok(())
    }

    pub fn ready(&self, id: InvocationId, key: &str) -> Result<(), InvocationError> {
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        if !state.invocations.contains_key(&id) {
            return Err(InvocationError::Unknown(id));
        }
        if state.producers.get(key) == Some(&id) {
            state.producers.remove(key);
        }
        state
            .invocations
            .get_mut(&id)
            .expect("checked above")
            .producing
            .remove(key);
        Ok(())
    }

    pub fn wait_for(&self, id: InvocationId, key: impl Into<String>) -> Result<(), InvocationError> {
        let key = key.into();
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        if !state.invocations.contains_key(&id) {
            return Err(InvocationError::Unknown(id));
        }
        if let Some(producer) = state.producers.get(&key).copied() {
            if producer == id || is_ancestor(&state, producer, id)? {
                return Err(InvocationError::CyclicComputation { key, producer });
            }
        }
        state
            .invocations
            .get_mut(&id)
            .expect("checked above")
            .waiting_for = Some(key);
        Ok(())
    }

    pub fn clear_wait(&self, id: InvocationId) -> Result<(), InvocationError> {
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        state
            .invocations
            .get_mut(&id)
            .ok_or(InvocationError::Unknown(id))?
            .waiting_for = None;
        Ok(())
    }

    pub fn snapshot(&self, id: InvocationId) -> Option<InvocationSnapshot> {
        self.state
            .lock()
            .expect("REL invocation tracker poisoned")
            .invocations
            .get(&id)
            .cloned()
    }

    fn finish(&self, id: InvocationId) {
        let mut state = self.state.lock().expect("REL invocation tracker poisoned");
        if let Some(invocation) = state.invocations.remove(&id) {
            for key in invocation.producing {
                if state.producers.get(&key) == Some(&id) {
                    state.producers.remove(&key);
                }
            }
        }
    }
}

pub struct InvocationGuard {
    tracker: Arc<InvocationTracker>,
    id: InvocationId,
    finished: bool,
}

impl InvocationGuard {
    pub fn id(&self) -> InvocationId {
        self.id
    }

    pub fn finish(mut self) {
        self.tracker.finish(self.id);
        self.finished = true;
    }
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.tracker.finish(self.id);
        }
    }
}

fn repeated_symbol_depth(
    state: &TrackerState,
    mut current: Option<InvocationId>,
    symbol: &SymbolId,
) -> Result<u64, InvocationError> {
    let mut count = 0u64;
    while let Some(id) = current {
        let invocation = state
            .invocations
            .get(&id)
            .ok_or(InvocationError::Unknown(id))?;
        if &invocation.symbol == symbol {
            count = count.saturating_add(1);
        }
        current = invocation.parent_id;
    }
    Ok(count)
}

fn is_ancestor(
    state: &TrackerState,
    ancestor: InvocationId,
    mut child: InvocationId,
) -> Result<bool, InvocationError> {
    loop {
        let invocation = state
            .invocations
            .get(&child)
            .ok_or(InvocationError::Unknown(child))?;
        let Some(parent) = invocation.parent_id else {
            return Ok(false);
        };
        if parent == ancestor {
            return Ok(true);
        }
        child = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::{RelSourceKind, SourceId};

    fn symbol(name: &str) -> SymbolId {
        SymbolId::new(
            SourceId::physical(RelSourceKind::Module, "Tree").unwrap(),
            name,
        )
    }

    #[test]
    fn legitimate_recursive_calls_are_allowed_within_budget() {
        let tracker = InvocationTracker::new(RecursionPolicy {
            max_depth: 10,
            repeated_symbol_depth: 10,
            operation_budget: 100,
        });
        let root = tracker.begin(None, symbol("walk"), 5).unwrap();
        let child = tracker
            .begin(Some(root.id()), symbol("walk"), 4)
            .unwrap();
        assert_eq!(tracker.snapshot(child.id()).unwrap().depth, 2);
    }

    #[test]
    fn detects_wait_on_value_produced_by_ancestor() {
        let tracker = InvocationTracker::new(RecursionPolicy::default());
        let root = tracker.begin(None, symbol("load"), 1).unwrap();
        tracker.producing(root.id(), "VALUE-X").unwrap();
        let child = tracker
            .begin(Some(root.id()), symbol("resolve"), 2)
            .unwrap();
        assert!(matches!(
            tracker.wait_for(child.id(), "VALUE-X"),
            Err(InvocationError::CyclicComputation { .. })
        ));
    }
}