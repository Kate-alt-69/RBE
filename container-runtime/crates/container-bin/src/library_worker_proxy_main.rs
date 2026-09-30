use std::path::PathBuf;

use container_bin_security::{
    execute_library_worker_proxy, run_library_worker_proxy_exec_child,
    LibraryWorkerProxyExecutionOptions,
};
use ipc_protocol::{
    read_library_worker_proxy_bootstrap, write_library_worker_proxy_result,
    LibraryWorkerProxyResult, MAX_LIBRARY_WORKER_PROXY_ERROR_BYTES,
};

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--library-worker-exec-child") {
        let cgroup_path = value_after(&args, "--cgroup-path")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("--cgroup-path is required for the internal proxy child"))?;
        return run_library_worker_proxy_exec_child(&cgroup_path).map_err(Into::into);
    }

    let result = match run_parent(&args) {
        Ok(result) => result,
        Err(error) => LibraryWorkerProxyResult::Error {
            code: "proxy_execution_failed".into(),
            message: bounded_error(&error.to_string()),
        },
    };
    let mut stdout = std::io::stdout().lock();
    write_library_worker_proxy_result(&mut stdout, &result)?;
    Ok(())
}

fn run_parent(args: &[String]) -> anyhow::Result<LibraryWorkerProxyResult> {
    let bootstrap = {
        let mut stdin = std::io::stdin().lock();
        read_library_worker_proxy_bootstrap(&mut stdin)?
    };
    let cgroup_root = value_after(args, "--cgroup-root")
        .or_else(|| std::env::var("RBE_CONTAINER_CGROUP_ROOT").ok())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Library Worker Proxy requires --cgroup-root or RBE_CONTAINER_CGROUP_ROOT"
            )
        })?;
    let options = LibraryWorkerProxyExecutionOptions::for_bootstrap(cgroup_root, &bootstrap);
    execute_library_worker_proxy(bootstrap, options).map_err(Into::into)
}

fn value_after(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == key)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn bounded_error(value: &str) -> String {
    let mut output = value.replace('\0', "?");
    if output.is_empty() {
        output = "Library Worker Proxy failed".into();
    }
    if output.len() <= MAX_LIBRARY_WORKER_PROXY_ERROR_BYTES {
        return output;
    }
    let mut end = MAX_LIBRARY_WORKER_PROXY_ERROR_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output
}
