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
fn worker_cannot_join_host_namespaces() {
    let policy = SandboxPolicy::default();
    let args = vec!["-c".into(), "test \"$(readlink /proc/1/ns/pid)\" != \"$(readlink /proc/self/ns/pid)\"".into()];
    let mut command = SandboxLauncher::command(&policy, "/bin/sh", &args).expect("sandbox command");
    let status = command.stdout(Stdio::null()).stderr(Stdio::null()).status().expect("run sandbox probe");
    assert!(status.success(), "PID namespace probe failed: {status}");
}

#[test]
#[ignore = "requires a Linux host that permits unshare namespaces"]
fn worker_network_namespace_isolated_by_default() {
    let policy = SandboxPolicy::default();
    let args = vec!["-c".into(), "test \"$(cat /proc/self/net/route | tail -n +2)\" = \"\"".into()];
    let mut command = SandboxLauncher::command(&policy, "/bin/sh", &args).expect("sandbox command");
    let status = command.stdout(Stdio::null()).stderr(Stdio::null()).status().expect("run network probe");
    assert!(status.success(), "network namespace probe failed: {status}");
}

#[test]
#[ignore = "seccomp installation intentionally terminates processes that call blocked syscalls"]
fn no_new_privileges_and_seccomp_are_installable() {
    set_no_new_privileges().expect("PR_SET_NO_NEW_PRIVS");
    install_restricted_seccomp().expect("seccomp filter installation");
}
