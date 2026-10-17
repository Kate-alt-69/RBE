//! Service-owned JSON memory with atomic admission and explicit payload budgets.
//! Charged bytes include keys, inline values and nested payloads, not allocator RSS.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub struct ServiceMemoryLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_entry_bytes: usize,
}

impl Default for ServiceMemoryLimits {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_bytes: 8 * 1024 * 1024,
            max_entry_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ServiceMemoryError(&'static str);

struct Entry {
    value: Value,
    bytes: usize,
}

#[derive(Default)]
struct State {
    values: HashMap<String, Entry>,
    bytes: usize,
}

/// Clones share both stored values and the accounting lock; no mutation can
/// admit against an obsolete total or silently evict application-owned state.
#[derive(Clone)]
pub struct ServiceMemory {
    state: Arc<RwLock<State>>,
    limits: ServiceMemoryLimits,
}

impl Default for ServiceMemory {
    fn default() -> Self {
        Self::with_limits(ServiceMemoryLimits::default())
    }
}

impl ServiceMemory {
    pub fn with_limits(limits: ServiceMemoryLimits) -> Self {
        Self {
            state: Arc::new(RwLock::new(State::default())),
            limits,
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.state
            .read()
            .ok()?
            .values
            .get(key)
            .map(|entry| entry.value.clone())
    }

    pub fn set(&self, key: String, value: Value) -> Result<(), ServiceMemoryError> {
        if key.len() > 1024 {
            return Err(ServiceMemoryError("service memory key exceeds 1024 bytes"));
        }
        let mut bytes = key.len();
        charge(&value, 0, &mut bytes, self.limits.max_entry_bytes)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| ServiceMemoryError("service memory lock is poisoned"))?;
        let old_bytes = state.values.get(&key).map_or(0, |entry| entry.bytes);
        if old_bytes == 0 && state.values.len() >= self.limits.max_entries {
            return Err(ServiceMemoryError("service memory entry budget exhausted"));
        }
        let total = state
            .bytes
            .checked_sub(old_bytes)
            .and_then(|remaining| remaining.checked_add(bytes))
            .filter(|total| *total <= self.limits.max_bytes)
            .ok_or(ServiceMemoryError("service memory byte budget exhausted"))?;
        state.values.insert(key, Entry { value, bytes });
        state.bytes = total;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> bool {
        let Ok(mut state) = self.state.write() else {
            return false;
        };
        if let Some(entry) = state.values.remove(key) {
            state.bytes -= entry.bytes;
            true
        } else {
            false
        }
    }

    pub fn clear(&self) {
        if let Ok(mut state) = self.state.write() {
            state.values.clear();
            state.bytes = 0;
        }
    }

    pub fn len(&self) -> usize {
        self.state
            .read()
            .map(|state| state.values.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn charged_bytes(&self) -> usize {
        self.state.read().map(|state| state.bytes).unwrap_or(0)
    }
}

/// Walk only bounded input. Constant per-node charging also limits collections
/// of empty values; nesting has its own cap to protect serialization and clones.
fn charge(
    value: &Value,
    depth: usize,
    bytes: &mut usize,
    limit: usize,
) -> Result<(), ServiceMemoryError> {
    if depth > 32 {
        return Err(ServiceMemoryError(
            "service memory nesting exceeds 32 levels",
        ));
    }
    let payload = match value {
        Value::String(text) => text.len(),
        _ => 0,
    };
    *bytes = bytes
        .checked_add(std::mem::size_of::<Value>())
        .and_then(|bytes| bytes.checked_add(payload))
        .filter(|bytes| *bytes <= limit)
        .ok_or(ServiceMemoryError(
            "service memory entry byte budget exhausted",
        ))?;
    match value {
        Value::Array(values) => {
            for value in values {
                charge(value, depth + 1, bytes, limit)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                *bytes = bytes
                    .checked_add(key.len() + std::mem::size_of::<String>())
                    .filter(|bytes| *bytes <= limit)
                    .ok_or(ServiceMemoryError(
                        "service memory entry byte budget exhausted",
                    ))?;
                charge(value, depth + 1, bytes, limit)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_replacement_preserves_values_and_releases_budget_on_delete() {
        let memory = ServiceMemory::with_limits(ServiceMemoryLimits {
            max_entries: 1,
            max_bytes: 150,
            max_entry_bytes: 200,
        });
        memory
            .set("a".into(), Value::String("small".into()))
            .unwrap();
        let bytes = memory.charged_bytes();
        assert!(memory
            .set("a".into(), Value::String("x".repeat(150)))
            .is_err());
        assert_eq!(memory.get("a"), Some(Value::String("small".into())));
        assert_eq!(memory.charged_bytes(), bytes);
        assert!(memory.set("b".into(), Value::Null).is_err());
        assert!(memory.clone().delete("a"));
        assert_eq!(memory.charged_bytes(), 0);
        memory.set("b".into(), Value::Null).unwrap();
        memory.clear();
        assert_eq!(memory.charged_bytes(), 0);
    }

    #[test]
    fn entry_and_depth_limits_precede_mutation() {
        let memory = ServiceMemory::with_limits(ServiceMemoryLimits {
            max_entries: 10,
            max_bytes: 5000,
            max_entry_bytes: 1000,
        });
        assert!(memory.set("x".repeat(1025), Value::Null).is_err());
        assert!(memory
            .set("x".into(), Value::String("x".repeat(1000)))
            .is_err());
        let mut value = Value::Null;
        for _ in 0..34 {
            value = Value::Array(vec![value]);
        }
        assert!(memory.set("deep".into(), value).is_err());
        assert!(memory.is_empty());
        assert_eq!(memory.charged_bytes(), 0);
    }

    #[test]
    fn concurrent_writers_share_one_admission_budget() {
        let memory = ServiceMemory::with_limits(ServiceMemoryLimits {
            max_entries: 4,
            max_bytes: 400,
            max_entry_bytes: 100,
        });
        let tasks: Vec<_> = (0..20)
            .map(|index| {
                let memory = memory.clone();
                std::thread::spawn(move || memory.set(index.to_string(), Value::Null).is_ok())
            })
            .collect();
        let accepted = tasks
            .into_iter()
            .map(|task| usize::from(task.join().unwrap()))
            .sum::<usize>();
        assert_eq!(accepted, 4);
        assert_eq!(memory.len(), 4);
        assert!(memory.charged_bytes() <= 400);
    }
}
