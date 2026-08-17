//! Kernel-facing resource policy model.
//!
//! This crate deliberately separates *policy* from enforcement. The container
//! supervisor can validate and carry these limits today; `sandbox-primitives`
//! and the future execution backend are responsible for translating them into
//! OS-enforced controls (cgroups/job objects, process limits, timeouts, etc.).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub network_bytes: u64,
    pub max_processes: u32,
    pub max_file_descriptors: u32,
    pub wall_time_ms: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_millis: 1_000,
            memory_bytes: 256 * 1024 * 1024,
            disk_bytes: 512 * 1024 * 1024,
            network_bytes: 64 * 1024 * 1024,
            max_processes: 64,
            max_file_descriptors: 512,
            wall_time_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    pub cpu_millis: u64,
    pub memory_peak_bytes: u64,
    pub disk_bytes: u64,
    pub network_bytes: u64,
    pub processes: u32,
    pub file_descriptors: u32,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitViolation {
    Cpu,
    Memory,
    Disk,
    Network,
    Processes,
    FileDescriptors,
    WallTime,
}

impl ResourceLimits {
    pub fn check(&self, usage: ResourceUsage) -> Option<LimitViolation> {
        if usage.cpu_millis > self.cpu_millis { return Some(LimitViolation::Cpu); }
        if usage.memory_peak_bytes > self.memory_bytes { return Some(LimitViolation::Memory); }
        if usage.disk_bytes > self.disk_bytes { return Some(LimitViolation::Disk); }
        if usage.network_bytes > self.network_bytes { return Some(LimitViolation::Network); }
        if usage.processes > self.max_processes { return Some(LimitViolation::Processes); }
        if usage.file_descriptors > self.max_file_descriptors { return Some(LimitViolation::FileDescriptors); }
        if usage.wall_time_ms > self.wall_time_ms { return Some(LimitViolation::WallTime); }
        None
    }
}
