//! Resource policy and Linux cgroup-v2 enforcement.

#[cfg(target_os = "linux")]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone)]
pub struct CgroupHandle {
    path: PathBuf,
}

impl CgroupHandle {
    pub fn create(root: &Path, execution_id: &str, limits: ResourceLimits) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let path = root.join(execution_id);
            fs::create_dir_all(&path)?;
            write_file(&path, "memory.max", limits.memory_bytes.to_string())?;
            write_file(&path, "pids.max", limits.max_processes.to_string())?;
            let quota = limits.cpu_millis.saturating_mul(100);
            write_file(&path, "cpu.max", format!("{quota} 100000"))?;
            return Ok(Self { path });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, execution_id, limits);
            Err(io::Error::new(io::ErrorKind::Unsupported, "cgroup-v2 enforcement is Linux-only"))
        }
    }

    pub fn attach_pid(&self, pid: u32) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            write_file(&self.path, "cgroup.procs", pid.to_string())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            Err(io::Error::new(io::ErrorKind::Unsupported, "cgroup-v2 enforcement is Linux-only"))
        }
    }

    pub fn path(&self) -> &Path { &self.path }
}

impl Drop for CgroupHandle {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn write_file(dir: &Path, name: &str, value: impl AsRef<[u8]>) -> io::Result<()> {
    fs::write(dir.join(name), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_check_detects_wall_time() {
        let limits = ResourceLimits::default();
        let usage = ResourceUsage { wall_time_ms: limits.wall_time_ms + 1, ..Default::default() };
        assert_eq!(limits.check(usage), Some(LimitViolation::WallTime));
    }
}
