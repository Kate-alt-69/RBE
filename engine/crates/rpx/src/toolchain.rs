//! Managed compiler selection contract for RPX authoring.
//!
//! RPX compilation must be able to run with absolute RBE-managed compiler paths
//! instead of silently trusting whatever executable happens to be first on the
//! host `PATH`. Managed compiler identities are pinned by SHA-256 and verified
//! immediately before process creation. A host compiler remains possible only
//! through an explicit authoring opt-in when no managed toolchain file exists.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const RPX_TOOLCHAIN_FORMAT: u32 = 2;
pub const RPX_TOOLCHAIN_RELATIVE_PATH: &str = ".rbe/rpx-toolchain.json";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCompilerToolchain {
    pub format: u32,
    pub tools: BTreeMap<String, ManagedCompilerTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCompilerTool {
    pub path: PathBuf,
    pub sha256: String,
}

impl ManagedCompilerToolchain {
    pub fn parse_json(input: &str) -> Result<Self, ToolchainError> {
        let toolchain: Self = serde_json::from_str(input)?;
        toolchain.validate()?;
        Ok(toolchain)
    }

    pub fn load(project_root: impl AsRef<Path>) -> Result<Self, ToolchainError> {
        let path = project_root.as_ref().join(RPX_TOOLCHAIN_RELATIVE_PATH);
        let input = fs::read_to_string(&path).map_err(|source| ToolchainError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse_json(&input).map_err(|source| ToolchainError::InvalidFile {
            path,
            source: Box::new(source),
        })
    }

    pub fn validate(&self) -> Result<(), ToolchainError> {
        if self.format != RPX_TOOLCHAIN_FORMAT {
            return Err(ToolchainError::UnsupportedFormat(self.format));
        }
        if self.tools.is_empty() {
            return Err(ToolchainError::EmptyToolchain);
        }
        for (name, tool) in &self.tools {
            validate_tool_name(name)?;
            if tool.path.as_os_str().is_empty() || !tool.path.is_absolute() {
                return Err(ToolchainError::ToolPathMustBeAbsolute {
                    tool: name.clone(),
                    path: tool.path.clone(),
                });
            }
            validate_sha256(&tool.sha256)?;
        }
        Ok(())
    }

    pub fn program(&self, tool: &str) -> Result<&ManagedCompilerTool, ToolchainError> {
        validate_tool_name(tool)?;
        self.tools
            .get(tool)
            .ok_or_else(|| ToolchainError::UnknownManagedTool(tool.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedCompiler {
    Managed { path: PathBuf, sha256: String },
    HostAuthoring(String),
}

impl ResolvedCompiler {
    pub fn display_name(&self) -> &str {
        match self {
            Self::Managed { path, .. } => path.to_str().unwrap_or("<managed-compiler>"),
            Self::HostAuthoring(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerResolver {
    managed: Option<ManagedCompilerToolchain>,
    allow_host_authoring: bool,
}

impl CompilerResolver {
    pub fn from_project(
        project_root: impl AsRef<Path>,
        allow_host_authoring: bool,
    ) -> Result<Self, ToolchainError> {
        let path = project_root.as_ref().join(RPX_TOOLCHAIN_RELATIVE_PATH);
        let managed = match fs::read_to_string(&path) {
            Ok(input) => Some(ManagedCompilerToolchain::parse_json(&input).map_err(|source| {
                ToolchainError::InvalidFile {
                    path,
                    source: Box::new(source),
                }
            })?),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(ToolchainError::Read { path, source }),
        };
        Ok(Self {
            managed,
            allow_host_authoring,
        })
    }

    pub fn managed(toolchain: ManagedCompilerToolchain) -> Result<Self, ToolchainError> {
        toolchain.validate()?;
        Ok(Self {
            managed: Some(toolchain),
            allow_host_authoring: false,
        })
    }

    pub fn resolve(&self, tool: &str) -> Result<ResolvedCompiler, ToolchainError> {
        validate_tool_name(tool)?;
        if let Some(toolchain) = &self.managed {
            let tool = toolchain.program(tool)?;
            return Ok(ResolvedCompiler::Managed {
                path: tool.path.clone(),
                sha256: tool.sha256.to_ascii_lowercase(),
            });
        }
        if self.allow_host_authoring {
            return Ok(ResolvedCompiler::HostAuthoring(tool.to_string()));
        }
        Err(ToolchainError::ManagedToolchainRequired {
            expected: RPX_TOOLCHAIN_RELATIVE_PATH,
            tool: tool.to_string(),
        })
    }

    pub fn is_managed(&self) -> bool {
        self.managed.is_some()
    }
}

pub fn sha256_file(path: impl AsRef<Path>) -> Result<String, ToolchainError> {
    let path = path.as_ref();
    let mut file = fs::File::open(path).map_err(|source| ToolchainError::ToolRead {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| ToolchainError::ToolRead {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn verify_managed_program(
    path: impl AsRef<Path>,
    expected_sha256: &str,
) -> Result<(), ToolchainError> {
    validate_sha256(expected_sha256)?;
    let path = path.as_ref();
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(ToolchainError::ToolHashMismatch {
            path: path.to_path_buf(),
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), ToolchainError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ToolchainError::InvalidToolName(value.to_string()));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), ToolchainError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ToolchainError::InvalidSha256(value.to_string()));
    }
    Ok(())
}

#[derive(Debug)]
pub enum ToolchainError {
    Json(serde_json::Error),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidFile {
        path: PathBuf,
        source: Box<ToolchainError>,
    },
    UnsupportedFormat(u32),
    EmptyToolchain,
    InvalidToolName(String),
    InvalidSha256(String),
    ToolPathMustBeAbsolute {
        tool: String,
        path: PathBuf,
    },
    UnknownManagedTool(String),
    ManagedToolchainRequired {
        expected: &'static str,
        tool: String,
    },
    ToolRead {
        path: PathBuf,
        source: std::io::Error,
    },
    ToolHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

impl From<serde_json::Error> for ToolchainError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl fmt::Display for ToolchainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid RPX toolchain JSON: {error}"),
            Self::Read { path, source } => {
                write!(formatter, "failed to read RPX toolchain {}: {source}", path.display())
            }
            Self::InvalidFile { path, source } => {
                write!(formatter, "invalid RPX toolchain {}: {source}", path.display())
            }
            Self::UnsupportedFormat(format) => {
                write!(formatter, "unsupported RPX toolchain format {format}")
            }
            Self::EmptyToolchain => write!(formatter, "RPX managed toolchain contains no tools"),
            Self::InvalidToolName(value) => write!(formatter, "invalid RPX tool name {value:?}"),
            Self::InvalidSha256(value) => {
                write!(formatter, "invalid RPX managed-tool SHA-256 {value:?}")
            }
            Self::ToolPathMustBeAbsolute { tool, path } => write!(
                formatter,
                "RBE-managed compiler {tool:?} must use an absolute path, got {}",
                path.display()
            ),
            Self::UnknownManagedTool(tool) => {
                write!(formatter, "RBE-managed compiler {tool:?} is not installed")
            }
            Self::ManagedToolchainRequired { expected, tool } => write!(
                formatter,
                "RBE-managed compiler {tool:?} is required; hydrate/write {expected} or explicitly opt into host authoring tools"
            ),
            Self::ToolRead { path, source } => write!(
                formatter,
                "failed to read RBE-managed compiler {} for identity verification: {source}",
                path.display()
            ),
            Self::ToolHashMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "RBE-managed compiler SHA-256 mismatch for {}: expected {expected}, got {actual}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ToolchainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Read { source, .. } | Self::ToolRead { source, .. } => Some(source),
            Self::InvalidFile { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn temp_project(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rpx-toolchain-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(path.join(".rbe")).unwrap();
        path
    }

    #[test]
    fn managed_toolchain_requires_absolute_paths() {
        let error = ManagedCompilerToolchain::parse_json(&format!(
            r#"{{"format":2,"tools":{{"cargo":{{"path":"bin/cargo","sha256":"{HASH_A}"}}}}}}"#
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            ToolchainError::ToolPathMustBeAbsolute { .. }
        ));
    }

    #[test]
    fn managed_toolchain_rejects_unpinned_or_invalid_hashes() {
        let error = ManagedCompilerToolchain::parse_json(
            r#"{"format":2,"tools":{"cargo":{"path":"/opt/rbe/rust/bin/cargo","sha256":"nope"}}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, ToolchainError::InvalidSha256(_)));
    }

    #[test]
    fn managed_toolchain_resolves_only_declared_tools() {
        let toolchain = ManagedCompilerToolchain::parse_json(&format!(
            r#"{{"format":2,"tools":{{"cargo":{{"path":"/opt/rbe/rust/bin/cargo","sha256":"{HASH_A}"}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            toolchain.program("cargo").unwrap().path,
            PathBuf::from("/opt/rbe/rust/bin/cargo")
        );
        assert!(matches!(
            toolchain.program("python").unwrap_err(),
            ToolchainError::UnknownManagedTool(_)
        ));
    }

    #[test]
    fn existing_managed_file_never_falls_back_to_host_path() {
        let project = temp_project("no-fallback");
        fs::write(
            project.join(RPX_TOOLCHAIN_RELATIVE_PATH),
            format!(
                r#"{{"format":2,"tools":{{"cargo":{{"path":"/opt/rbe/rust/bin/cargo","sha256":"{HASH_A}"}}}}}}"#
            ),
        )
        .unwrap();
        let resolver = CompilerResolver::from_project(&project, true).unwrap();
        assert!(resolver.is_managed());
        assert!(matches!(
            resolver.resolve("python").unwrap_err(),
            ToolchainError::UnknownManagedTool(_)
        ));
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn host_path_requires_explicit_authoring_opt_in() {
        let project = temp_project("host-opt-in");
        let strict = CompilerResolver::from_project(&project, false).unwrap();
        assert!(matches!(
            strict.resolve("node").unwrap_err(),
            ToolchainError::ManagedToolchainRequired { .. }
        ));

        let authoring = CompilerResolver::from_project(&project, true).unwrap();
        assert_eq!(
            authoring.resolve("node").unwrap(),
            ResolvedCompiler::HostAuthoring("node".into())
        );
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn malformed_managed_file_is_a_hard_error() {
        let project = temp_project("malformed");
        fs::write(
            project.join(RPX_TOOLCHAIN_RELATIVE_PATH),
            format!(
                r#"{{"format":99,"tools":{{"node":{{"path":"/opt/node/bin/node","sha256":"{HASH_A}"}}}}}}"#
            ),
        )
        .unwrap();
        let error = CompilerResolver::from_project(&project, true).unwrap_err();
        assert!(matches!(error, ToolchainError::InvalidFile { .. }));
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn managed_program_hash_is_verified_on_use() {
        let project = temp_project("hash");
        let tool = project.join("compiler.bin");
        fs::write(&tool, b"trusted compiler bytes").unwrap();
        let pinned = sha256_file(&tool).unwrap();
        verify_managed_program(&tool, &pinned).unwrap();

        fs::write(&tool, b"tampered compiler bytes").unwrap();
        let error = verify_managed_program(&tool, &pinned).unwrap_err();
        assert!(matches!(error, ToolchainError::ToolHashMismatch { .. }));
        fs::remove_dir_all(project).unwrap();
    }
}
