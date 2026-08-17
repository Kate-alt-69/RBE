//! Sandbox policy primitives and Linux launcher hooks.
//!
//! The policy is deny-by-default. The launcher prepares a disposable worker
//! process using Linux namespaces plus `PR_SET_NO_NEW_PRIVS`; cgroups and
//! seccomp are represented by the policy contract and are enforced by the
//! resource/supervisor layer before a workload is considered trusted.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
    pub privileges: PrivilegePolicy,
    pub namespaces: NamespacePolicy,
    pub syscalls: SyscallPolicy,
    pub max_processes: u64,
    pub max_memory_bytes: u64,
    pub max_cpu_micros: u64,
    pub timeout_ms: u64,
    pub writable_paths: Vec<PathBuf>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            network: NetworkPolicy::DenyAll,
            filesystem: FilesystemPolicy::WorkspaceOnly,
            privileges: PrivilegePolicy::NoExtraCapabilities,
            namespaces: NamespacePolicy::FullyIsolated,
            syscalls: SyscallPolicy::Restricted,
            max_processes: 64,
            max_memory_bytes: 256 * 1024 * 1024,
            max_cpu_micros: 60_000_000,
            timeout_ms: 30_000,
            writable_paths: vec![PathBuf::from("/work"), PathBuf::from("/tmp")],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicy {
    DenyAll,
    AllowList(Vec<HostRule>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    pub host: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemPolicy {
    WorkspaceOnly,
    ReadOnlyRootWithWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegePolicy {
    NoExtraCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespacePolicy {
    FullyIsolated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyscallPolicy {
    Restricted,
}

impl SandboxPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let NetworkPolicy::AllowList(rules) = &self.network {
            for rule in rules {
                if rule.host.trim().is_empty() || rule.ports.is_empty() {
                    return Err("network allow-list entries require a host and at least one port");
                }
            }
        }
        if self.max_processes == 0 || self.max_memory_bytes == 0 || self.timeout_ms == 0 {
            return Err("sandbox limits must be non-zero");
        }
        Ok(())
    }
}

pub struct SandboxLauncher;

impl SandboxLauncher {
    /// Construct a Linux namespace wrapper for a disposable worker.
    /// Network is deliberately absent unless explicitly allowed later.
    pub fn command(policy: &SandboxPolicy, program: &str, args: &[String]) -> Result<Command, String> {
        policy.validate().map_err(str::to_string)?;

        #[cfg(target_os = "linux")]
        {
            let mut command = Command::new("unshare");
            command.args(["--fork", "--pid", "--mount", "--ipc", "--uts", "--mount-proc"]);
            if matches!(policy.network, NetworkPolicy::DenyAll) {
                command.arg("--net");
            }
            command.arg("--").arg(program).args(args);
            return Ok(command);
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (policy, program, args);
            Err("OS sandbox backend is not implemented on this platform".to_string())
        }
    }
}

#[cfg(target_os = "linux")]
pub fn set_no_new_privileges() -> std::io::Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_deny_first() {
        let policy = SandboxPolicy::default();
        assert_eq!(policy.network, NetworkPolicy::DenyAll);
        assert_eq!(policy.privileges, PrivilegePolicy::NoExtraCapabilities);
        assert_eq!(policy.namespaces, NamespacePolicy::FullyIsolated);
        assert_eq!(policy.syscalls, SyscallPolicy::Restricted);
        assert!(policy.validate().is_ok());
    }
}
