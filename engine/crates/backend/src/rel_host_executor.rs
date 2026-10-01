use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use ipc_protocol::{
    read_library_worker_proxy_result, write_library_worker_proxy_bootstrap,
    LibraryWorkerProxyBootstrap, LibraryWorkerProxyResult, LibraryWorkerProxySourceFile,
    LIBRARY_WORKER_PROXY_PROTOCOL_VERSION, MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES,
};
use rbe_install_request::SystemRuntimeKind;
use rbe_install_runtime::load_admitted_system_runtime;
use route_engine::{
    ModuleEvalError, RelHostExecutionFuture, RelHostExecutor, RelHostOutput, RelHostRequest,
    ScriptLanguage, ScriptPlan, WorkspacePath, WorkspaceRoot,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

const MAX_SCRIPT_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SCRIPT_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const PROXY_GRACE_SECONDS: u64 = 5;

mod proxy_integrity {
    include!(concat!(env!("OUT_DIR"), "/library_worker_proxy_integrity.rs"));
}

pub struct BackendRelHostExecutor {
    project_root: PathBuf,
    proxy_path: PathBuf,
    cgroup_root: Option<PathBuf>,
}

impl BackendRelHostExecutor {
    pub fn install(project_root: &Path) -> Result<(), String> {
        let executor = Arc::new(Self::new(project_root).map_err(|error| error.to_string())?);
        <dyn RelHostExecutor>::install_process_executor(executor)
    }

    fn new(project_root: &Path) -> anyhow::Result<Self> {
        if !project_root.is_absolute() || !project_root.is_dir() {
            anyhow::bail!(
                "REL host executor requires an existing absolute project root: {}",
                project_root.display()
            );
        }
        ensure_no_symlink_components(project_root)?;
        let proxy_path = packaged_proxy_path()?;
        verify_proxy(&proxy_path)?;
        let cgroup_root = std::env::var_os("RBE_CONTAINER_CGROUP_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                let default = PathBuf::from("/sys/fs/cgroup");
                default.is_dir().then_some(default)
            });
        Ok(Self {
            project_root: project_root.to_path_buf(),
            proxy_path,
            cgroup_root,
        })
    }

    async fn execute_request(
        &self,
        request: RelHostRequest,
    ) -> Result<route_engine::Value, ModuleEvalError> {
        match request {
            RelHostRequest::Script(plan) => self.execute_script(plan, None).await,
            RelHostRequest::Temp(inner) => {
                let workspace = FreshWorkspace::new(&self.project_root).map_err(|error| {
                    rel_error(
                        "REL2213",
                        format!("could not create temporary workspace: {error}"),
                    )
                })?;
                match *inner {
                    RelHostRequest::Script(plan) => {
                        self.execute_script(plan, Some(workspace.path())).await
                    }
                    RelHostRequest::Archive(_) => Err(rel_error(
                        "REL2200",
                        "archive materialization inside workspace.temp() is not wired yet",
                    )),
                    RelHostRequest::Workspace(_) => Err(rel_error(
                        "REL2200",
                        "nested workspace plans are not materializable yet",
                    )),
                    RelHostRequest::Temp(_) => Err(rel_error(
                        "REL2200",
                        "nested workspace.temp() operations are not supported",
                    )),
                }
            }
            RelHostRequest::Archive(_) => Err(rel_error(
                "REL2200",
                "archive materialization is not wired to the trusted host executor yet",
            )),
            RelHostRequest::Workspace(_) => Err(rel_error(
                "REL2200",
                "workspace.construct() execution is not wired to the trusted host executor yet",
            )),
        }
    }

    async fn execute_script(
        &self,
        plan: ScriptPlan,
        temp_root: Option<&Path>,
    ) -> Result<route_engine::Value, ModuleEvalError> {
        if !cfg!(target_os = "linux") {
            return Err(rel_error(
                "REL2212",
                "secure script execution currently requires the Linux Container sandbox; RBE will not fall back to an unsandboxed host process",
            ));
        }
        if plan.path.language() == ScriptLanguage::Rust {
            return Err(rel_error(
                "REL2210",
                "script.runRust() requires the trusted Rust compile-and-run plan; raw rustc execution is intentionally not treated as an interpreted script",
            ));
        }

        let runtime_kind = runtime_kind(plan.path.language());
        let runtime_identity = plan.path.runtime_identity();
        let admitted = load_admitted_system_runtime(&self.project_root, runtime_kind).map_err(|error| {
            rel_error(
                "REL2211",
                format!(
                    "managed runtime {runtime_identity} is not admitted or changed after hydration: {error}. Hydrate the pinned rbe.sys runtime through RPX/install-runtime; PATH fallback is forbidden"
                ),
            )
        })?;

        let prepared = self
            .prepare_script_source(plan.path.workspace(), temp_root)
            .map_err(|error| rel_error("REL2213", error))?;
        let source_files = inventory_source_tree(&prepared.source_root)
            .map_err(|error| rel_error("REL2213", error))?;
        let timeout_seconds = plan.timeout_ms.saturating_add(999) / 1_000;
        let bootstrap = seal_proxy_bootstrap(
            &admitted.executable,
            &admitted.executable_sha256,
            &prepared.source_root,
            &prepared.entrypoint,
            source_files,
            plan.args,
            timeout_seconds.max(1),
        )
        .map_err(|error| {
            rel_error(
                "REL2215",
                format!("could not seal Container proxy bootstrap: {error}"),
            )
        })?;
        let output = self.run_proxy(bootstrap).await?;
        Ok(output.into_rel_value())
    }

    fn prepare_script_source(
        &self,
        path: &WorkspacePath,
        temp_root: Option<&Path>,
    ) -> Result<PreparedScriptSource, String> {
        match path.root() {
            WorkspaceRoot::Project => {
                let source = self.project_root.join(path.relative());
                require_regular_project_file(&self.project_root, &source)?;
                let parent = source
                    .parent()
                    .ok_or_else(|| "script source has no parent directory".to_string())?;
                if let Some(temp_root) = temp_root {
                    let staged_root = temp_root.join("source");
                    copy_tree_bounded(parent, &staged_root)?;
                    let name = source
                        .file_name()
                        .ok_or_else(|| "script source has no file name".to_string())?;
                    Ok(PreparedScriptSource {
                        source_root: staged_root.clone(),
                        entrypoint: staged_root.join(name),
                    })
                } else {
                    Ok(PreparedScriptSource {
                        source_root: parent.to_path_buf(),
                        entrypoint: source,
                    })
                }
            }
            WorkspaceRoot::Temp => {
                let temp_root = temp_root.ok_or_else(|| {
                    "a ??/ script path requires workspace.temp() or an active workspace plan"
                        .to_string()
                })?;
                let source = temp_root.join(path.relative());
                require_regular_project_file(temp_root, &source)?;
                Ok(PreparedScriptSource {
                    source_root: temp_root.to_path_buf(),
                    entrypoint: source,
                })
            }
        }
    }

    async fn run_proxy(
        &self,
        bootstrap: LibraryWorkerProxyBootstrap,
    ) -> Result<RelHostOutput, ModuleEvalError> {
        verify_proxy(&self.proxy_path).map_err(|error| {
            rel_error(
                "REL2214",
                format!(
                    "packaged Container Library Worker Proxy failed integrity verification: {error}"
                ),
            )
        })?;
        let cgroup_root = self.cgroup_root.as_ref().ok_or_else(|| {
            rel_error(
                "REL2212",
                "Linux cgroup-v2 root is unavailable; configure RBE_CONTAINER_CGROUP_ROOT with a delegated writable cgroup root",
            )
        })?;
        if !cgroup_root.is_absolute() || !cgroup_root.is_dir() {
            return Err(rel_error(
                "REL2212",
                format!(
                    "configured RBE_CONTAINER_CGROUP_ROOT is not an absolute directory: {}",
                    cgroup_root.display()
                ),
            ));
        }

        let mut bootstrap_bytes = Vec::new();
        write_library_worker_proxy_bootstrap(&mut bootstrap_bytes, &bootstrap)
            .map_err(|error| rel_error("REL2215", format!("invalid proxy bootstrap: {error}")))?;

        let mut command = Command::new(&self.proxy_path);
        command
            .arg("--cgroup-root")
            .arg(cgroup_root)
            .current_dir(&self.project_root)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            rel_error(
                "REL2215",
                format!("could not spawn verified Container Library Worker Proxy: {error}"),
            )
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            rel_error("REL2215", "Container proxy stdin pipe was not created")
        })?;
        stdin.write_all(&bootstrap_bytes).await.map_err(|error| {
            rel_error(
                "REL2215",
                format!("could not write Container proxy bootstrap: {error}"),
            )
        })?;
        stdin.shutdown().await.map_err(|error| {
            rel_error(
                "REL2215",
                format!("could not close Container proxy bootstrap pipe: {error}"),
            )
        })?;
        drop(stdin);

        let outer_timeout = Duration::from_secs(
            bootstrap
                .startup_timeout_seconds
                .saturating_add(PROXY_GRACE_SECONDS),
        );
        let output = timeout(outer_timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                rel_error(
                    "REL2215",
                    "Container proxy exceeded its bounded outer timeout",
                )
            })?
            .map_err(|error| {
                rel_error(
                    "REL2215",
                    format!("Container proxy wait failed: {error}"),
                )
            })?;

        let result = read_library_worker_proxy_result(&mut Cursor::new(output.stdout)).map_err(
            |error| {
                let proxy_stderr = bounded_lossy(&output.stderr, 32 * 1024);
                rel_error(
                    "REL2215",
                    format!(
                        "Container proxy returned an invalid result frame: {error}; proxy stderr: {proxy_stderr}"
                    ),
                )
            },
        )?;
        match result {
            LibraryWorkerProxyResult::Completed {
                exit_code,
                stdout,
                stderr,
                timed_out,
                output_limit_exceeded,
                cgroup_enforced,
                ..
            } => {
                if !cgroup_enforced {
                    return Err(rel_error(
                        "REL2212",
                        "Container proxy completed without cgroup enforcement; refusing the result",
                    ));
                }
                let mut stderr_text = String::from_utf8_lossy(&stderr).into_owned();
                if timed_out {
                    append_diagnostic(
                        &mut stderr_text,
                        "RBE terminated the script after its timeout",
                    );
                }
                if output_limit_exceeded {
                    append_diagnostic(
                        &mut stderr_text,
                        "RBE terminated the script after bounded output was exceeded",
                    );
                }
                Ok(RelHostOutput::bounded(
                    exit_code == 0 && !timed_out && !output_limit_exceeded,
                    String::from_utf8_lossy(&stdout).into_owned(),
                    stderr_text,
                    Some(exit_code),
                ))
            }
            LibraryWorkerProxyResult::Error { code, message } => Err(rel_error(
                "REL2215",
                format!("Container proxy rejected script execution ({code}): {message}"),
            )),
        }
    }
}

impl RelHostExecutor for BackendRelHostExecutor {
    fn execute<'a>(&'a self, request: RelHostRequest) -> RelHostExecutionFuture<'a> {
        Box::pin(async move { self.execute_request(request).await })
    }
}

struct PreparedScriptSource {
    source_root: PathBuf,
    entrypoint: PathBuf,
}

struct FreshWorkspace {
    path: PathBuf,
}

impl FreshWorkspace {
    fn new(project_root: &Path) -> Result<Self, String> {
        let parent = project_root.join(".cache").join("rbe").join("workspaces");
        ensure_no_symlink_components_allow_missing(&parent).map_err(|error| error.to_string())?;
        fs::create_dir_all(&parent).map_err(|error| error.to_string())?;
        ensure_no_symlink_components(&parent).map_err(|error| error.to_string())?;
        let path = parent.join(Uuid::new_v4().to_string());
        fs::create_dir(&path).map_err(|error| error.to_string())?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FreshWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn runtime_kind(language: ScriptLanguage) -> SystemRuntimeKind {
    match language {
        ScriptLanguage::JavaScript => SystemRuntimeKind::Nodejs,
        ScriptLanguage::TypeScript => SystemRuntimeKind::Bunjs,
        ScriptLanguage::Python => SystemRuntimeKind::Python,
        ScriptLanguage::PyPy => SystemRuntimeKind::PyPy,
        ScriptLanguage::Rust => SystemRuntimeKind::Rust,
    }
}

fn seal_proxy_bootstrap(
    program: &Path,
    program_sha256: &str,
    source_root: &Path,
    entrypoint: &Path,
    source_files: Vec<LibraryWorkerProxySourceFile>,
    arguments: Vec<String>,
    startup_timeout_seconds: u64,
) -> Result<LibraryWorkerProxyBootstrap, String> {
    let program = utf8_path("managed runtime", program)?;
    let working_directory = utf8_path("script source root", source_root)?;
    let entrypoint = utf8_path("script entrypoint", entrypoint)?;
    let mut args = Vec::with_capacity(arguments.len() + 1);
    args.push(entrypoint);
    args.extend(arguments);
    let bootstrap = LibraryWorkerProxyBootstrap {
        protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
        program,
        program_sha256: program_sha256.to_ascii_lowercase(),
        args,
        working_directory,
        source_files,
        clear_environment: true,
        environment: BTreeMap::new(),
        direct_network_allowed: false,
        use_shell: false,
        startup_timeout_seconds,
    };
    bootstrap.validate().map_err(|error| error.to_string())?;
    Ok(bootstrap)
}

fn require_regular_project_file(root: &Path, path: &Path) -> Result<(), String> {
    if !path.starts_with(root) {
        return Err("script path escaped its symbolic workspace root".into());
    }
    ensure_no_symlink_components(path).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "script path is not a regular non-symlink file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn copy_tree_bounded(source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err(format!(
            "temporary source destination already exists: {}",
            destination.display()
        ));
    }
    ensure_no_symlink_components(source).map_err(|error| error.to_string())?;
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    copy_tree_entries(source, destination, &mut file_count, &mut total_bytes)
}

fn copy_tree_entries(
    source: &Path,
    destination: &Path,
    file_count: &mut usize,
    total_bytes: &mut u64,
) -> Result<(), String> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "script source tree contains a symlink: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(&target).map_err(|error| error.to_string())?;
            copy_tree_entries(&source_path, &target, file_count, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "script source tree contains a non-regular entry: {}",
                source_path.display()
            ));
        }
        *file_count = file_count.saturating_add(1);
        *total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| "script source size overflow".to_string())?;
        if *file_count > MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES
            || *total_bytes > MAX_SCRIPT_SOURCE_BYTES
        {
            return Err(format!(
                "script source tree exceeds RBE limits (files={}, bytes={})",
                file_count, total_bytes
            ));
        }
        fs::copy(&source_path, &target).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn inventory_source_tree(root: &Path) -> Result<Vec<LibraryWorkerProxySourceFile>, String> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(format!(
            "script source root is not an absolute directory: {}",
            root.display()
        ));
    }
    ensure_no_symlink_components(root).map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    inventory_entries(root, root, &mut files, &mut total_bytes)?;
    if files.is_empty() {
        return Err("script source root contains no regular files".into());
    }
    Ok(files)
}

fn inventory_entries(
    root: &Path,
    directory: &Path,
    files: &mut Vec<LibraryWorkerProxySourceFile>,
    total_bytes: &mut u64,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "script source tree contains a symlink: {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            inventory_entries(root, &path, files, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "script source tree contains a non-regular entry: {}",
                path.display()
            ));
        }
        if files.len() >= MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES {
            return Err(format!(
                "script source tree exceeds {} files",
                MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES
            ));
        }
        if metadata.len() > MAX_SCRIPT_SOURCE_FILE_BYTES {
            return Err(format!(
                "script source file exceeds {MAX_SCRIPT_SOURCE_FILE_BYTES} bytes: {}",
                path.display()
            ));
        }
        *total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| "script source size overflow".to_string())?;
        if *total_bytes > MAX_SCRIPT_SOURCE_BYTES {
            return Err(format!(
                "script source tree exceeds {MAX_SCRIPT_SOURCE_BYTES} bytes"
            ));
        }
        let relative = relative_source_path(root, &path)?;
        files.push(LibraryWorkerProxySourceFile {
            path: relative,
            size: metadata.len(),
            sha256: sha256_file(&path).map_err(|error| error.to_string())?,
        });
    }
    Ok(())
}

fn relative_source_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "script source escaped its source root".to_string())?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err("script source contains a non-normal path component".into());
        };
        let value = value
            .to_str()
            .ok_or_else(|| "script source path is not valid UTF-8".to_string())?;
        if value.is_empty() || matches!(value, "." | "..") {
            return Err("script source contains an unsafe path component".into());
        }
        parts.push(value);
    }
    if parts.is_empty() {
        return Err("script source path is empty".into());
    }
    Ok(parts.join("/"))
}

fn utf8_path(label: &str, path: &Path) -> Result<String, String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{label} path is not valid UTF-8: {}", path.display()))
}

fn packaged_proxy_path() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let root = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backend executable has no parent directory"))?;
    let name = if cfg!(windows) {
        "container-library-worker-proxy.exe"
    } else {
        "container-library-worker-proxy"
    };
    Ok(root.join("dep").join(name))
}

fn verify_proxy(binary: &Path) -> anyhow::Result<()> {
    if proxy_integrity::EXPECTED_LIBRARY_WORKER_PROXY_SHA256.is_empty()
        || proxy_integrity::LIBRARY_WORKER_PROXY_PUBLIC_KEY_HEX.is_empty()
        || proxy_integrity::LIBRARY_WORKER_PROXY_SIGNATURE_HEX.is_empty()
    {
        anyhow::bail!(
            "Library Worker Proxy integrity metadata is absent from this backend build"
        );
    }
    let metadata = fs::symlink_metadata(binary)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "Library Worker Proxy is not a regular non-symlink file: {}",
            binary.display()
        );
    }
    let actual = sha256_file(binary)?;
    if !constant_time_eq(
        actual.as_bytes(),
        proxy_integrity::EXPECTED_LIBRARY_WORKER_PROXY_SHA256.as_bytes(),
    ) {
        anyhow::bail!(
            "Library Worker Proxy SHA-256 mismatch: expected {}, got {}",
            proxy_integrity::EXPECTED_LIBRARY_WORKER_PROXY_SHA256,
            actual
        );
    }
    let public_key = VerifyingKey::from_bytes(&decode_exact::<32>(
        proxy_integrity::LIBRARY_WORKER_PROXY_PUBLIC_KEY_HEX,
        "Library Worker Proxy public key",
    )?)?;
    let signature = Signature::from_bytes(&decode_exact::<64>(
        proxy_integrity::LIBRARY_WORKER_PROXY_SIGNATURE_HEX,
        "Library Worker Proxy signature",
    )?);
    let statement = format!(
        "RBE-LIBRARY-WORKER-PROXY-INTEGRITY-V1\nsha256={}\nbuild_id={}\ntarget={}\n",
        proxy_integrity::EXPECTED_LIBRARY_WORKER_PROXY_SHA256,
        proxy_integrity::LIBRARY_WORKER_PROXY_BUILD_ID,
        proxy_integrity::LIBRARY_WORKER_PROXY_TARGET,
    );
    public_key.verify(statement.as_bytes(), &signature)?;
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn decode_exact<const N: usize>(hex_value: &str, label: &str) -> anyhow::Result<[u8; N]> {
    let bytes = hex::decode(hex_value)
        .map_err(|error| anyhow::anyhow!("invalid {label}: {error}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid {label}: expected {N} bytes"))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn ensure_no_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("path traverses a symbolic link: {}", current.display());
        }
    }
    Ok(())
}

fn ensure_no_symlink_components_allow_missing(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("path traverses a symbolic link: {}", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn append_diagnostic(target: &mut String, message: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(message);
}

fn bounded_lossy(bytes: &[u8], maximum: usize) -> String {
    let bytes = if bytes.len() > maximum {
        &bytes[..maximum]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).into_owned()
}

fn rel_error(code: &'static str, message: impl Into<String>) -> ModuleEvalError {
    ModuleEvalError {
        code,
        message: message.into(),
    }
}
