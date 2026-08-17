//! Sandbox policy primitives and Linux launcher hooks.
//!
//! The policy is deny-by-default. Linux workers get namespace isolation,
//! no-new-privileges and a seccomp deny-list for syscalls that can alter host
//! control or create a second sandbox. cgroup-v2 resource enforcement lives in
//! `resource-limits`.

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
pub enum NetworkPolicy { DenyAll, AllowList(Vec<HostRule>) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule { pub host: String, pub ports: Vec<u16> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemPolicy { WorkspaceOnly, ReadOnlyRootWithWorkspace }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegePolicy { NoExtraCapabilities }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespacePolicy { FullyIsolated }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyscallPolicy { Restricted }

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
    pub fn command(policy: &SandboxPolicy, program: &str, args: &[String]) -> Result<Command, String> {
        policy.validate().map_err(str::to_string)?;
        #[cfg(target_os = "linux")]
        {
            let mut command = Command::new("unshare");
            command.args(["--fork", "--pid", "--mount", "--ipc", "--uts", "--mount-proc"]);
            if matches!(policy.network, NetworkPolicy::DenyAll) { command.arg("--net"); }
            command.arg("--").arg(program).args(args);
            return Ok(command);
        }
        #[cfg(not(target_os = "linux")]
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

/// Installs a conservative syscall deny-list. It deliberately blocks host /
/// privilege-control primitives while leaving ordinary file, memory, thread,
/// socket and WASM-runtime syscalls available. The list is architecture-
/// specific today; unsupported Linux architectures return an explicit error.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn install_restricted_seccomp() -> std::io::Result<()> {
    use std::mem;

    const ARCH_X86_64: u32 = 0xC000_003E;
    let blocked: &[i64] = &[
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_ptrace,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_open_by_handle_at,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
    ];

    let mut filter = Vec::with_capacity(blocked.len() * 2 + 4);
    filter.push(libc::sock_filter { code: libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, jt: 0, jf: 0, k: 4 });
    filter.push(libc::sock_filter { code: libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, jt: 1, jf: 0, k: ARCH_X86_64 });
    filter.push(libc::sock_filter { code: libc::BPF_RET | libc::BPF_K, jt: 0, jf: 0, k: libc::SECCOMP_RET_KILL_PROCESS });
    filter.push(libc::sock_filter { code: libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, jt: 0, jf: 0, k: 0 });

    for syscall in blocked {
        filter.push(libc::sock_filter {
            code: libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            jt: 0,
            jf: 1,
            k: *syscall as u32,
        });
        filter.push(libc::sock_filter { code: libc::BPF_RET | libc::BPF_K, jt: 0, jf: 0, k: libc::SECCOMP_RET_KILL_PROCESS });
    }

    filter.push(libc::sock_filter { code: libc::BPF_RET | libc::BPF_K, jt: 0, jf: 0, k: libc::SECCOMP_RET_ALLOW });
    let mut program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &mut program as *mut libc::sock_fprog) };
    let _ = mem::size_of::<libc::sock_fprog>();
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
pub fn install_restricted_seccomp() -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "restricted seccomp backend currently supports x86_64 Linux only"))
}

#[cfg(not(target_os = "linux"))]
pub fn install_restricted_seccomp() -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "seccomp is Linux-only"))
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
