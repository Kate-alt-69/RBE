//! Source-only execution contracts for RBE package installation.
//!
//! This crate does not open sockets, write files, extract archives, or spawn
//! processes. It defines the bounded plans and verification state that trusted
//! Backend orchestration can execute without falling back to shell/PATH trust.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rbe_install_request::{PortableArchive, SystemRuntimePlan};
use rbe_library_package::BuildStep;
use rbe_project_package::LockedArtifactFetch;
use sha2::{Digest, Sha256};
use url::Url;

pub const DEFAULT_DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_BUILD_TIMEOUT_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudgetPolicy {
    pub reserve_bytes: u64,
    pub unpack_overhead_bytes: u64,
    pub build_overhead_bytes: u64,
}

impl Default for DiskBudgetPolicy {
    fn default() -> Self {
        Self {
            reserve_bytes: DEFAULT_DISK_RESERVE_BYTES,
            unpack_overhead_bytes: 0,
            build_overhead_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudgetInput {
    pub artifact_bytes: u64,
    pub reusable_partial_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudget {
    pub download_bytes_remaining: u64,
    pub unpack_overhead_bytes: u64,
    pub build_overhead_bytes: u64,
    pub reserve_bytes: u64,
    pub required_bytes: u64,
    pub available_bytes: u64,
}

impl DiskBudget {
    pub fn plan(input: DiskBudgetInput, policy: DiskBudgetPolicy) -> Result<Self, ExecutorError> {
        if input.reusable_partial_bytes > input.artifact_bytes {
            return Err(ExecutorError::PartialLargerThanArtifact {
                partial: input.reusable_partial_bytes,
                artifact: input.artifact_bytes,
            });
        }
        let remaining = input.artifact_bytes - input.reusable_partial_bytes;
        let required = remaining
            .checked_add(policy.unpack_overhead_bytes)
            .and_then(|value| value.checked_add(policy.build_overhead_bytes))
            .and_then(|value| value.checked_add(policy.reserve_bytes))
            .ok_or(ExecutorError::DiskBudgetOverflow)?;
        if input.available_bytes < required {
            return Err(ExecutorError::InsufficientDiskSpace {
                required,
                available: input.available_bytes,
            });
        }
        Ok(Self {
            download_bytes_remaining: remaining,
            unpack_overhead_bytes: policy.unpack_overhead_bytes,
            build_overhead_bytes: policy.build_overhead_bytes,
            reserve_bytes: policy.reserve_bytes,
            required_bytes: required,
            available_bytes: input.available_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadLimits {
    pub maximum_bytes: u64,
    pub connect_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub maximum_redirects: u8,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            connect_timeout_seconds: 15,
            idle_timeout_seconds: 30,
            maximum_redirects: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDownloadPlan {
    pub package: String,
    pub version: String,
    pub source: Url,
    pub expected_sha256: String,
    pub expected_size_bytes: Option<u64>,
    pub staging_dir: PathBuf,
    pub partial_path: PathBuf,
    pub final_dir: PathBuf,
    pub final_artifact_path: PathBuf,
    pub limits: DownloadLimits,
    pub resume: ResumePolicy,
}

impl ArtifactDownloadPlan {
    pub fn from_locked(
        fetch: &LockedArtifactFetch,
        expected_size_bytes: Option<u64>,
    ) -> Result<Self, ExecutorError> {
        let expected_sha256 = canonical_sha256(&fetch.artifact_sha256)?;
        let library_root = fetch
            .cache_path
            .parent()
            .ok_or(ExecutorError::InvalidCachePath)?;
        let staging_dir = library_root.join(".staging").join(&expected_sha256);
        Ok(Self {
            package: fetch.package.clone(),
            version: fetch.version.clone(),
            source: require_https(fetch.artifact_url.clone())?,
            expected_sha256,
            expected_size_bytes,
            partial_path: staging_dir.join("artifact.rbe.part"),
            staging_dir,
            final_dir: fetch.cache_path.clone(),
            final_artifact_path: fetch.cache_path.join("artifact.rbe"),
            limits: DownloadLimits::default(),
            resume: ResumePolicy::default(),
        })
    }

    pub fn verifier(&self) -> Result<StreamingVerifier, ExecutorError> {
        let maximum_bytes = match self.expected_size_bytes {
            Some(expected) => expected.min(self.limits.maximum_bytes),
            None => self.limits.maximum_bytes,
        };
        StreamingVerifier::new(
            self.expected_sha256.clone(),
            self.expected_size_bytes,
            maximum_bytes,
        )
    }

    pub fn promotion(&self, verified: &VerifiedDownload) -> Result<PromotionPlan, ExecutorError> {
        if verified.sha256 != self.expected_sha256 {
            return Err(ExecutorError::HashMismatch {
                expected: self.expected_sha256.clone(),
                actual: verified.sha256.clone(),
            });
        }
        if self
            .expected_size_bytes
            .is_some_and(|expected| expected != verified.size_bytes)
        {
            return Err(ExecutorError::SizeMismatch {
                expected: self.expected_size_bytes.unwrap_or_default(),
                actual: verified.size_bytes,
            });
        }
        Ok(PromotionPlan {
            verified_partial: self.partial_path.clone(),
            final_dir: self.final_dir.clone(),
            final_artifact: self.final_artifact_path.clone(),
            create_final_dir: true,
            replace_existing: false,
            fsync_before_publish: true,
            fsync_parent_after_publish: true,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePolicy {
    pub enabled: bool,
    pub rehash_existing_prefix: bool,
    pub require_final_pinned_hash: bool,
    pub restart_on_range_rejection: bool,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            rehash_existing_prefix: true,
            require_final_pinned_hash: true,
            restart_on_range_rejection: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    pub offset_bytes: u64,
    pub range_header: String,
    pub must_rehash_prefix_first: bool,
}

impl ResumeRequest {
    pub fn for_partial(
        plan: &ArtifactDownloadPlan,
        partial_bytes: u64,
    ) -> Result<Option<Self>, ExecutorError> {
        if !plan.resume.enabled || partial_bytes == 0 {
            return Ok(None);
        }
        if plan
            .expected_size_bytes
            .is_some_and(|expected| partial_bytes >= expected)
        {
            return Err(ExecutorError::InvalidResumeOffset(partial_bytes));
        }
        if partial_bytes > plan.limits.maximum_bytes {
            return Err(ExecutorError::InvalidResumeOffset(partial_bytes));
        }
        Ok(Some(Self {
            offset_bytes: partial_bytes,
            range_header: format!("bytes={partial_bytes}-"),
            must_rehash_prefix_first: plan.resume.rehash_existing_prefix,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct StreamingVerifier {
    hasher: Sha256,
    expected_sha256: String,
    expected_size_bytes: Option<u64>,
    maximum_bytes: u64,
    observed_bytes: u64,
}

impl StreamingVerifier {
    pub fn new(
        expected_sha256: String,
        expected_size_bytes: Option<u64>,
        maximum_bytes: u64,
    ) -> Result<Self, ExecutorError> {
        let expected_sha256 = canonical_sha256(&expected_sha256)?;
        if maximum_bytes == 0 {
            return Err(ExecutorError::InvalidMaximumBytes);
        }
        if expected_size_bytes.is_some_and(|size| size > maximum_bytes) {
            return Err(ExecutorError::ArtifactExceedsLimit {
                limit: maximum_bytes,
                observed: expected_size_bytes.unwrap_or_default(),
            });
        }
        Ok(Self {
            hasher: Sha256::new(),
            expected_sha256,
            expected_size_bytes,
            maximum_bytes,
            observed_bytes: 0,
        })
    }

    pub fn update(&mut self, bytes: &[u8]) -> Result<(), ExecutorError> {
        let next = self
            .observed_bytes
            .checked_add(bytes.len() as u64)
            .ok_or(ExecutorError::ArtifactSizeOverflow)?;
        if next > self.maximum_bytes {
            return Err(ExecutorError::ArtifactExceedsLimit {
                limit: self.maximum_bytes,
                observed: next,
            });
        }
        self.hasher.update(bytes);
        self.observed_bytes = next;
        Ok(())
    }

    pub fn finish(self) -> Result<VerifiedDownload, ExecutorError> {
        if let Some(expected) = self.expected_size_bytes {
            if self.observed_bytes != expected {
                return Err(ExecutorError::SizeMismatch {
                    expected,
                    actual: self.observed_bytes,
                });
            }
        }
        let actual = format!("{:x}", self.hasher.finalize());
        if actual != self.expected_sha256 {
            return Err(ExecutorError::HashMismatch {
                expected: self.expected_sha256,
                actual,
            });
        }
        Ok(VerifiedDownload {
            sha256: actual,
            size_bytes: self.observed_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDownload {
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionPlan {
    pub verified_partial: PathBuf,
    pub final_dir: PathBuf,
    pub final_artifact: PathBuf,
    pub create_final_dir: bool,
    pub replace_existing: bool,
    pub fsync_before_publish: bool,
    pub fsync_parent_after_publish: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemRuntimeArtifactPlan {
    pub runtime: String,
    pub version: String,
    pub source: Url,
    pub sha256: String,
    pub size_bytes: u64,
    pub archive: PortableArchive,
    pub staging_path: PathBuf,
    pub install_dir: PathBuf,
    pub executable: PathBuf,
    pub limits: DownloadLimits,
}

impl SystemRuntimeArtifactPlan {
    pub fn from_runtime(plan: &SystemRuntimePlan) -> Result<Self, ExecutorError> {
        let sha256 = canonical_sha256(&plan.sha256)?;
        if plan.size_bytes > DEFAULT_MAX_ARTIFACT_BYTES {
            return Err(ExecutorError::ArtifactExceedsLimit {
                limit: DEFAULT_MAX_ARTIFACT_BYTES,
                observed: plan.size_bytes,
            });
        }
        Ok(Self {
            runtime: plan.runtime.key().to_string(),
            version: plan.version.clone(),
            source: require_https(plan.source.clone())?,
            sha256: sha256.clone(),
            size_bytes: plan.size_bytes,
            archive: plan.archive,
            staging_path: plan
                .cache_root
                .join(".staging")
                .join(format!("{sha256}.part")),
            install_dir: plan.install_dir.clone(),
            executable: plan.executable.clone(),
            limits: DownloadLimits::default(),
        })
    }

    pub fn verifier(&self) -> Result<StreamingVerifier, ExecutorError> {
        StreamingVerifier::new(
            self.sha256.clone(),
            Some(self.size_bytes),
            self.limits.maximum_bytes,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedToolchain {
    pub tools: BTreeMap<String, PathBuf>,
}

impl ManagedToolchain {
    pub fn new(tools: BTreeMap<String, PathBuf>) -> Result<Self, ExecutorError> {
        if tools.is_empty() {
            return Err(ExecutorError::EmptyToolchain);
        }
        for (name, path) in &tools {
            validate_tool_name(name)?;
            if path.as_os_str().is_empty() || !path.is_absolute() {
                return Err(ExecutorError::ToolPathMustBeAbsolute(name.clone()));
            }
        }
        Ok(Self { tools })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInvocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub clear_environment: bool,
    pub environment: BTreeMap<String, String>,
    pub network_allowed: bool,
    pub timeout_seconds: u64,
    pub use_shell: bool,
}

pub fn build_invocations(
    steps: &[BuildStep],
    toolchain: &ManagedToolchain,
    source_root: impl AsRef<Path>,
) -> Result<Vec<BuildInvocation>, ExecutorError> {
    let source_root = source_root.as_ref();
    if source_root.as_os_str().is_empty() {
        return Err(ExecutorError::InvalidSourceRoot);
    }
    steps
        .iter()
        .map(|step| {
            let program = toolchain
                .tools
                .get(&step.program)
                .ok_or_else(|| ExecutorError::UnknownManagedTool(step.program.clone()))?;
            Ok(BuildInvocation {
                program: program.clone(),
                args: step.args.clone(),
                working_directory: source_root.to_path_buf(),
                clear_environment: true,
                environment: BTreeMap::new(),
                network_allowed: false,
                timeout_seconds: DEFAULT_BUILD_TIMEOUT_SECONDS,
                use_shell: false,
            })
        })
        .collect()
}

fn validate_tool_name(value: &str) -> Result<(), ExecutorError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ExecutorError::InvalidToolName(value.to_string()));
    }
    Ok(())
}

fn canonical_sha256(value: &str) -> Result<String, ExecutorError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ExecutorError::InvalidSha256(value.to_string()));
    }
    Ok(value.to_ascii_lowercase())
}

fn require_https(url: Url) -> Result<Url, ExecutorError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ExecutorError::HttpsRequired(url.to_string()));
    }
    Ok(url)
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("cache path must have a library-cache parent")]
    InvalidCachePath,
    #[error("remote artifact URL must use HTTPS: {0:?}")]
    HttpsRequired(String),
    #[error("partial download {partial} bytes exceeds artifact size {artifact} bytes")]
    PartialLargerThanArtifact { partial: u64, artifact: u64 },
    #[error("disk-space calculation overflow")]
    DiskBudgetOverflow,
    #[error("insufficient disk space: need {required} bytes, have {available} bytes")]
    InsufficientDiskSpace { required: u64, available: u64 },
    #[error("invalid resume offset {0}")]
    InvalidResumeOffset(u64),
    #[error("artifact maximum byte limit must be greater than zero")]
    InvalidMaximumBytes,
    #[error("artifact size accounting overflow")]
    ArtifactSizeOverflow,
    #[error("artifact exceeded byte limit {limit}: observed {observed}")]
    ArtifactExceedsLimit { limit: u64, observed: u64 },
    #[error("artifact size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("artifact hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("managed toolchain must contain at least one executable")]
    EmptyToolchain,
    #[error("invalid managed tool name {0:?}")]
    InvalidToolName(String),
    #[error("managed tool {0:?} must resolve to an absolute path")]
    ToolPathMustBeAbsolute(String),
    #[error("build step requested unavailable managed tool {0:?}")]
    UnknownManagedTool(String),
    #[error("build source root must be non-empty")]
    InvalidSourceRoot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbe_install_request::{CacheHardeningPolicy, SystemRuntimeKind};

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn locked_fetch() -> LockedArtifactFetch {
        LockedArtifactFetch {
            package: "advancenet".into(),
            version: "4.0.1".into(),
            artifact_url: Url::parse("https://cdn.kastrick.invalid/advancenet.zip").unwrap(),
            artifact_sha256: ABC_SHA256.into(),
            manifest_sha256: "b".repeat(64),
            source_sha256: None,
            cache_path: PathBuf::from("/project/.cache/library").join(ABC_SHA256),
        }
    }

    #[test]
    fn disk_budget_accounts_for_resume_and_reserve() {
        let budget = DiskBudget::plan(
            DiskBudgetInput {
                artifact_bytes: 100,
                reusable_partial_bytes: 40,
                available_bytes: 500,
            },
            DiskBudgetPolicy {
                reserve_bytes: 100,
                unpack_overhead_bytes: 50,
                build_overhead_bytes: 25,
            },
        )
        .unwrap();
        assert_eq!(budget.download_bytes_remaining, 60);
        assert_eq!(budget.required_bytes, 235);
    }

    #[test]
    fn insufficient_disk_space_is_rejected_before_download() {
        let error = DiskBudget::plan(
            DiskBudgetInput {
                artifact_bytes: 200,
                reusable_partial_bytes: 0,
                available_bytes: 100,
            },
            DiskBudgetPolicy {
                reserve_bytes: 0,
                unpack_overhead_bytes: 0,
                build_overhead_bytes: 0,
            },
        )
        .unwrap_err();
        assert!(matches!(error, ExecutorError::InsufficientDiskSpace { .. }));
    }

    #[test]
    fn locked_download_uses_same_cache_filesystem_for_atomic_publish() {
        let plan = ArtifactDownloadPlan::from_locked(&locked_fetch(), Some(3)).unwrap();
        assert_eq!(
            plan.partial_path,
            PathBuf::from("/project/.cache/library/.staging")
                .join(ABC_SHA256)
                .join("artifact.rbe.part")
        );
        assert_eq!(
            plan.final_artifact_path,
            PathBuf::from("/project/.cache/library")
                .join(ABC_SHA256)
                .join("artifact.rbe")
        );
    }

    #[test]
    fn streaming_verifier_accepts_expected_bytes_and_enables_promotion() {
        let plan = ArtifactDownloadPlan::from_locked(&locked_fetch(), Some(3)).unwrap();
        let mut verifier = plan.verifier().unwrap();
        verifier.update(b"a").unwrap();
        verifier.update(b"bc").unwrap();
        let verified = verifier.finish().unwrap();
        let promotion = plan.promotion(&verified).unwrap();
        assert!(!promotion.replace_existing);
        assert!(promotion.fsync_before_publish);
    }

    #[test]
    fn streaming_verifier_rejects_tampering() {
        let plan = ArtifactDownloadPlan::from_locked(&locked_fetch(), Some(3)).unwrap();
        let mut verifier = plan.verifier().unwrap();
        verifier.update(b"abd").unwrap();
        assert!(matches!(
            verifier.finish(),
            Err(ExecutorError::HashMismatch { .. })
        ));
    }

    #[test]
    fn resume_requires_prefix_rehash_and_pinned_final_hash() {
        let plan = ArtifactDownloadPlan::from_locked(&locked_fetch(), Some(100)).unwrap();
        let resume = ResumeRequest::for_partial(&plan, 40).unwrap().unwrap();
        assert_eq!(resume.range_header, "bytes=40-");
        assert!(resume.must_rehash_prefix_first);
        assert!(plan.resume.require_final_pinned_hash);
    }

    #[test]
    fn system_runtime_artifact_stays_under_rbe_system_cache() {
        let runtime = SystemRuntimePlan {
            runtime: SystemRuntimeKind::Python,
            version: "3.13.7".into(),
            host: "windows-x86_64".into(),
            source: Url::parse("https://runtime.kastrick.invalid/python.zip").unwrap(),
            sha256: "a".repeat(64),
            size_bytes: 1024,
            archive: PortableArchive::Zip,
            cache_root: PathBuf::from("/project/.cache/rbe/sys/python"),
            manifest_cache_path: PathBuf::from("/project/.cache/rbe/sys/python/manifest.json"),
            install_dir: PathBuf::from("/project/.cache/rbe/sys/python/3.13.7/windows-x86_64"),
            executable: PathBuf::from(
                "/project/.cache/rbe/sys/python/3.13.7/windows-x86_64/python.exe",
            ),
            hardening: CacheHardeningPolicy::default(),
            mutate_system_path: false,
            visible_as_user_package: false,
        };
        let plan = SystemRuntimeArtifactPlan::from_runtime(&runtime).unwrap();
        assert!(plan
            .staging_path
            .starts_with("/project/.cache/rbe/sys/python/.staging"));
    }

    #[test]
    fn build_invocation_uses_managed_absolute_tools_without_shell_or_network() {
        let mut tools = BTreeMap::new();
        tools.insert(
            "cargo".into(),
            PathBuf::from("/project/.cache/rbe/sys/rust/bin/cargo"),
        );
        let toolchain = ManagedToolchain::new(tools).unwrap();
        let invocations = build_invocations(
            &[BuildStep {
                program: "cargo".into(),
                args: vec!["build".into(), "--release".into()],
            }],
            &toolchain,
            "/project/.cache/library/.build/advancenet",
        )
        .unwrap();
        assert_eq!(invocations.len(), 1);
        assert!(!invocations[0].use_shell);
        assert!(!invocations[0].network_allowed);
        assert!(invocations[0].clear_environment);
    }

    #[test]
    fn build_cannot_fall_back_to_unknown_system_path_tool() {
        let mut tools = BTreeMap::new();
        tools.insert("cargo".into(), PathBuf::from("/managed/cargo"));
        let toolchain = ManagedToolchain::new(tools).unwrap();
        let error = build_invocations(
            &[BuildStep {
                program: "powershell".into(),
                args: vec!["-Command".into(), "whoami".into()],
            }],
            &toolchain,
            "/source",
        )
        .unwrap_err();
        assert!(matches!(error, ExecutorError::UnknownManagedTool(_)));
    }
}
