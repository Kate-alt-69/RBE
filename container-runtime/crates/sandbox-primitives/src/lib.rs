//! Sandbox policy primitives.
//!
//! This crate is the contract for the *actual* OS isolation layer. The policy
//! is intentionally deny-by-default and can be consumed by a Linux namespace /
//! cgroup / seccomp backend or a Windows Job Object/AppContainer backend later.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
    pub privileges: PrivilegePolicy,
    pub namespaces: NamespacePolicy,
    pub syscalls: SyscallPolicy,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            network: NetworkPolicy::DenyAll,
            filesystem: FilesystemPolicy::WorkspaceOnly,
            privileges: PrivilegePolicy::NoExtraCapabilities,
            namespaces: NamespacePolicy::FullyIsolated,
            syscalls: SyscallPolicy::Restricted,
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
    /// Reject clearly unsafe policy shapes before an OS backend sees them.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let NetworkPolicy::AllowList(rules) = &self.network {
            for rule in rules {
                if rule.host.trim().is_empty() || rule.ports.is_empty() {
                    return Err("network allow-list entries require a host and at least one port");
                }
            }
        }
        Ok(())
    }
}
