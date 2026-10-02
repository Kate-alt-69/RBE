use std::collections::BTreeMap;
use std::fs::File as StdFile;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rbe_install_executor::{GitSourceAcquisitionPlan, GitSourceReceipt, SourceFileDigest};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const TREE_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const RESOLVE_OUTPUT_LIMIT: usize = 512;
const FETCH_SIZE_POLL: Duration = Duration::from_millis(100);

pub async fn fetch(plan: &GitSourceAcquisitionPlan) -> Result<String> {
    let invocation = plan.verify_before_fetch()?;
    if invocation.use_shell
        || !invocation.clear_environment
        || !invocation.require_fresh_workspace
        || !invocation.require_public_address_resolution
        || invocation.allowed_network_origins.as_slice() != [plan.origin()]
    {
        anyhow::bail!("Git source fetch plan violates RBE hosted-build policy");
    }

    prepare_workspace(
        &invocation.working_directory,
        &invocation.environment,
        &invocation.init_args,
    )
    .await?;
    verify_program(&invocation.program, &invocation.program_sha256)?;
    run_status(
        &invocation.program,
        &invocation.init_args,
        &invocation.working_directory,
        &invocation.environment,
        Duration::from_secs(30),
    )
    .await
    .context("initialize isolated bare Git object store")?;

    let mut fetch_environment = invocation.environment.clone();
    pin_repository_destination(plan, &mut fetch_environment).await?;
    verify_program(&invocation.program, &invocation.program_sha256)?;
    run_fetch_status(
        &invocation.program,
        &invocation.fetch_args,
        &invocation.working_directory,
        &fetch_environment,
        Duration::from_secs(invocation.timeout_seconds),
        invocation.maximum_download_bytes,
    )
    .await
    .context("fetch sealed Git source")?;

    verify_program(&invocation.program, &invocation.program_sha256)?;
    let resolved = run_stdout(
        &invocation.program,
        &invocation.resolve_args,
        &invocation.working_directory,
        &invocation.environment,
        Duration::from_secs(30),
        RESOLVE_OUTPUT_LIMIT,
    )
    .await
    .context("resolve fetched Git commit")?;
    let resolved = std::str::from_utf8(&resolved)
        .context("Git resolved commit is not UTF-8")?
        .trim();
    let materialization = plan.materialization(resolved)?;
    Ok(materialization.resolved_commit)
}

pub async fn materialize(
    plan: &GitSourceAcquisitionPlan,
    resolved_commit: &str,
    source_root: &Path,
) -> Result<GitSourceReceipt> {
    if !source_root.is_absolute() {
        anyhow::bail!("deployment source root must be absolute");
    }
    if source_root.exists() {
        anyhow::bail!("deployment source root must be fresh");
    }
    tokio::fs::create_dir_all(source_root)
        .await
        .context("create deployment source root")?;

    let materialization = plan.materialization(resolved_commit)?;
    if materialization.use_shell
        || !materialization.clear_environment
        || materialization.direct_network_allowed
    {
        anyhow::bail!("Git materialization plan violates network-dead RBE policy");
    }
    verify_program(&materialization.program, &materialization.program_sha256)?;
    let tree = run_stdout(
        &materialization.program,
        &materialization.list_tree_args,
        &materialization.working_directory,
        &materialization.environment,
        Duration::from_secs(plan.limits().timeout_seconds),
        TREE_OUTPUT_LIMIT,
    )
    .await
    .context("list immutable Git source tree")?;
    let entries = materialization.parse_tree(&tree)?;

    let mut total_bytes = 0_u64;
    let mut digests = Vec::with_capacity(entries.len());
    for entry in entries {
        let destination = safe_destination(source_root, &entry.path)?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let remaining = materialization
            .maximum_total_bytes
            .checked_sub(total_bytes)
            .context("Git source tree exceeded total-byte limit")?;
        let maximum = materialization.maximum_file_bytes.min(remaining);
        let args = materialization.blob_args(&entry.object_id)?;
        verify_program(&materialization.program, &materialization.program_sha256)?;
        let digest = stream_blob(
            &materialization.program,
            &args,
            &materialization.working_directory,
            &materialization.environment,
            &destination,
            &entry.path,
            maximum,
            Duration::from_secs(plan.limits().timeout_seconds),
        )
        .await
        .with_context(|| format!("materialize Git blob {:?}", entry.path))?;
        total_bytes = total_bytes
            .checked_add(digest.size)
            .context("Git source size accounting overflow")?;
        if total_bytes > materialization.maximum_total_bytes {
            anyhow::bail!("Git source tree exceeded total-byte limit");
        }
        set_executable_if_requested(&destination, entry.executable)?;
        digests.push(digest);
    }

    plan.seal_receipt(resolved_commit, &digests)
        .map_err(Into::into)
}

async fn pin_repository_destination(
    plan: &GitSourceAcquisitionPlan,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let repository = plan.repository();
    let host = repository.host_str().context("Git repository host missing")?;
    let port = repository
        .port_or_known_default()
        .context("Git repository port missing")?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        if forbidden_ip(ip) {
            anyhow::bail!("Git repository address is not publicly routable");
        }
        return Ok(());
    }

    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve Git repository host {host:?}"))?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        anyhow::bail!("Git repository DNS resolution returned no addresses");
    }
    if addresses.len() > 16 {
        anyhow::bail!("Git repository DNS resolution returned too many addresses");
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        anyhow::bail!("Git repository DNS resolution included a non-public address");
    }
    let address = addresses[0];
    install_curl_resolve(environment, host, port, address)?;
    Ok(())
}

fn install_curl_resolve(
    environment: &mut BTreeMap<String, String>,
    host: &str,
    port: u16,
    address: SocketAddr,
) -> Result<()> {
    for key in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"] {
        if environment.contains_key(key) {
            anyhow::bail!("sealed Git environment already contains dynamic config slots");
        }
    }
    let ip = match address.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    environment.insert("GIT_CONFIG_COUNT".to_owned(), "1".to_owned());
    environment.insert(
        "GIT_CONFIG_KEY_0".to_owned(),
        "http.curloptResolve".to_owned(),
    );
    environment.insert(
        "GIT_CONFIG_VALUE_0".to_owned(),
        format!("{host}:{port}:{ip}"),
    );
    Ok(())
}

async fn prepare_workspace(
    workspace: &Path,
    environment: &BTreeMap<String, String>,
    init_args: &[String],
) -> Result<()> {
    if !workspace.is_absolute() || workspace.exists() {
        anyhow::bail!("Git acquisition workspace must be a fresh absolute path");
    }
    tokio::fs::create_dir_all(workspace).await?;

    let config = PathBuf::from(
        environment
            .get("GIT_CONFIG_GLOBAL")
            .context("sealed Git environment is missing GIT_CONFIG_GLOBAL")?,
    );
    ensure_child(workspace, &config)?;
    tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&config)
        .await
        .context("create isolated empty Git config")?;

    let template = init_args
        .iter()
        .find_map(|arg| arg.strip_prefix("--template="))
        .map(PathBuf::from)
        .context("sealed Git init plan is missing empty template root")?;
    ensure_child(workspace, &template)?;
    tokio::fs::create_dir(&template)
        .await
        .context("create isolated empty Git template root")?;
    Ok(())
}

fn ensure_child(root: &Path, path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.starts_with(root) || path == root {
        anyhow::bail!("sealed Git helper path escapes acquisition workspace");
    }
    Ok(())
}

async fn run_status(
    program: &Path,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    let mut command = sealed_command(program, args, cwd, environment);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout(timeout, command.status())
        .await
        .context("sealed Git command timed out")??;
    if !status.success() {
        anyhow::bail!("sealed Git command exited with {status}");
    }
    Ok(())
}

async fn run_fetch_status(
    program: &Path,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
    maximum_bytes: u64,
) -> Result<()> {
    let mut command = sealed_command(program, args, cwd, environment);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.kill_on_drop(true);
    let mut child = command.spawn().context("spawn sealed Git fetch")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                anyhow::bail!("sealed Git fetch exited with {status}");
            }
            let root = cwd.to_path_buf();
            let size = tokio::task::spawn_blocking(move || tree_size_bounded(&root, maximum_bytes))
                .await??;
            if size > maximum_bytes {
                anyhow::bail!("Git fetch exceeded source byte budget");
            }
            return Ok(());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill().await;
            anyhow::bail!("sealed Git fetch timed out");
        }
        let root = cwd.to_path_buf();
        let size = tokio::task::spawn_blocking(move || tree_size_bounded(&root, maximum_bytes))
            .await??;
        if size > maximum_bytes {
            let _ = child.kill().await;
            anyhow::bail!("Git fetch exceeded source byte budget");
        }
        tokio::time::sleep(FETCH_SIZE_POLL).await;
    }
}

async fn run_stdout(
    program: &Path,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
    maximum_bytes: usize,
) -> Result<Vec<u8>> {
    let mut command = sealed_command(program, args, cwd, environment);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    command.kill_on_drop(true);
    let mut child = command.spawn().context("spawn sealed Git query")?;
    let mut stdout = child.stdout.take().context("capture sealed Git stdout")?;
    let operation = async {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            if output.len().saturating_add(read) > maximum_bytes {
                anyhow::bail!("sealed Git query exceeded output byte limit");
            }
            output.extend_from_slice(&buffer[..read]);
        }
        let status = child.wait().await?;
        if !status.success() {
            anyhow::bail!("sealed Git query exited with {status}");
        }
        Ok::<Vec<u8>, anyhow::Error>(output)
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("sealed Git query timed out")
        }
    }
}

async fn stream_blob(
    program: &Path,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    destination: &Path,
    source_path: &str,
    maximum_bytes: u64,
    timeout: Duration,
) -> Result<SourceFileDigest> {
    let mut command = sealed_command(program, args, cwd, environment);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    command.kill_on_drop(true);
    let mut child = command.spawn().context("spawn sealed Git blob reader")?;
    let mut stdout = child.stdout.take().context("capture Git blob stdout")?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .await
        .with_context(|| format!("create source file {}", destination.display()))?;

    let operation = async {
        let mut observed = 0_u64;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            observed = observed
                .checked_add(read as u64)
                .context("Git blob size accounting overflow")?;
            if observed > maximum_bytes {
                anyhow::bail!("Git blob exceeded source-file byte limit");
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read]).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            anyhow::bail!("sealed Git blob reader exited with {status}");
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok::<SourceFileDigest, anyhow::Error>(SourceFileDigest {
            path: source_path.to_owned(),
            size: observed,
            sha256: format!("{:x}", hasher.finalize()),
        })
    };

    let result = match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(digest)) => Ok(digest),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(anyhow::anyhow!("sealed Git blob reader timed out"))
        }
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

fn sealed_command(
    program: &Path,
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null());
    command
}

fn safe_destination(root: &Path, source_path: &str) -> Result<PathBuf> {
    let mut destination = root.to_path_buf();
    for part in source_path.split('/') {
        if part.is_empty() || matches!(part, "." | "..") {
            anyhow::bail!("invalid Git source path");
        }
        destination.push(part);
    }
    if !destination.starts_with(root) || destination == root {
        anyhow::bail!("Git source path escaped source root");
    }
    Ok(destination)
}

fn set_executable_if_requested(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

fn verify_program(path: &Path, expected_sha256: &str) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("managed Git executable path is not absolute");
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("managed Git executable is not a regular non-symlink file");
    }
    let mut file = StdFile::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        anyhow::bail!("managed Git executable changed after admission");
    }
    Ok(())
}

fn tree_size_bounded(root: &Path, limit: u64) -> Result<u64> {
    fn visit(path: &Path, total: &mut u64, limit: u64) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("Git acquisition workspace contains a symbolic link");
        }
        if metadata.is_file() {
            *total = total
                .checked_add(metadata.len())
                .context("Git acquisition size accounting overflow")?;
            return Ok(());
        }
        if !metadata.is_dir() {
            anyhow::bail!("Git acquisition workspace contains an unsupported filesystem entry");
        }
        for entry in std::fs::read_dir(path)? {
            visit(&entry?.path(), total, limit)?;
            if *total > limit {
                return Ok(());
            }
        }
        Ok(())
    }

    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    visit(root, &mut total, limit)?;
    Ok(total)
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_ipv4(ip),
        IpAddr::V6(ip) => forbidden_ipv6(ip),
    }
}

fn forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip == Ipv4Addr::BROADCAST
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn forbidden_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || ip.to_ipv4_mapped().is_some_and(forbidden_ipv4)
}
