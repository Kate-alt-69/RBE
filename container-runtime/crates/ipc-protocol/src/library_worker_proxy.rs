use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

pub const LIBRARY_WORKER_PROXY_PROTOCOL_VERSION: u16 = 1;
pub const MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES: usize = 16_384;
pub const MAX_LIBRARY_WORKER_PROXY_PATH_BYTES: usize = 4 * 1024;
pub const MAX_LIBRARY_WORKER_PROXY_STARTUP_SECONDS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryWorkerProxySourceFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryWorkerProxyBootstrap {
    pub protocol: u16,
    pub program: String,
    pub program_sha256: String,
    pub args: Vec<String>,
    pub working_directory: String,
    pub source_files: Vec<LibraryWorkerProxySourceFile>,
    pub clear_environment: bool,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub direct_network_allowed: bool,
    pub use_shell: bool,
    pub startup_timeout_seconds: u64,
}

impl LibraryWorkerProxyBootstrap {
    /// Validate the complete trusted Backend -> Container worker handoff.
    ///
    /// Protocol v1 intentionally supports only one interpreted entrypoint
    /// argument. Extra interpreter flags, inherited environment, shell launch,
    /// and direct networking are rejected rather than silently weakened.
    pub fn validate(&self) -> Result<(), LibraryWorkerProxyError> {
        if self.protocol != LIBRARY_WORKER_PROXY_PROTOCOL_VERSION {
            return Err(LibraryWorkerProxyError::UnsupportedProtocol(self.protocol));
        }
        validate_absolute_path("program", &self.program)?;
        validate_sha256("program_sha256", &self.program_sha256)?;
        validate_absolute_path("working_directory", &self.working_directory)?;

        if self.args.len() != 1 {
            return Err(LibraryWorkerProxyError::InvalidArgumentCount(
                self.args.len(),
            ));
        }
        let entrypoint = &self.args[0];
        validate_absolute_path("entrypoint", entrypoint)?;
        let root = Path::new(&self.working_directory);
        let entrypoint_path = Path::new(entrypoint);
        if entrypoint_path == root || !entrypoint_path.starts_with(root) {
            return Err(LibraryWorkerProxyError::EntrypointOutsideSourceRoot);
        }

        if !self.clear_environment || !self.environment.is_empty() {
            return Err(LibraryWorkerProxyError::EnvironmentMustBeEmpty);
        }
        if self.direct_network_allowed {
            return Err(LibraryWorkerProxyError::DirectNetworkForbidden);
        }
        if self.use_shell {
            return Err(LibraryWorkerProxyError::ShellForbidden);
        }
        if self.startup_timeout_seconds == 0
            || self.startup_timeout_seconds > MAX_LIBRARY_WORKER_PROXY_STARTUP_SECONDS
        {
            return Err(LibraryWorkerProxyError::InvalidStartupTimeout(
                self.startup_timeout_seconds,
            ));
        }
        if self.source_files.is_empty()
            || self.source_files.len() > MAX_LIBRARY_WORKER_PROXY_SOURCE_FILES
        {
            return Err(LibraryWorkerProxyError::InvalidSourceFileCount(
                self.source_files.len(),
            ));
        }

        let mut paths = BTreeSet::new();
        for file in &self.source_files {
            validate_relative_source_path(&file.path)?;
            validate_sha256("source sha256", &file.sha256)?;
            if !paths.insert(file.path.clone()) {
                return Err(LibraryWorkerProxyError::DuplicateSourceFile(
                    file.path.clone(),
                ));
            }
        }

        let entrypoint_relative = relative_source_path(root, entrypoint_path)?;
        if !paths.contains(&entrypoint_relative) {
            return Err(
                LibraryWorkerProxyError::EntrypointMissingFromSourceManifest(entrypoint_relative),
            );
        }
        Ok(())
    }
}

fn validate_absolute_path(field: &'static str, value: &str) -> Result<(), LibraryWorkerProxyError> {
    if value.is_empty()
        || value.len() > MAX_LIBRARY_WORKER_PROXY_PATH_BYTES
        || value.chars().any(char::is_control)
        || !Path::new(value).is_absolute()
    {
        return Err(LibraryWorkerProxyError::InvalidAbsolutePath(field));
    }
    Ok(())
}

fn validate_relative_source_path(value: &str) -> Result<(), LibraryWorkerProxyError> {
    if value.is_empty()
        || value.len() > MAX_LIBRARY_WORKER_PROXY_PATH_BYTES
        || value.ends_with('/')
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(LibraryWorkerProxyError::InvalidRelativeSourcePath(
            value.to_string(),
        ));
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), LibraryWorkerProxyError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LibraryWorkerProxyError::InvalidSha256(field));
    }
    Ok(())
}

fn relative_source_path(root: &Path, path: &Path) -> Result<String, LibraryWorkerProxyError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| LibraryWorkerProxyError::EntrypointOutsideSourceRoot)?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(LibraryWorkerProxyError::EntrypointOutsideSourceRoot);
        };
        let value = value
            .to_str()
            .ok_or(LibraryWorkerProxyError::EntrypointPathNotUtf8)?;
        if value.is_empty() || matches!(value, "." | "..") {
            return Err(LibraryWorkerProxyError::EntrypointOutsideSourceRoot);
        }
        parts.push(value);
    }
    if parts.is_empty() {
        return Err(LibraryWorkerProxyError::EntrypointOutsideSourceRoot);
    }
    Ok(parts.join("/"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryWorkerProxyError {
    UnsupportedProtocol(u16),
    InvalidAbsolutePath(&'static str),
    InvalidSha256(&'static str),
    InvalidArgumentCount(usize),
    EntrypointOutsideSourceRoot,
    EntrypointPathNotUtf8,
    EnvironmentMustBeEmpty,
    DirectNetworkForbidden,
    ShellForbidden,
    InvalidStartupTimeout(u64),
    InvalidSourceFileCount(usize),
    InvalidRelativeSourcePath(String),
    DuplicateSourceFile(String),
    EntrypointMissingFromSourceManifest(String),
}

impl fmt::Display for LibraryWorkerProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocol(version) => {
                write!(
                    formatter,
                    "unsupported Library Worker Proxy protocol {version}"
                )
            }
            Self::InvalidAbsolutePath(field) => {
                write!(
                    formatter,
                    "Library Worker Proxy {field} must be a bounded absolute path"
                )
            }
            Self::InvalidSha256(field) => {
                write!(
                    formatter,
                    "Library Worker Proxy {field} must be a SHA-256 digest"
                )
            }
            Self::InvalidArgumentCount(count) => {
                write!(
                    formatter,
                    "Library Worker Proxy v1 requires exactly one entrypoint argument, got {count}"
                )
            }
            Self::EntrypointOutsideSourceRoot => formatter
                .write_str("Library Worker Proxy entrypoint must be inside the source root"),
            Self::EntrypointPathNotUtf8 => {
                formatter.write_str("Library Worker Proxy entrypoint path must be valid UTF-8")
            }
            Self::EnvironmentMustBeEmpty => {
                formatter.write_str("Library Worker Proxy requires a cleared empty environment")
            }
            Self::DirectNetworkForbidden => {
                formatter.write_str("Library Worker Proxy forbids direct worker networking")
            }
            Self::ShellForbidden => {
                formatter.write_str("Library Worker Proxy forbids shell launch")
            }
            Self::InvalidStartupTimeout(seconds) => {
                write!(
                    formatter,
                    "invalid Library Worker Proxy startup timeout {seconds}s"
                )
            }
            Self::InvalidSourceFileCount(count) => {
                write!(
                    formatter,
                    "invalid Library Worker Proxy source file count {count}"
                )
            }
            Self::InvalidRelativeSourcePath(path) => {
                write!(
                    formatter,
                    "invalid Library Worker Proxy source path {path:?}"
                )
            }
            Self::DuplicateSourceFile(path) => {
                write!(
                    formatter,
                    "duplicate Library Worker Proxy source path {path:?}"
                )
            }
            Self::EntrypointMissingFromSourceManifest(path) => {
                write!(
                    formatter,
                    "Library Worker Proxy entrypoint {path:?} is absent from the source manifest"
                )
            }
        }
    }
}

impl Error for LibraryWorkerProxyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> LibraryWorkerProxyBootstrap {
        LibraryWorkerProxyBootstrap {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            program: "/managed/bun".into(),
            program_sha256: "a".repeat(64),
            args: vec!["/worker/worker.js".into()],
            working_directory: "/worker".into(),
            source_files: vec![
                LibraryWorkerProxySourceFile {
                    path: "worker.js".into(),
                    size: 12,
                    sha256: "b".repeat(64),
                },
                LibraryWorkerProxySourceFile {
                    path: "internal/helper.js".into(),
                    size: 7,
                    sha256: "c".repeat(64),
                },
            ],
            clear_environment: true,
            environment: BTreeMap::new(),
            direct_network_allowed: false,
            use_shell: false,
            startup_timeout_seconds: 30,
        }
    }

    #[test]
    fn strict_bootstrap_accepts_verified_interpreted_worker_shape() {
        valid().validate().unwrap();
    }

    #[test]
    fn authority_widening_fields_fail_closed() {
        let mut value = valid();
        value.direct_network_allowed = true;
        assert_eq!(
            value.validate(),
            Err(LibraryWorkerProxyError::DirectNetworkForbidden)
        );

        let mut value = valid();
        value.use_shell = true;
        assert_eq!(
            value.validate(),
            Err(LibraryWorkerProxyError::ShellForbidden)
        );

        let mut value = valid();
        value.environment.insert("PATH".into(), "/tmp".into());
        assert_eq!(
            value.validate(),
            Err(LibraryWorkerProxyError::EnvironmentMustBeEmpty)
        );
    }

    #[test]
    fn entrypoint_must_be_inside_and_present_in_source_manifest() {
        let mut outside = valid();
        outside.args[0] = "/outside/worker.js".into();
        assert_eq!(
            outside.validate(),
            Err(LibraryWorkerProxyError::EntrypointOutsideSourceRoot)
        );

        let mut missing = valid();
        missing.source_files.remove(0);
        assert_eq!(
            missing.validate(),
            Err(LibraryWorkerProxyError::EntrypointMissingFromSourceManifest("worker.js".into()))
        );
    }

    #[test]
    fn malformed_integrity_manifest_is_rejected() {
        let mut bad_program = valid();
        bad_program.program_sha256 = "nope".into();
        assert_eq!(
            bad_program.validate(),
            Err(LibraryWorkerProxyError::InvalidSha256("program_sha256"))
        );

        let mut duplicate = valid();
        duplicate
            .source_files
            .push(duplicate.source_files[0].clone());
        assert_eq!(
            duplicate.validate(),
            Err(LibraryWorkerProxyError::DuplicateSourceFile(
                "worker.js".into()
            ))
        );
    }

    #[test]
    fn protocol_and_timeout_are_bounded() {
        let mut wrong_protocol = valid();
        wrong_protocol.protocol += 1;
        assert_eq!(
            wrong_protocol.validate(),
            Err(LibraryWorkerProxyError::UnsupportedProtocol(2))
        );

        let mut timeout = valid();
        timeout.startup_timeout_seconds = MAX_LIBRARY_WORKER_PROXY_STARTUP_SECONDS + 1;
        assert_eq!(
            timeout.validate(),
            Err(LibraryWorkerProxyError::InvalidStartupTimeout(
                MAX_LIBRARY_WORKER_PROXY_STARTUP_SECONDS + 1
            ))
        );
    }
}
