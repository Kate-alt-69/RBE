//! Hostile-workload regression tests for the Linux sandbox boundary.
//!
//! These are intentionally ignored by default because namespace/cgroup tests
//! need a Linux host with the required kernel features and (for cgroup tests)
//! a delegated cgroup v2 subtree. Run explicitly on a hardened test runner:
//!
//!     cargo test -p sandbox-primitives --test security -- --ignored --nocapture

#![cfg(target_os = "linux")]

use std::process::Stdio;

use sandbox_primitives::{install_restricted_seccomp, set_no_new_privileges, SandboxLauncher, SandboxPolicy};

#[test]
#[ignore = "requires a Linux host that permits unshare namespaces"]
fn worker_gets_a_private_pid_namespace() {
    let policy = SandboxPolicy::default();
    let args = vec!["-c".into(), "test \"$$\" = \"1\"".into()];
    let mut command = SandboxLauncher::command(&policy, "/bin/sh", &args).expect("sandbox command");
    let status = command.stdout(Stdio::null()).stderr(Stdio::null()).status().expect("run sandbox probe");
    assert!(status.success(), "PID namespace probe failed: {status}");
}

#[test]
#[ignore = "requires a Linux host that permits unshare network namespaces"]
fn worker_network_namespace_is_denied_by_default() {
    let policy = SandboxPolicy::default();
    let args = vec!["-c".into(), "test \"$(awk 'NR>1 && $2 != \"00000000\" {print; exit}' /proc/net/route)\" = \"\"".into()];
    let mut command = SandboxLauncher::command(&policy, "/bin/sh", &args).expect("sandbox command");
    let status = command.stdout(Stdio::null()).stderr(Stdio::null()).status().expect("run network probe");
    assert!(status.success(), "network namespace probe failed: {status}");
}

#[test]
#[ignore = "seccomp installation intentionally changes the current process syscall policy"]
fn no_new_privileges_and_seccomp_are_installable() {
    set_no_new_privileges().expect("PR_SET_NO_NEW_PRIVS");
    install_restricted_seccomp().expect("seccomp filter installation");
}
