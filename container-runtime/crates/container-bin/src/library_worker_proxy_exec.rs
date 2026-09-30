use std::error::Error;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ipc_protocol::{
    read_library_worker_proxy_bootstrap, write_library_worker_proxy_bootstrap,
    LibraryWorkerProxyBootstrap, LibraryWorkerProxyResult, MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES,
    MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES,
};
use resource_limits::{attach_current_process_to_cgroup, CgroupHandle, ResourceLimits};
use sandbox_primitives::{
    install_restricted_seccomp, install_workspace_landlock, set_no_new_privileges, NetworkPolicy,
    SandboxLauncher, SandboxPolicy,
};

use crate::verify_library_worker_proxy;

#[derive(Debug, Clone)]
pub struct LibraryWorkerProxyExecutionOptions {
    pub cgroup_root: PathBuf,
    pub limits: ResourceLimits,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl LibraryWorkerProxyExecutionOptions {
    pub fn for_bootstrap(cgroup_root: PathBuf, bootstrap: &LibraryWorkerProxyBootstrap) -> Self {
        let mut limits = ResourceLimits::default();
        limits.wall_time_ms = bootstrap.startup_timeout_seconds.saturating_mul(1_000);
        Self {
            cgroup_root,
            limits,
            max_stdout_bytes: MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES,
            max_stderr_bytes: MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES,
        }
    }

    fn validate(&self) -> Result<(), LibraryWorkerProxyExecutionError> {
        if !self.cgroup_root.is_absolute() || !self.cgroup_root.is_dir() {
            return Err(LibraryWorkerProxyExecutionError::InvalidCgroupRoot(
                self.cgroup_root.clone(),
            ));
        }
        if self.limits.wall_time_ms == 0
            || self.limits.memory_bytes == 0
            || self.limits.max_processes == 0
            || self.max_stdout_bytes == 0
            || self.max_stdout_bytes > MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES
            || self.max_stderr_bytes == 0
            || self.max_stderr_bytes > MAX_LIBRARY_WORKER_PROXY_STDERR_BYTES
        {
            return Err(LibraryWorkerProxyExecutionError::InvalidLimits);
        }
        Ok(())
    }
}

pub fn execute_library_worker_proxy(
    bootstrap: LibraryWorkerProxyBootstrap,
    options: LibraryWorkerProxyExecutionOptions,
) -> Result<LibraryWorkerProxyResult, LibraryWorkerProxyExecutionError> {
    options.validate()?;
    let verified = verify_library_worker_proxy(bootstrap.clone())?;
    verified.verify_before_spawn()?;

    let timeout_ms = options
        .limits
        .wall_time_ms
        .min(verified.startup_timeout_seconds().saturating_mul(1_000));
    if timeout_ms == 0 {
        return Err(LibraryWorkerProxyExecutionError::InvalidLimits);
    }

    let cgroup = CgroupHandle::create(&options.cgroup_root, &execution_id(), options.limits)?;
    let current_exe = std::env::current_exe()?;
    let current_exe_text = current_exe
        .to_str()
        .ok_or_else(|| LibraryWorkerProxyExecutionError::NonUtf8Path(current_exe.clone()))?;
    let cgroup_path_text = cgroup.path().to_str().ok_or_else(|| {
        LibraryWorkerProxyExecutionError::NonUtf8Path(cgroup.path().to_path_buf())
    })?;

    let mut policy = SandboxPolicy::default();
    policy.network = NetworkPolicy::DenyAll;
    policy.max_processes = u64::from(options.limits.max_processes);
    policy.max_memory_bytes = options.limits.memory_bytes;
    policy.max_cpu_micros = options.limits.cpu_millis.saturating_mul(1_000);
    policy.timeout_ms = timeout_ms;

    let args = vec![
        "--library-worker-exec-child".to_string(),
        "--cgroup-path".to_string(),
        cgroup_path_text.to_string(),
    ];
    let mut command = SandboxLauncher::command(&policy, current_exe_text, &args)
        .map_err(LibraryWorkerProxyExecutionError::Sandbox)?;
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or(LibraryWorkerProxyExecutionError::MissingChildPipe("stdin"))?;
        if let Err(error) = write_library_worker_proxy_bootstrap(&mut stdin, &bootstrap) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or(LibraryWorkerProxyExecutionError::MissingChildPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(LibraryWorkerProxyExecutionError::MissingChildPipe("stderr"))?;
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_reader =
        spawn_bounded_reader(stdout, options.max_stdout_bytes, Arc::clone(&overflow));
    let stderr_reader =
        spawn_bounded_reader(stderr, options.max_stderr_bytes, Arc::clone(&overflow));

    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let mut output_limit_exceeded = false;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            output_limit_exceeded = true;
            terminate_cgroup(&cgroup, &mut child)?;
            break child.wait()?;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            terminate_cgroup(&cgroup, &mut child)?;
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| LibraryWorkerProxyExecutionError::CaptureThreadPanicked)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| LibraryWorkerProxyExecutionError::CaptureThreadPanicked)?;
    let result = LibraryWorkerProxyResult::Completed {
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        timed_out,
        output_limit_exceeded,
        cgroup_enforced: true,
        wall_time_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    result
        .validate()
        .map_err(|error| LibraryWorkerProxyExecutionError::InvalidResult(error.to_string()))?;
    Ok(result)
}

pub fn run_library_worker_proxy_exec_child(
    cgroup_path: &Path,
) -> Result<(), LibraryWorkerProxyExecutionError> {
    if !cgroup_path.is_absolute() {
        return Err(LibraryWorkerProxyExecutionError::InvalidCgroupRoot(
            cgroup_path.to_path_buf(),
        ));
    }
    let bootstrap = {
        let mut stdin = std::io::stdin().lock();
        read_library_worker_proxy_bootstrap(&mut stdin)?
    };
    let verified = verify_library_worker_proxy(bootstrap)?;
    attach_current_process_to_cgroup(cgroup_path)?;
    verified.verify_before_spawn()?;

    set_no_new_privileges()?;
    install_workspace_landlock(verified.working_directory(), verified.program())?;
    install_restricted_seccomp()?;
    verified.verify_before_spawn()?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = Command::new(verified.program())
            .args(verified.args())
            .current_dir(verified.working_directory())
            .env_clear()
            .exec();
        Err(error.into())
    }
    #[cfg(not(unix))]
    {
        let _ = verified;
        Err(LibraryWorkerProxyExecutionError::UnsupportedPlatform)
    }
}

fn terminate_cgroup(
    cgroup: &CgroupHandle,
    child: &mut std::process::Child,
) -> Result<(), LibraryWorkerProxyExecutionError> {
    let cgroup_result = cgroup.kill_all();
    let child_result = child.kill();
    if let Err(error) = cgroup_result {
        return Err(LibraryWorkerProxyExecutionError::CgroupKill(error));
    }
    if let Err(error) = child_result {
        if child.try_wait()?.is_none() {
            return Err(LibraryWorkerProxyExecutionError::Io(error));
        }
    }
    Ok(())
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    limit: usize,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut stored = Vec::with_capacity(limit.min(64 * 1024));
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let Ok(read) = reader.read(&mut buffer) else {
                break;
            };
            if read == 0 {
                break;
            }
            let remaining = limit.saturating_sub(stored.len());
            let copy = remaining.min(read);
            stored.extend_from_slice(&buffer[..copy]);
            if copy < read {
                overflow.store(true, Ordering::Release);
            }
        }
        stored
    })
}

fn execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("rbe-library-worker-{}-{nanos}", std::process::id())
}

#[derive(Debug)]
pub enum LibraryWorkerProxyExecutionError {
    InvalidCgroupRoot(PathBuf),
    InvalidLimits,
    NonUtf8Path(PathBuf),
    MissingChildPipe(&'static str),
    CaptureThreadPanicked,
    Sandbox(String),
    InvalidResult(String),
    CgroupKill(std::io::Error),
    Proxy(super::library_worker_proxy::ContainerWorkerProxyError),
    Io(std::io::Error),
    UnsupportedPlatform,
}

impl fmt::Display for LibraryWorkerProxyExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCgroupRoot(path) => write!(
                formatter,
                "Library Worker Proxy requires a delegated absolute cgroup-v2 root: {}",
                path.display()
            ),
            Self::InvalidLimits => {
                formatter.write_str("Library Worker Proxy execution limits are invalid")
            }
            Self::NonUtf8Path(path) => write!(
                formatter,
                "Library Worker Proxy path is not UTF-8: {}",
                path.display()
            ),
            Self::MissingChildPipe(name) => write!(
                formatter,
                "Library Worker Proxy child {name} pipe is unavailable"
            ),
            Self::CaptureThreadPanicked => {
                formatter.write_str("Library Worker Proxy output capture thread panicked")
            }
            Self::Sandbox(message) => write!(
                formatter,
                "Library Worker Proxy sandbox launch failed: {message}"
            ),
            Self::InvalidResult(message) => write!(
                formatter,
                "Library Worker Proxy produced an invalid result: {message}"
            ),
            Self::CgroupKill(error) => write!(
                formatter,
                "Library Worker Proxy failed to kill its cgroup: {error}"
            ),
            Self::Proxy(error) => write!(
                formatter,
                "Library Worker Proxy verification failed: {error}"
            ),
            Self::Io(error) => write!(formatter, "Library Worker Proxy I/O failed: {error}"),
            Self::UnsupportedPlatform => formatter
                .write_str("Library Worker Proxy execution is unsupported on this platform"),
        }
    }
}

impl Error for LibraryWorkerProxyExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CgroupKill(error) | Self::Io(error) => Some(error),
            Self::Proxy(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for LibraryWorkerProxyExecutionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<super::library_worker_proxy::ContainerWorkerProxyError>
    for LibraryWorkerProxyExecutionError
{
    fn from(error: super::library_worker_proxy::ContainerWorkerProxyError) -> Self {
        Self::Proxy(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_timeout_becomes_the_default_wall_limit() {
        let bootstrap = LibraryWorkerProxyBootstrap {
            protocol: ipc_protocol::LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            program: "/managed/bun".into(),
            program_sha256: "a".repeat(64),
            args: vec!["/worker/worker.js".into()],
            working_directory: "/worker".into(),
            source_files: vec![ipc_protocol::LibraryWorkerProxySourceFile {
                path: "worker.js".into(),
                size: 1,
                sha256: "b".repeat(64),
            }],
            clear_environment: true,
            environment: Default::default(),
            direct_network_allowed: false,
            use_shell: false,
            startup_timeout_seconds: 42,
        };
        let options = LibraryWorkerProxyExecutionOptions::for_bootstrap(
            PathBuf::from("/sys/fs/cgroup/rbe"),
            &bootstrap,
        );
        assert_eq!(options.limits.wall_time_ms, 42_000);
        assert_eq!(
            options.max_stdout_bytes,
            MAX_LIBRARY_WORKER_PROXY_STDOUT_BYTES
        );
    }
}
