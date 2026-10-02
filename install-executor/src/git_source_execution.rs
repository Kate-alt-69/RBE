//! Trusted execution for the sealed Git source-acquisition contract.
//!
//! Repository bytes are acquired only through an already-admitted managed Git
//! executable. DNS answers are validated before Git is started and then pinned
//! into libcurl with `http.curloptResolve`; repository code is never executed.
//! After the fetch resolves to one immutable commit, all tree/blob reads are
//! network-dead and the exact bytes written to `source_root` are hashed into the
//! deterministic RBE source-tree receipt.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{sleep, Instant};
use url::Url;

use crate::{
    GitSourceAcquisitionPlan, GitSourceError, GitSourceReceipt, GitTreeEntry, SourceFileDigest,
    SourceFileHasher, VerifiedGitSourceFetchInvocation, VerifiedGitTreeMaterialization,
};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_RESOLVE_OUTPUT_BYTES: usize = 512;
const MAX_OBJECT_SIZE_OUTPUT_BYTES: usize = 128;
const MAX_TREE_LISTING_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedGitSource {
    pub source_root: PathBuf,
    pub source_files: Vec<SourceFileDigest>,
    pub receipt: GitSourceReceipt,
}

/// Acquire and materialize one public HTTPS Git source tree using only the
/// sealed invocations produced by [`GitSourceAcquisitionPlan`].
pub async fn acquire_git_source(
    plan: &GitSourceAcquisitionPlan,
    source_root: impl AsRef<Path>,
) -> Result<MaterializedGitSource, GitSourceExecutionError> {
    let fetch = plan.verify_before_fetch()?;
    prepare_fetch_workspace(&fetch)?;

    let pinned_resolution = resolve_public_repository(plan.repository()).await?;
    let fetch_args = with_pinned_resolution(&fetch.fetch_args, &pinned_resolution)?;

    run_git(
        "init",
        &fetch.program,
        &fetch.program_sha256,
        &fetch.init_args,
        &fetch.working_directory,
        &fetch.environment,
        fetch.timeout_seconds,
        MAX_DIAGNOSTIC_BYTES,
        None,
    )
    .await?;

    let object_root = fetch.working_directory.join("objects.git").join("objects");
    run_git(
        "fetch",
        &fetch.program,
        &fetch.program_sha256,
        &fetch_args,
        &fetch.working_directory,
        &fetch.environment,
        fetch.timeout_seconds,
        MAX_DIAGNOSTIC_BYTES,
        Some((&object_root, fetch.maximum_download_bytes)),
    )
    .await?;

    let resolved = run_git(
        "resolve",
        &fetch.program,
        &fetch.program_sha256,
        &fetch.resolve_args,
        &fetch.working_directory,
        &fetch.environment,
        fetch.timeout_seconds,
        MAX_RESOLVE_OUTPUT_BYTES,
        None,
    )
    .await?;
    let resolved_commit = parse_single_line_utf8("resolved commit", &resolved.stdout)?;

    let materialization = plan.materialization(resolved_commit)?;
    let source_root = prepare_fresh_source_root(source_root.as_ref())?;
    let source_files =
        materialize_tree(&materialization, &source_root, fetch.timeout_seconds).await?;
    let receipt = plan.seal_receipt(&materialization.resolved_commit, &source_files)?;

    Ok(MaterializedGitSource {
        source_root,
        source_files,
        receipt,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedRepositoryResolution {
    host: String,
    port: u16,
    addresses: Vec<IpAddr>,
}

async fn resolve_public_repository(
    repository: &Url,
) -> Result<PinnedRepositoryResolution, GitSourceExecutionError> {
    let host = repository
        .host_str()
        .ok_or(GitSourceExecutionError::RepositoryHostMissing)?
        .to_ascii_lowercase();
    let port = repository.port_or_known_default().unwrap_or(443);

    let mut addresses = BTreeSet::new();
    if let Ok(address) = host.parse::<IpAddr>() {
        addresses.insert(address);
    } else {
        let resolved = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|source| GitSourceExecutionError::DnsLookup {
                host: host.clone(),
                source,
            })?;
        for socket in resolved {
            addresses.insert(socket.ip());
        }
    }

    if addresses.is_empty() {
        return Err(GitSourceExecutionError::DnsNoAddresses(host));
    }
    if let Some(address) = addresses
        .iter()
        .find(|address| !is_public_address(**address))
    {
        return Err(GitSourceExecutionError::DnsPrivateAddress {
            host,
            address: *address,
        });
    }

    Ok(PinnedRepositoryResolution {
        host,
        port,
        addresses: addresses.into_iter().collect(),
    })
}

fn with_pinned_resolution(
    args: &[String],
    resolution: &PinnedRepositoryResolution,
) -> Result<Vec<String>, GitSourceExecutionError> {
    if resolution.addresses.is_empty() {
        return Err(GitSourceExecutionError::DnsNoAddresses(
            resolution.host.clone(),
        ));
    }
    let Some(fetch_index) = args.iter().position(|arg| arg == "fetch") else {
        return Err(GitSourceExecutionError::MalformedSealedInvocation("fetch"));
    };
    let addresses = resolution
        .addresses
        .iter()
        .map(|address| match address {
            IpAddr::V4(address) => address.to_string(),
            IpAddr::V6(address) => format!("[{address}]"),
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut pinned = args.to_vec();
    pinned.splice(
        fetch_index..fetch_index,
        [
            "-c".to_string(),
            format!(
                "http.curloptResolve={}:{}:{}",
                resolution.host, resolution.port, addresses
            ),
        ],
    );
    Ok(pinned)
}

fn prepare_fetch_workspace(
    invocation: &VerifiedGitSourceFetchInvocation,
) -> Result<(), GitSourceExecutionError> {
    if !invocation.clear_environment
        || invocation.use_shell
        || !invocation.require_fresh_workspace
        || !invocation.require_public_address_resolution
        || invocation.allowed_network_origins.len() != 1
    {
        return Err(GitSourceExecutionError::UnsafeSealedInvocation("fetch"));
    }
    prepare_fresh_directory(&invocation.working_directory)?;
    let config = invocation.working_directory.join("gitconfig.empty");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config)?;
    std::fs::create_dir(invocation.working_directory.join("git-template.empty"))?;
    Ok(())
}

fn prepare_fresh_source_root(path: &Path) -> Result<PathBuf, GitSourceExecutionError> {
    if !path.is_absolute() {
        return Err(GitSourceExecutionError::UnsafeDestination(
            path.to_path_buf(),
        ));
    }
    prepare_fresh_directory(path)?;
    Ok(path.to_path_buf())
}

fn prepare_fresh_directory(path: &Path) -> Result<(), GitSourceExecutionError> {
    if !path.is_absolute() || path.parent().is_none() || path.exists() {
        return Err(GitSourceExecutionError::UnsafeDestination(
            path.to_path_buf(),
        ));
    }
    let parent = path.parent().expect("absolute non-root path has parent");
    if !parent.is_dir() {
        return Err(GitSourceExecutionError::UnsafeDestination(
            path.to_path_buf(),
        ));
    }
    ensure_no_symlink_components(parent)?;
    std::fs::create_dir(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GitSourceExecutionError::UnsafeDestination(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

async fn materialize_tree(
    materialization: &VerifiedGitTreeMaterialization,
    source_root: &Path,
    timeout_seconds: u64,
) -> Result<Vec<SourceFileDigest>, GitSourceExecutionError> {
    if !materialization.clear_environment
        || materialization.direct_network_allowed
        || materialization.use_shell
    {
        return Err(GitSourceExecutionError::UnsafeSealedInvocation(
            "materialize",
        ));
    }

    let listing_limit = materialization
        .maximum_total_bytes
        .min(MAX_TREE_LISTING_BYTES)
        .max(1) as usize;
    let listing = run_git(
        "list-tree",
        &materialization.program,
        &materialization.program_sha256,
        &materialization.list_tree_args,
        &materialization.working_directory,
        &materialization.environment,
        timeout_seconds,
        listing_limit,
        None,
    )
    .await?;
    let entries = materialization.parse_tree(&listing.stdout)?;

    let mut source_files = Vec::with_capacity(entries.len());
    let mut total_bytes = 0_u64;
    for entry in entries {
        let size_args = materialization.size_args(&entry.object_id)?;
        let size_output = run_git(
            "object-size",
            &materialization.program,
            &materialization.program_sha256,
            &size_args,
            &materialization.working_directory,
            &materialization.environment,
            timeout_seconds,
            MAX_OBJECT_SIZE_OUTPUT_BYTES,
            None,
        )
        .await?;
        let size_text = parse_single_line_utf8("Git object size", &size_output.stdout)?;
        let size = size_text
            .parse::<u64>()
            .map_err(|_| GitSourceExecutionError::InvalidObjectSize(size_text.to_string()))?;
        if size > materialization.maximum_file_bytes {
            return Err(GitSourceExecutionError::ObjectTooLarge {
                path: entry.path,
                observed: size,
                limit: materialization.maximum_file_bytes,
            });
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or(GitSourceExecutionError::SourceSizeOverflow)?;
        if total_bytes > materialization.maximum_total_bytes {
            return Err(GitSourceExecutionError::SourceTooLarge {
                observed: total_bytes,
                limit: materialization.maximum_total_bytes,
            });
        }

        let blob_args = materialization.blob_args(&entry.object_id)?;
        let blob = run_git(
            "object-read",
            &materialization.program,
            &materialization.program_sha256,
            &blob_args,
            &materialization.working_directory,
            &materialization.environment,
            timeout_seconds,
            usize::try_from(size).map_err(|_| GitSourceExecutionError::SourceSizeOverflow)?,
            None,
        )
        .await?;
        if blob.stdout.len() as u64 != size {
            return Err(GitSourceExecutionError::ObjectSizeMismatch {
                path: entry.path,
                expected: size,
                actual: blob.stdout.len() as u64,
            });
        }
        let digest = write_source_file(source_root, &entry, &blob.stdout, size)?;
        source_files.push(digest);
    }

    Ok(source_files)
}

fn write_source_file(
    source_root: &Path,
    entry: &GitTreeEntry,
    bytes: &[u8],
    expected_size: u64,
) -> Result<SourceFileDigest, GitSourceExecutionError> {
    let mut destination = source_root.to_path_buf();
    for part in entry.path.split('/') {
        destination.push(part);
    }
    if !destination.starts_with(source_root) {
        return Err(GitSourceExecutionError::UnsafeDestination(destination));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| GitSourceExecutionError::UnsafeDestination(destination.clone()))?;
    std::fs::create_dir_all(parent)?;
    ensure_no_symlink_components(parent)?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)?;
    use std::io::Write as _;
    file.write_all(bytes)?;
    file.sync_all()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if entry.executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode))?;
    }

    let mut hasher =
        SourceFileHasher::with_limit(entry.path.clone(), expected_size, expected_size.max(1))?;
    hasher.update(bytes)?;
    Ok(hasher.finish()?)
}

#[derive(Debug)]
struct GitCommandOutput {
    stdout: Vec<u8>,
}

async fn run_git(
    phase: &'static str,
    program: &Path,
    expected_program_sha256: &str,
    args: &[String],
    working_directory: &Path,
    environment: &std::collections::BTreeMap<String, String>,
    timeout_seconds: u64,
    stdout_limit: usize,
    disk_budget: Option<(&Path, u64)>,
) -> Result<GitCommandOutput, GitSourceExecutionError> {
    verify_program(program, expected_program_sha256)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(working_directory)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|source| GitSourceExecutionError::Spawn { phase, source })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(GitSourceExecutionError::MissingPipe {
            phase,
            pipe: "stdout",
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or(GitSourceExecutionError::MissingPipe {
            phase,
            pipe: "stderr",
        })?;
    let stdout_task = tokio::spawn(drain_output(stdout, stdout_limit));
    let stderr_task = tokio::spawn(drain_output(stderr, MAX_DIAGNOSTIC_BYTES));

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(GitSourceExecutionError::Timeout {
                phase,
                seconds: timeout_seconds,
            });
        }
        if let Some((root, limit)) = disk_budget {
            let observed = directory_regular_file_bytes(root)?;
            if observed > limit {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(GitSourceExecutionError::DownloadBudgetExceeded { observed, limit });
            }
        }
        sleep(PROCESS_POLL_INTERVAL).await;
    };

    let (stdout, stdout_exceeded) = stdout_task
        .await
        .map_err(|_| GitSourceExecutionError::OutputTask { phase })??;
    let (stderr, _) = stderr_task
        .await
        .map_err(|_| GitSourceExecutionError::OutputTask { phase })??;
    if stdout_exceeded {
        return Err(GitSourceExecutionError::OutputTooLarge {
            phase,
            limit: stdout_limit,
        });
    }
    if !status.success() {
        return Err(GitSourceExecutionError::ProcessFailed {
            phase,
            status: status.code(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }
    if let Some((root, limit)) = disk_budget {
        let observed = directory_regular_file_bytes(root)?;
        if observed > limit {
            return Err(GitSourceExecutionError::DownloadBudgetExceeded { observed, limit });
        }
    }
    Ok(GitCommandOutput { stdout })
}

async fn drain_output<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        if keep < read {
            exceeded = true;
        }
    }
    Ok((output, exceeded))
}

fn verify_program(path: &Path, expected_sha256: &str) -> Result<(), GitSourceExecutionError> {
    ensure_no_symlink_components(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(GitSourceExecutionError::UnsafeManagedGit(
            path.to_path_buf(),
        ));
    }
    let mut file = std::fs::File::open(path)?;
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
        return Err(GitSourceExecutionError::ManagedGitHashMismatch {
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

fn directory_regular_file_bytes(root: &Path) -> Result<u64, GitSourceExecutionError> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(GitSourceExecutionError::UnsafeDestination(path));
        }
        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or(GitSourceExecutionError::SourceSizeOverflow)?;
            continue;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(total)
}

fn ensure_no_symlink_components(path: &Path) -> Result<(), GitSourceExecutionError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(GitSourceExecutionError::UnsafeDestination(current));
        }
    }
    Ok(())
}

fn parse_single_line_utf8<'a>(
    label: &'static str,
    bytes: &'a [u8],
) -> Result<&'a str, GitSourceExecutionError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| GitSourceExecutionError::InvalidUtf8Output(label))?
        .trim();
    if text.is_empty() || text.lines().count() != 1 {
        return Err(GitSourceExecutionError::InvalidProcessOutput(label));
    }
    Ok(text)
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_ipv4(address),
        IpAddr::V6(address) => public_ipv6(address),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let shared_address_space = octets[0] == 100 && (64..=127).contains(&octets[1]);
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !shared_address_space
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
        return false;
    }
    let octets = address.octets();
    if octets[0] & 0xfe == 0xfc || (octets[0] == 0xfe && octets[1] & 0xc0 == 0x80) {
        return false;
    }
    let segments = address.segments();
    if segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff {
        return public_ipv4(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    true
}

#[derive(Debug, thiserror::Error)]
pub enum GitSourceExecutionError {
    #[error(transparent)]
    Plan(#[from] GitSourceError),
    #[error("Git source repository has no host")]
    RepositoryHostMissing,
    #[error("Git source DNS lookup failed for {host}: {source}")]
    DnsLookup {
        host: String,
        #[source]
        source: io::Error,
    },
    #[error("Git source DNS lookup returned no addresses for {0}")]
    DnsNoAddresses(String),
    #[error("Git source DNS lookup for {host} returned private/local address {address}")]
    DnsPrivateAddress { host: String, address: IpAddr },
    #[error("sealed Git {0} invocation is malformed")]
    MalformedSealedInvocation(&'static str),
    #[error("sealed Git {0} invocation violates trusted execution policy")]
    UnsafeSealedInvocation(&'static str),
    #[error("unsafe Git source destination {0}")]
    UnsafeDestination(PathBuf),
    #[error("managed Git is not a safe regular file: {0}")]
    UnsafeManagedGit(PathBuf),
    #[error("managed Git changed after admission: expected SHA-256 {expected}, got {actual}")]
    ManagedGitHashMismatch { expected: String, actual: String },
    #[error("could not spawn managed Git during {phase}: {source}")]
    Spawn {
        phase: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("managed Git {phase} process did not expose {pipe}")]
    MissingPipe {
        phase: &'static str,
        pipe: &'static str,
    },
    #[error("managed Git {phase} output task failed")]
    OutputTask { phase: &'static str },
    #[error("managed Git {phase} exceeded {limit} bytes of captured output")]
    OutputTooLarge { phase: &'static str, limit: usize },
    #[error("managed Git {phase} timed out after {seconds} seconds")]
    Timeout { phase: &'static str, seconds: u64 },
    #[error("managed Git {phase} failed with status {status:?}: {stderr}")]
    ProcessFailed {
        phase: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("Git source download exceeded byte budget: observed {observed}, limit {limit}")]
    DownloadBudgetExceeded { observed: u64, limit: u64 },
    #[error("invalid UTF-8 from managed Git while reading {0}")]
    InvalidUtf8Output(&'static str),
    #[error("invalid managed Git process output for {0}")]
    InvalidProcessOutput(&'static str),
    #[error("invalid Git object size {0:?}")]
    InvalidObjectSize(String),
    #[error("Git source object {path:?} is {observed} bytes, above limit {limit}")]
    ObjectTooLarge {
        path: String,
        observed: u64,
        limit: u64,
    },
    #[error("Git source object {path:?} size mismatch: expected {expected}, got {actual}")]
    ObjectSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("Git source size accounting overflow")]
    SourceSizeOverflow,
    #[error("Git source tree is {observed} bytes, above limit {limit}")]
    SourceTooLarge { observed: u64, limit: u64 },
    #[error("Git source execution I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Source(#[from] crate::SourceStageError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_address_policy_rejects_local_and_shared_ranges() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.10.2",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!is_public_address(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_address("8.8.8.8".parse().unwrap()));
        assert!(is_public_address("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn curl_resolution_pin_is_injected_before_fetch() {
        let resolution = PinnedRepositoryResolution {
            host: "github.com".into(),
            port: 443,
            addresses: vec![
                "140.82.112.4".parse().unwrap(),
                "2606:50c0:8000::154".parse().unwrap(),
            ],
        };
        let args = vec![
            "--no-pager".into(),
            "--git-dir".into(),
            "/tmp/objects.git".into(),
            "fetch".into(),
            "https://github.com/example/repo.git".into(),
            "main".into(),
        ];
        let pinned = with_pinned_resolution(&args, &resolution).unwrap();
        let fetch = pinned.iter().position(|arg| arg == "fetch").unwrap();
        assert_eq!(pinned[fetch - 2], "-c");
        assert_eq!(
            pinned[fetch - 1],
            "http.curloptResolve=github.com:443:140.82.112.4,[2606:50c0:8000::154]"
        );
    }
}
