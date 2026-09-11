use std::time::{SystemTime, UNIX_EPOCH};

use resource_limits::ResourceLimits;
use sandbox_primitives::SandboxPolicy;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExecutionId {
    epoch_ns: u64,
    sequence: u64,
}

impl ExecutionId {
    pub(crate) fn new(sequence: u64) -> Self {
        let epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        Self { epoch_ns, sequence }
    }

    pub fn from_parts(epoch_ns: u64, sequence: u64) -> Self {
        Self { epoch_ns, sequence }
    }
    pub fn epoch_ns(self) -> u64 {
        self.epoch_ns
    }
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec-{:016x}-{:016x}", self.epoch_ns, self.sequence)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCost {
    pub cpu: u64,
    pub memory: u64,
    pub io: u64,
    pub network: u64,
}

impl WorkCost {
    pub fn scalar(self) -> u64 {
        self.cpu
            .saturating_add(self.memory)
            .saturating_add(self.io)
            .saturating_add(self.network)
            .max(1)
    }
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            cpu: self.cpu.saturating_add(other.cpu),
            memory: self.memory.saturating_add(other.memory),
            io: self.io.saturating_add(other.io),
            network: self.network.saturating_add(other.network),
        }
    }
}

/// Controller-stamped identity that authorized one execution. The caller
/// supplies Runtime Image / SourceId / ABI, but only Container Controller may
/// bind those values to an Environment generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProvenance {
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub environment: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {
    Queued,
    Assigned,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    SecurityTerminated,
}

#[derive(Debug, Clone)]
pub struct ExecutionTask {
    pub id: ExecutionId,
    pub environment: String,
    /// None exists only for trusted internal/simulated work and legacy journal
    /// decoding. Production Environment dispatch rejects unattributed tasks.
    pub provenance: Option<ExecutionProvenance>,
    pub artifact_hash: String,
    pub declared_cost: WorkCost,
    pub limits: ResourceLimits,
    pub sandbox: SandboxPolicy,
    pub work_ms: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ExecutionOutcome {
    pub output: Vec<u8>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct ExecutionRecord {
    pub task: ExecutionTask,
    pub state: ExecutionState,
    pub swamp_id: Option<usize>,
    pub worker_id: Option<usize>,
    pub started_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
}
