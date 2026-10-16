//! Sealed Git source-acquisition contracts for hosted RBE builds.
//!
//! This module does not open sockets or spawn Git. It defines the exact
//! public-HTTPS fetch, immutable commit resolution, and network-dead Git object
//! materialization that trusted orchestration may execute. Source bytes come
//! directly from Git blobs instead of a checkout/archive so attributes,
//! filters, hooks, submodules, or host Git configuration cannot silently
//! redefine the source snapshot consumed by RBE.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Component, Path, PathBuf};

use url::{Host, Url};

use crate::{
    PinnedManagedTool, PinnedManagedToolchain, PinnedToolchainError, SourceFileDigest,
    SourceSelection, SourceStageError, SourceTreeDigest,
};

pub const DEFAULT_GIT_SOURCE_TIMEOUT_SECONDS: u64 = 5 * 60;
pub const DEFAULT_GIT_SOURCE_MAXIMUM_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_GIT_SOURCE_MAXIMUM_FILES: usize = 100_000;
pub const DEFAULT_GIT_SOURCE_MAXIMUM_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitSourceLimits {
    pub maximum_bytes: u64,
    pub maximum_files: usize,
    pub maximum_file_bytes: u64,
    pub timeout_seconds: u64,
}

impl Default for GitSourceLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: DEFAULT_GIT_SOURCE_MAXIMUM_BYTES,
            maximum_files: DEFAULT_GIT_SOURCE_MAXIMUM_FILES,
            maximum_file_bytes: DEFAULT_GIT_SOURCE_MAXIMUM_FILE_BYTES,
            timeout_seconds: DEFAULT_GIT_SOURCE_TIMEOUT_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSourceAcquisitionPlan {
    repository: Url,
    requested_ref: String,
    origin: String,
    git: PinnedManagedTool,
    workspace_root: PathBuf,
    object_store: PathBuf,
    isolated_global_config: PathBuf,
    empty_template_root: PathBuf,
    limits: GitSourceLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedGitSourceFetchInvocation {
    pub program: PathBuf,
    pub program_sha256: String,
    pub init_args: Vec<String>,
    pub fetch_args: Vec<String>,
    pub resolve_args: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub allowed_network_origins: Vec<String>,
    pub maximum_download_bytes: u64,
    pub timeout_seconds: u64,
    pub clear_environment: bool,
    pub use_shell: bool,
    pub require_fresh_workspace: bool,
    pub require_public_address_resolution: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedGitTreeMaterialization {
    pub program: PathBuf,
    pub program_sha256: String,
    pub list_tree_args: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub resolved_commit: String,
    pub maximum_files: usize,
    pub maximum_total_bytes: u64,
    pub maximum_file_bytes: u64,
    pub clear_environment: bool,
    pub direct_network_allowed: bool,
    pub use_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub path: String,
    pub object_id: String,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSourceReceipt {
    pub repository: String,
    pub origin: String,
    pub requested_ref: String,
    pub resolved_commit: String,
    pub source_tree: SourceTreeDigest,
}

impl GitSourceAcquisitionPlan {
    pub fn new(
        repository: impl AsRef<str>,
        requested_ref: impl Into<String>,
        toolchain: &PinnedManagedToolchain,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, GitSourceError> {
        Self::with_limits(
            repository,
            requested_ref,
            toolchain,
            workspace_root,
            GitSourceLimits::default(),
        )
    }

    pub fn with_limits(
        repository: impl AsRef<str>,
        requested_ref: impl Into<String>,
        toolchain: &PinnedManagedToolchain,
        workspace_root: impl AsRef<Path>,
        limits: GitSourceLimits,
    ) -> Result<Self, GitSourceError> {
        validate_limits(limits)?;
        let repository = validate_repository(repository.as_ref())?;
        let requested_ref = normalize_requested_ref(&requested_ref.into())?;
        let workspace_root = validate_workspace_root(workspace_root.as_ref())?;
        let git = toolchain.verified_tool("git")?.clone();
        let origin = repository.origin().ascii_serialization();
        let object_store = workspace_root.join("objects.git");
        let isolated_global_config = workspace_root.join("gitconfig.empty");
        let empty_template_root = workspace_root.join("git-template.empty");

        Ok(Self {
            repository,
            requested_ref,
            origin,
            git,
            workspace_root,
            object_store,
            isolated_global_config,
            empty_template_root,
            limits,
        })
    }

    pub fn repository(&self) -> &Url {
        &self.repository
    }

    pub fn requested_ref(&self) -> &str {
        &self.requested_ref
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn limits(&self) -> GitSourceLimits {
        self.limits
    }

    /// Re-verify the pinned Git executable immediately before the only
    /// network-enabled source-acquisition phase.
    pub fn verify_before_fetch(&self) -> Result<VerifiedGitSourceFetchInvocation, GitSourceError> {
        self.git.verify("git")?;
        let object_store = path_text(&self.object_store)?;
        let empty_template_root = path_text(&self.empty_template_root)?;

        let mut init_args = git_configuration_args();
        init_args.push("init".to_string());
        init_args.push("--bare".to_string());
        init_args.push(format!("--template={empty_template_root}"));
        init_args.push(object_store.clone());

        let mut fetch_args = git_configuration_args();
        fetch_args.push("--git-dir".to_string());
        fetch_args.push(object_store.clone());
        fetch_args.push("fetch".to_string());
        fetch_args.push("--no-tags".to_string());
        fetch_args.push("--no-recurse-submodules".to_string());
        fetch_args.push("--depth=1".to_string());
        fetch_args.push("--force".to_string());
        fetch_args.push(self.repository.as_str().to_string());
        fetch_args.push(self.requested_ref.clone());

        let mut resolve_args = git_configuration_args();
        resolve_args.push("--git-dir".to_string());
        resolve_args.push(object_store);
        resolve_args.push("rev-parse".to_string());
        resolve_args.push("--verify".to_string());
        resolve_args.push("FETCH_HEAD^{commit}".to_string());

        Ok(VerifiedGitSourceFetchInvocation {
            program: self.git.path().to_path_buf(),
            program_sha256: self.git.sha256().to_string(),
            init_args,
            fetch_args,
            resolve_args,
            working_directory: self.workspace_root.clone(),
            environment: isolated_git_environment(&self.isolated_global_config)?,
            allowed_network_origins: vec![self.origin.clone()],
            maximum_download_bytes: self.limits.maximum_bytes,
            timeout_seconds: self.limits.timeout_seconds,
            clear_environment: true,
            use_shell: false,
            require_fresh_workspace: true,
            require_public_address_resolution: true,
        })
    }

    /// Build the network-dead exact-object materialization contract after the
    /// fetch result has been reduced to one immutable commit object ID.
    pub fn materialization(
        &self,
        resolved_commit: impl AsRef<str>,
    ) -> Result<VerifiedGitTreeMaterialization, GitSourceError> {
        self.git.verify("git")?;
        let resolved_commit = normalize_object_id(resolved_commit.as_ref())?;
        let object_store = path_text(&self.object_store)?;

        let mut list_tree_args = git_configuration_args();
        list_tree_args.push("--git-dir".to_string());
        list_tree_args.push(object_store);
        list_tree_args.push("ls-tree".to_string());
        list_tree_args.push("-r".to_string());
        list_tree_args.push("-z".to_string());
        list_tree_args.push("--full-tree".to_string());
        list_tree_args.push(resolved_commit.clone());

        Ok(VerifiedGitTreeMaterialization {
            program: self.git.path().to_path_buf(),
            program_sha256: self.git.sha256().to_string(),
            list_tree_args,
            working_directory: self.workspace_root.clone(),
            environment: isolated_git_environment(&self.isolated_global_config)?,
            resolved_commit,
            maximum_files: self.limits.maximum_files,
            maximum_total_bytes: self.limits.maximum_bytes,
            maximum_file_bytes: self.limits.maximum_file_bytes,
            clear_environment: true,
            direct_network_allowed: false,
            use_shell: false,
        })
    }

    /// Seal a receipt only from file digests produced after exact Git-blob
    /// materialization. This binds mutable repository/ref metadata to both the
    /// immutable commit and RBE's deterministic source-tree digest.
    pub fn seal_receipt(
        &self,
        resolved_commit: impl AsRef<str>,
        source_files: &[SourceFileDigest],
    ) -> Result<GitSourceReceipt, GitSourceError> {
        let resolved_commit = normalize_object_id(resolved_commit.as_ref())?;
        validate_source_files(source_files, self.limits)?;

        let roots = source_files
            .iter()
            .map(|file| file.path.split('/').next().unwrap_or_default().to_string())
            .collect::<BTreeSet<_>>();
        if roots.contains("") {
            return Err(GitSourceError::InvalidTreePath(String::new()));
        }

        let selection = SourceSelection::new(roots)?;
        let source_tree = SourceTreeDigest::from_files(&selection, source_files.iter().cloned())?;

        Ok(GitSourceReceipt {
            repository: self.repository.as_str().to_string(),
            origin: self.origin.clone(),
            requested_ref: self.requested_ref.clone(),
            resolved_commit,
            source_tree,
        })
    }
}

impl VerifiedGitTreeMaterialization {
    pub fn parse_tree(&self, bytes: &[u8]) -> Result<Vec<GitTreeEntry>, GitSourceError> {
        parse_ls_tree_z(bytes, self.maximum_files)
    }

    pub fn size_args(&self, object_id: &str) -> Result<Vec<String>, GitSourceError> {
        self.cat_file_args("-s", object_id)
    }

    pub fn blob_args(&self, object_id: &str) -> Result<Vec<String>, GitSourceError> {
        self.cat_file_args("blob", object_id)
    }

    fn cat_file_args(&self, mode: &str, object_id: &str) -> Result<Vec<String>, GitSourceError> {
        let object_id = normalize_object_id(object_id)?;
        let mut args = git_configuration_args();
        args.push("--git-dir".to_string());
        args.push(path_text(&self.working_directory.join("objects.git"))?);
        args.push("cat-file".to_string());
        args.push(mode.to_string());
        args.push(object_id);
        Ok(args)
    }
}

/// Parse `git ls-tree -r -z --full-tree` output and reject every entry type
/// except regular blobs. Symlinks (120000), gitlinks/submodules (160000), and
/// unexpected modes/types never become hosted build source files.
pub fn parse_ls_tree_z(
    bytes: &[u8],
    maximum_files: usize,
) -> Result<Vec<GitTreeEntry>, GitSourceError> {
    if maximum_files == 0 {
        return Err(GitSourceError::InvalidLimits);
    }

    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in bytes.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        if entries.len() >= maximum_files {
            return Err(GitSourceError::TooManyFiles {
                limit: maximum_files,
                observed: entries.len().saturating_add(1),
            });
        }

        let text = std::str::from_utf8(raw).map_err(|_| GitSourceError::InvalidTreeEntry)?;
        let (metadata, path) = text
            .split_once('\t')
            .ok_or(GitSourceError::InvalidTreeEntry)?;
        let mut fields = metadata.split(' ');
        let mode = fields.next().ok_or(GitSourceError::InvalidTreeEntry)?;
        let kind = fields.next().ok_or(GitSourceError::InvalidTreeEntry)?;
        let object_id = fields.next().ok_or(GitSourceError::InvalidTreeEntry)?;

        if fields.next().is_some() || kind != "blob" || !matches!(mode, "100644" | "100755") {
            return Err(GitSourceError::UnsupportedTreeEntry {
                path: path.to_string(),
                mode: mode.to_string(),
                kind: kind.to_string(),
            });
        }

        validate_tree_path(path)?;
        let object_id = normalize_object_id(object_id)?;
        if !seen.insert(path.to_ascii_lowercase()) {
            return Err(GitSourceError::DuplicateTreePath(path.to_string()));
        }
        entries.push(GitTreeEntry {
            path: path.to_string(),
            object_id,
            executable: mode == "100755",
        });
    }

    if entries.is_empty() {
        return Err(GitSourceError::EmptySourceTree);
    }
    Ok(entries)
}

fn validate_source_files(
    source_files: &[SourceFileDigest],
    limits: GitSourceLimits,
) -> Result<(), GitSourceError> {
    if source_files.is_empty() {
        return Err(GitSourceError::EmptySourceTree);
    }
    if source_files.len() > limits.maximum_files {
        return Err(GitSourceError::TooManyFiles {
            limit: limits.maximum_files,
            observed: source_files.len(),
        });
    }

    let mut total = 0_u64;
    for file in source_files {
        if file.size > limits.maximum_file_bytes {
            return Err(GitSourceError::FileTooLarge {
                path: file.path.clone(),
                limit: limits.maximum_file_bytes,
                observed: file.size,
            });
        }
        total = total
            .checked_add(file.size)
            .ok_or(GitSourceError::SourceSizeOverflow)?;
    }
    if total > limits.maximum_bytes {
        return Err(GitSourceError::SourceTooLarge {
            limit: limits.maximum_bytes,
            observed: total,
        });
    }
    Ok(())
}

fn git_configuration_args() -> Vec<String> {
    let settings = [
        "protocol.version=2",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "protocol.http.allow=never",
        "protocol.ssh.allow=never",
        "protocol.git.allow=never",
        "protocol.file.allow=never",
        "protocol.ext.allow=never",
        "credential.helper=",
        "core.askPass=",
        "http.followRedirects=false",
        "fetch.recurseSubmodules=false",
        "submodule.recurse=false",
        "fetch.writeCommitGraph=false",
        "transfer.fsckObjects=true",
        "fetch.fsckObjects=true",
    ];

    let mut args = Vec::with_capacity(settings.len() * 2 + 1);
    args.push("--no-pager".to_string());
    for setting in settings {
        args.push("-c".to_string());
        args.push(setting.to_string());
    }
    args
}

fn isolated_git_environment(
    global_config: &Path,
) -> Result<BTreeMap<String, String>, GitSourceError> {
    Ok(BTreeMap::from([
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), path_text(global_config)?),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GCM_INTERACTIVE".to_string(), "Never".to_string()),
        ("GIT_ATTR_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string()),
        ("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string()),
    ]))
}

fn validate_limits(limits: GitSourceLimits) -> Result<(), GitSourceError> {
    if limits.maximum_bytes == 0
        || limits.maximum_files == 0
        || limits.maximum_file_bytes == 0
        || limits.maximum_file_bytes > limits.maximum_bytes
        || limits.timeout_seconds == 0
    {
        return Err(GitSourceError::InvalidLimits);
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<Url, GitSourceError> {
    let repository = Url::parse(value).map_err(|_| GitSourceError::InvalidRepository)?;
    if repository.scheme() != "https"
        || !repository.username().is_empty()
        || repository.password().is_some()
        || repository.query().is_some()
        || repository.fragment().is_some()
        || repository.path().is_empty()
        || repository.path() == "/"
    {
        return Err(GitSourceError::InvalidRepository);
    }

    match repository.host().ok_or(GitSourceError::InvalidRepository)? {
        Host::Ipv4(address) if !public_ipv4(address) => {
            return Err(GitSourceError::PrivateRepositoryHost);
        }
        Host::Ipv6(address) if !public_ipv6(address) => {
            return Err(GitSourceError::PrivateRepositoryHost);
        }
        Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            if domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".local")
            {
                return Err(GitSourceError::PrivateRepositoryHost);
            }
        }
        _ => {}
    }
    Ok(repository)
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
        let mapped = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return public_ipv4(mapped);
    }
    true
}

fn normalize_requested_ref(value: &str) -> Result<String, GitSourceError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.ends_with(".lock")
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value
            .chars()
            .any(|ch| matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return Err(GitSourceError::InvalidRequestedRef(value.to_string()));
    }
    Ok(value.to_string())
}

fn normalize_object_id(value: &str) -> Result<String, GitSourceError> {
    let value = value.trim();
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitSourceError::InvalidObjectId(value.to_string()));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_workspace_root(path: &Path) -> Result<PathBuf, GitSourceError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(GitSourceError::UnsafeWorkspace(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

fn validate_tree_path(path: &str) -> Result<(), GitSourceError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(GitSourceError::InvalidTreePath(path.to_string()));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String, GitSourceError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| GitSourceError::NonUtf8Path(path.to_path_buf()))
}

#[derive(Debug, thiserror::Error)]
pub enum GitSourceError {
    #[error("Git source repository must be a public credential-free HTTPS URL")]
    InvalidRepository,
    #[error("Git source repository resolves directly to a private/local address")]
    PrivateRepositoryHost,
    #[error("invalid Git source ref {0:?}")]
    InvalidRequestedRef(String),
    #[error("invalid Git object ID {0:?}")]
    InvalidObjectId(String),
    #[error("Git source limits are invalid")]
    InvalidLimits,
    #[error("unsafe Git source workspace {0}")]
    UnsafeWorkspace(PathBuf),
    #[error("Git source path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("invalid Git ls-tree entry")]
    InvalidTreeEntry,
    #[error("unsupported Git tree entry {path:?}: mode={mode:?} type={kind:?}")]
    UnsupportedTreeEntry {
        path: String,
        mode: String,
        kind: String,
    },
    #[error("invalid Git tree path {0:?}")]
    InvalidTreePath(String),
    #[error("duplicate or case-colliding Git tree path {0:?}")]
    DuplicateTreePath(String),
    #[error("Git source tree is empty")]
    EmptySourceTree,
    #[error("Git source tree contains {observed} files, above limit {limit}")]
    TooManyFiles { limit: usize, observed: usize },
    #[error("Git source file {path:?} is {observed} bytes, above limit {limit}")]
    FileTooLarge {
        path: String,
        limit: u64,
        observed: u64,
    },
    #[error("Git source tree is {observed} bytes, above limit {limit}")]
    SourceTooLarge { limit: u64, observed: u64 },
    #[error("Git source size accounting overflow")]
    SourceSizeOverflow,
    #[error(transparent)]
    Toolchain(#[from] PinnedToolchainError),
    #[error(transparent)]
    Source(#[from] SourceStageError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use sha2::{Digest, Sha256};

    use crate::ManagedToolchain;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rbe-git-source-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn toolchain(root: &Path) -> PinnedManagedToolchain {
        let git = root.join("git");
        std::fs::write(&git, b"managed-git-v1").unwrap();
        let managed = ManagedToolchain::new(BTreeMap::from([("git".to_string(), git)])).unwrap();
        PinnedManagedToolchain::pin(&managed).unwrap()
    }

    fn plan(temp: &TestDir) -> GitSourceAcquisitionPlan {
        GitSourceAcquisitionPlan::new(
            "https://github.com/Kate-alt-69/RBE.git",
            "refs/heads/main",
            &toolchain(&temp.0),
            temp.0.join("source-work"),
        )
        .unwrap()
    }

    fn digest(path: &str, bytes: &[u8]) -> SourceFileDigest {
        SourceFileDigest {
            path: path.to_string(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    #[test]
    fn public_https_fetch_is_isolated_and_origin_bound() {
        let temp = TestDir::new();
        let invocation = plan(&temp).verify_before_fetch().unwrap();
        assert_eq!(invocation.allowed_network_origins, ["https://github.com"]);
        assert!(invocation.clear_environment);
        assert!(!invocation.use_shell);
        assert!(invocation.require_fresh_workspace);
        assert!(invocation.require_public_address_resolution);
        assert_eq!(invocation.environment["GIT_TERMINAL_PROMPT"], "0");
        assert!(invocation
            .fetch_args
            .iter()
            .any(|arg| arg == "protocol.allow=never"));
        assert!(invocation
            .fetch_args
            .iter()
            .any(|arg| arg == "protocol.https.allow=always"));
        assert!(invocation
            .fetch_args
            .iter()
            .any(|arg| arg == "http.followRedirects=false"));
        assert!(invocation
            .fetch_args
            .iter()
            .any(|arg| arg == "--no-recurse-submodules"));
        assert!(invocation
            .resolve_args
            .iter()
            .any(|arg| arg == "FETCH_HEAD^{commit}"));
    }

    #[test]
    fn credentials_non_https_and_local_hosts_are_rejected() {
        let temp = TestDir::new();
        let pinned = toolchain(&temp.0);
        for repository in [
            "http://github.com/Kate-alt-69/RBE.git",
            "https://user:secret@github.com/Kate-alt-69/RBE.git",
            "https://localhost/repo.git",
            "https://127.0.0.1/repo.git",
            "https://10.0.0.2/repo.git",
            "https://100.64.0.1/repo.git",
            "https://[::1]/repo.git",
        ] {
            assert!(GitSourceAcquisitionPlan::new(
                repository,
                "main",
                &pinned,
                temp.0.join("work")
            )
            .is_err());
        }
    }

    #[test]
    fn tree_parser_accepts_only_regular_blob_modes() {
        let blob = "a".repeat(40);
        let input = format!(
            "100644 blob {blob}\tpackage.rbe.yaml\0\
             100755 blob {blob}\tscripts/build.sh\0"
        );
        let entries = parse_ls_tree_z(input.as_bytes(), 10).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].executable);
        assert!(entries[1].executable);

        let symlink = format!("120000 blob {blob}\tlink\0");
        assert!(matches!(
            parse_ls_tree_z(symlink.as_bytes(), 10),
            Err(GitSourceError::UnsupportedTreeEntry { .. })
        ));
        let gitlink = format!("160000 commit {blob}\tsubmodule\0");
        assert!(matches!(
            parse_ls_tree_z(gitlink.as_bytes(), 10),
            Err(GitSourceError::UnsupportedTreeEntry { .. })
        ));
    }

    #[test]
    fn materialization_is_network_dead_and_object_id_bound() {
        let temp = TestDir::new();
        let commit = "b".repeat(40);
        let materialization = plan(&temp).materialization(&commit).unwrap();
        assert!(!materialization.direct_network_allowed);
        assert!(!materialization.use_shell);
        assert!(materialization.clear_environment);
        assert_eq!(materialization.resolved_commit, commit);
        assert!(materialization
            .list_tree_args
            .iter()
            .any(|arg| arg == &commit));

        let blob = "c".repeat(40);
        assert_eq!(
            materialization.blob_args(&blob).unwrap().last(),
            Some(&blob)
        );
        assert_eq!(
            materialization.size_args(&blob).unwrap().last(),
            Some(&blob)
        );
    }

    #[test]
    fn receipt_binds_commit_and_deterministic_source_tree() {
        let temp = TestDir::new();
        let plan = plan(&temp);
        let commit = "d".repeat(40);
        let first = plan
            .seal_receipt(
                &commit,
                &[
                    digest("src/main.rs", b"fn main() {}"),
                    digest("package.rbe.yaml", b"name: demo"),
                ],
            )
            .unwrap();
        let second = plan
            .seal_receipt(
                &commit,
                &[
                    digest("package.rbe.yaml", b"name: demo"),
                    digest("src/main.rs", b"fn main() {}"),
                ],
            )
            .unwrap();
        assert_eq!(first.resolved_commit, commit);
        assert_eq!(first.source_tree, second.source_tree);
        assert_eq!(first.origin, "https://github.com");
    }

    #[test]
    fn replaced_git_binary_is_rejected_before_fetch() {
        let temp = TestDir::new();
        let pinned = toolchain(&temp.0);
        let plan = GitSourceAcquisitionPlan::new(
            "https://github.com/Kate-alt-69/RBE.git",
            "main",
            &pinned,
            temp.0.join("work"),
        )
        .unwrap();
        std::fs::write(temp.0.join("git"), b"replaced-git").unwrap();
        assert!(matches!(
            plan.verify_before_fetch(),
            Err(GitSourceError::Toolchain(
                PinnedToolchainError::ToolHashMismatch { .. }
            ))
        ));
    }
}
