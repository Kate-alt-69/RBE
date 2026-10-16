use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use ipc_protocol::{
    read_library_worker_proxy_bootstrap, read_library_worker_proxy_status,
    write_library_worker_proxy_bootstrap, write_library_worker_proxy_status,
    LibraryWorkerProxyBootstrap, LibraryWorkerProxyStatus, MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES,
};
use resource_limits::{attach_current_process_to_cgroup, CgroupHandle, ResourceLimits};
use sandbox_primitives::{
    install_restricted_seccomp, install_workspace_landlock, set_no_new_privileges, NetworkPolicy,
    SandboxLauncher, SandboxPolicy,
};

use crate::{verify_library_worker_proxy, VerifiedLibraryWorkerProxy};

/// Run a long-lived Library Worker Proxy.
///
/// `library.proxy.ready` only means the sandboxed child boundary is established.
/// Backend must still require the normal `library.hello` identity/ABI handshake
/// before any package invocation or host capability call is accepted.
pub fn run_live_library_worker_proxy(
    bootstrap: LibraryWorkerProxyBootstrap,
    cgroup_root: PathBuf,
) -> Result<()> {
    validate_cgroup_root(&cgroup_root)?;
    let verified = verify_library_worker_proxy(bootstrap.clone())
        .context("verify Library Worker Proxy bootstrap before live launch")?;
    verified
        .verify_before_spawn()
        .context("reverify Library Worker Proxy bytes before live launch")?;

    let startup_timeout_ms = verified.startup_timeout_seconds().saturating_mul(1_000);
    if startup_timeout_ms == 0 {
        bail!("Library Worker Proxy startup timeout must be non-zero");
    }
    let limits = ResourceLimits {
        wall_time_ms: startup_timeout_ms,
        ..Default::default()
    };
    let cgroup = CgroupHandle::create(&cgroup_root, &execution_id(), limits)
        .context("create live Library Worker Proxy cgroup")?;

    let current_exe = std::env::current_exe().context("resolve Library Worker Proxy executable")?;
    let current_exe_text = current_exe
        .to_str()
        .ok_or_else(|| anyhow!("Library Worker Proxy executable path is not UTF-8"))?;
    let cgroup_path_text = cgroup
        .path()
        .to_str()
        .ok_or_else(|| anyhow!("Library Worker Proxy cgroup path is not UTF-8"))?;
    let policy = SandboxPolicy {
        network: NetworkPolicy::DenyAll,
        max_processes: u64::from(limits.max_processes),
        max_memory_bytes: limits.memory_bytes,
        max_cpu_micros: limits.cpu_millis.saturating_mul(1_000),
        timeout_ms: startup_timeout_ms,
        ..Default::default()
    };
    let args = vec![
        "--library-worker-live-child".to_string(),
        "--cgroup-path".to_string(),
        cgroup_path_text.to_string(),
    ];
    let mut command = SandboxLauncher::command(&policy, current_exe_text, &args)
        .map_err(|error| anyhow!("create Library Worker Proxy sandbox command: {error}"))?;
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("spawn sandboxed live Library Worker Proxy child")?;
    let child_pid = child.id();

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("live Library Worker Proxy child stdin is unavailable"))?;
    if let Err(error) = write_library_worker_proxy_bootstrap(&mut child_stdin, &bootstrap) {
        terminate(&cgroup, &mut child);
        return Err(error).context("send verified bootstrap to live proxy child");
    }
    child_stdin.flush()?;

    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("live Library Worker Proxy child stdout is unavailable"))?;
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("live Library Worker Proxy child stderr is unavailable"))?;

    let (status_tx, status_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("rbe-library-proxy-status".into())
        .spawn(move || {
            let mut stdout = child_stdout;
            let status = read_library_worker_proxy_status(&mut stdout);
            let _ = status_tx.send((status, stdout));
        })?;

    let (status, mut child_stdout) =
        match status_rx.recv_timeout(Duration::from_millis(startup_timeout_ms)) {
            Ok(value) => value,
            Err(error) => {
                terminate(&cgroup, &mut child);
                bail!("live Library Worker Proxy startup timed out: {error}");
            }
        };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            terminate(&cgroup, &mut child);
            return Err(error).context("read live Library Worker Proxy startup status");
        }
    };

    match &status {
        LibraryWorkerProxyStatus::Ready { pid, .. } if *pid == child_pid => {}
        LibraryWorkerProxyStatus::Ready { pid, .. } => {
            terminate(&cgroup, &mut child);
            bail!(
                "live Library Worker Proxy startup pid mismatch: expected {child_pid}, got {pid}"
            );
        }
        LibraryWorkerProxyStatus::Reject { code, message, .. } => {
            forward_status(&status)?;
            terminate(&cgroup, &mut child);
            bail!("live Library Worker Proxy rejected startup ({code}): {message}");
        }
    }
    forward_status(&status)?;

    // From this point onward stdout is raw Library Protocol. Package logs must
    // stay on stderr so they can never corrupt framed protocol traffic.
    let input_cgroup = cgroup.clone();
    thread::Builder::new()
        .name("rbe-library-proxy-input".into())
        .spawn(move || {
            let mut backend_stdin = std::io::stdin().lock();
            let _ = io::copy(&mut backend_stdin, &mut child_stdin);
            drop(child_stdin);
            let _ = input_cgroup.kill_all();
        })?;
    let stderr_relay = thread::Builder::new()
        .name("rbe-library-proxy-stderr".into())
        .spawn(move || {
            let mut child_stderr = child_stderr;
            let mut backend_stderr = std::io::stderr().lock();
            io::copy(&mut child_stderr, &mut backend_stderr)
        })?;

    let relay_result = {
        let mut backend_stdout = std::io::stdout().lock();
        io::copy(&mut child_stdout, &mut backend_stdout).and_then(|_| backend_stdout.flush())
    };
    if let Err(error) = relay_result {
        terminate(&cgroup, &mut child);
        return Err(error).context("relay Library Protocol worker output");
    }

    let status = child.wait().context("wait for live package worker")?;
    stderr_relay
        .join()
        .map_err(|_| anyhow!("Library Worker Proxy stderr relay thread panicked"))??;
    if !status.success() {
        bail!(
            "live package worker exited with code {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Internal sandbox child for the long-lived proxy.
///
/// It emits one trusted startup status and then `exec`s the managed interpreter,
/// preserving stdin/stdout as the raw Library Protocol channel.
pub fn run_library_worker_proxy_live_child(cgroup_path: &Path) -> Result<()> {
    let verified = match prepare_live_child(cgroup_path) {
        Ok(verified) => verified,
        Err(error) => {
            let status = LibraryWorkerProxyStatus::reject(
                "SANDBOX_START_FAILED",
                bounded_status_message(&error.to_string()),
            );
            let _ = forward_status(&status);
            return Err(error);
        }
    };

    forward_status(&LibraryWorkerProxyStatus::ready(std::process::id()))?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = Command::new(verified.program())
            .args(verified.args())
            .current_dir(verified.working_directory())
            .env_clear()
            .exec();
        Err(error).context("exec managed Library Worker Proxy interpreter")
    }
    #[cfg(not(unix))]
    {
        let _ = verified;
        bail!("live Library Worker Proxy execution is unsupported on this platform")
    }
}

fn prepare_live_child(cgroup_path: &Path) -> Result<VerifiedLibraryWorkerProxy> {
    if !cgroup_path.is_absolute() {
        bail!("Library Worker Proxy cgroup path must be absolute");
    }
    let bootstrap = {
        let mut stdin = std::io::stdin().lock();
        read_library_worker_proxy_bootstrap(&mut stdin)
            .context("read live Library Worker Proxy bootstrap")?
    };
    let verified = verify_library_worker_proxy(bootstrap)
        .context("verify live Library Worker Proxy bootstrap")?;
    attach_current_process_to_cgroup(cgroup_path)
        .context("attach live package worker to cgroup")?;
    verified.verify_before_spawn()?;
    set_no_new_privileges().context("set no_new_privs for live package worker")?;
    install_workspace_landlock(verified.working_directory(), verified.program())
        .context("install Landlock for live package worker")?;
    install_restricted_seccomp().context("install seccomp for live package worker")?;
    verified.verify_before_spawn()?;
    Ok(verified)
}

fn validate_cgroup_root(root: &Path) -> Result<()> {
    if !root.is_absolute() || !root.is_dir() {
        bail!(
            "Library Worker Proxy requires a delegated absolute cgroup-v2 root: {}",
            root.display()
        );
    }
    Ok(())
}

fn forward_status(status: &LibraryWorkerProxyStatus) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_library_worker_proxy_status(&mut stdout, status)?;
    stdout.flush()?;
    Ok(())
}

fn terminate(cgroup: &CgroupHandle, child: &mut std::process::Child) {
    let _ = cgroup.kill_all();
    let _ = child.kill();
    let _ = child.wait();
}

fn bounded_status_message(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect::<String>();
    if output.is_empty() {
        output = "Container could not start the isolated package worker".into();
    }
    if output.len() <= MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES {
        return output;
    }
    let mut end = MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output
}

fn execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("rbe-library-live-worker-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_messages_are_bounded_printable_text() {
        let message = format!(
            "bad\n{}",
            "x".repeat(MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES + 32)
        );
        let bounded = bounded_status_message(&message);
        assert!(bounded.len() <= MAX_LIBRARY_WORKER_PROXY_MESSAGE_BYTES);
        assert!(!bounded.chars().any(char::is_control));
    }
}
