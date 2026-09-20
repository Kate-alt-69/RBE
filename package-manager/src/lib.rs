//! Source-only package-manager presentation and local tooling plans.
//!
//! This crate deliberately performs no downloads, process spawning, PATH edits,
//! or machine-wide installation. It turns trusted installer state into useful CLI
//! events and plans isolated RBE-managed Python/SDK locations for a future
//! `backend install` command to execute.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

use rbe_library_installer::{
    ConfigArtifact, HostTarget, InstallerError, PackageRequest, RegistryEndpoints, SdkRegistryPlan,
    VersionCatalog,
};
use rbe_library_lock::ProjectLayout;
use rbe_library_package::HostOs;
use semver::Version;
use serde::Serialize;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Interactive,
    Plain,
    JsonLines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalStyle {
    pub progress_width: usize,
    pub unicode: bool,
    pub ansi: bool,
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            progress_width: 24,
            unicode: true,
            ansi: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolingScope {
    Project,
    UserShared,
}

impl ToolingScope {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::UserShared => "user-shared",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum InstallEvent {
    Resolving {
        roots: Vec<String>,
    },
    Resolved {
        packages: usize,
    },
    Downloading {
        package: String,
        version: String,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        bytes_per_second: Option<u64>,
        eta_seconds: Option<u64>,
    },
    Downloaded {
        package: String,
        version: String,
        bytes: u64,
    },
    CacheHit {
        package: String,
        version: String,
    },
    Verifying {
        package: String,
    },
    Verified {
        package: String,
        sha256: String,
    },
    RuntimeReady {
        kind: String,
        version: String,
        scope: ToolingScope,
        cached: bool,
    },
    SdkReady {
        family: String,
        version: String,
        scope: ToolingScope,
        cached: bool,
    },
    Building {
        package: String,
        version: String,
    },
    Built {
        package: String,
        version: String,
    },
    HandshakeVerified {
        package: String,
    },
    Installed {
        package: String,
        version: String,
    },
    Warning {
        message: String,
    },
    Summary {
        packages: usize,
        dependencies: usize,
        downloaded_bytes: u64,
        cache_hits: usize,
        elapsed_millis: u64,
    },
}

pub fn render_event(
    event: &InstallEvent,
    mode: OutputMode,
    style: TerminalStyle,
) -> Result<String, PackageManagerError> {
    match mode {
        OutputMode::JsonLines => Ok(serde_json::to_string(event)?),
        OutputMode::Plain => Ok(render_human(event, style, false)),
        OutputMode::Interactive => Ok(render_human(event, style, true)),
    }
}

fn render_human(event: &InstallEvent, style: TerminalStyle, interactive: bool) -> String {
    let ok = symbol(style, "✓", "OK");
    let down = symbol(style, "↓", "v");
    let dot = symbol(style, "•", "*");
    let warn = symbol(style, "!", "!");

    match event {
        InstallEvent::Resolving { roots } => {
            format!("{} Resolving {}", dot, clean(&roots.join(", ")))
        }
        InstallEvent::Resolved { packages } => {
            paint(style, 32, &format!("{} Resolved {} package(s)", ok, packages))
        }
        InstallEvent::Downloading {
            package,
            version,
            downloaded_bytes,
            total_bytes,
            bytes_per_second,
            eta_seconds,
        } => render_download(
            style,
            interactive,
            down,
            package,
            version,
            *downloaded_bytes,
            *total_bytes,
            *bytes_per_second,
            *eta_seconds,
        ),
        InstallEvent::Downloaded {
            package,
            version,
            bytes,
        } => paint(
            style,
            32,
            &format!(
                "{} Downloaded {} {} ({})",
                ok,
                clean(package),
                clean(version),
                format_bytes(*bytes)
            ),
        ),
        InstallEvent::CacheHit { package, version } => format!(
            "{} {} {} already cached",
            dot,
            clean(package),
            clean(version)
        ),
        InstallEvent::Verifying { package } => {
            format!("{} Verifying {}", dot, clean(package))
        }
        InstallEvent::Verified { package, sha256 } => paint(
            style,
            32,
            &format!(
                "{} Verified {} (sha256:{})",
                ok,
                clean(package),
                short_hash(sha256)
            ),
        ),
        InstallEvent::RuntimeReady {
            kind,
            version,
            scope,
            cached,
        } => format!(
            "{} Runtime {} {} ready [{}{}]",
            ok,
            clean(kind),
            clean(version),
            scope.label(),
            if *cached { ", cached" } else { "" }
        ),
        InstallEvent::SdkReady {
            family,
            version,
            scope,
            cached,
        } => format!(
            "{} SDK {} {} ready [{}{}]",
            ok,
            clean(family),
            clean(version),
            scope.label(),
            if *cached { ", cached" } else { "" }
        ),
        InstallEvent::Building { package, version } => {
            format!("{} Building {} {}", dot, clean(package), clean(version))
        }
        InstallEvent::Built { package, version } => paint(
            style,
            32,
            &format!("{} Built {} {}", ok, clean(package), clean(version)),
        ),
        InstallEvent::HandshakeVerified { package } => paint(
            style,
            32,
            &format!("{} Library handshake verified for {}", ok, clean(package)),
        ),
        InstallEvent::Installed { package, version } => paint(
            style,
            32,
            &format!("{} Installed {} {}", ok, clean(package), clean(version)),
        ),
        InstallEvent::Warning { message } => paint(
            style,
            33,
            &format!("{} Warning: {}", warn, clean(message)),
        ),
        InstallEvent::Summary {
            packages,
            dependencies,
            downloaded_bytes,
            cache_hits,
            elapsed_millis,
        } => paint(
            style,
            32,
            &format!(
                "{} Installed {} package(s) + {} dependenc{} in {} · {} downloaded · {} cache hit{}",
                ok,
                packages,
                dependencies,
                if *dependencies == 1 { "y" } else { "ies" },
                format_duration_millis(*elapsed_millis),
                format_bytes(*downloaded_bytes),
                cache_hits,
                if *cache_hits == 1 { "" } else { "s" }
            ),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_download(
    style: TerminalStyle,
    interactive: bool,
    down: &str,
    package: &str,
    version: &str,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_second: Option<u64>,
    eta_seconds: Option<u64>,
) -> String {
    let mut parts = vec![format!("{} {} {}", down, clean(package), clean(version))];

    if interactive {
        if let Some(total) = total_bytes.filter(|total| *total > 0) {
            parts.push(progress_bar(
                downloaded_bytes,
                total,
                style.progress_width.max(8),
                style.unicode,
            ));
        }
    }

    match total_bytes.filter(|total| *total > 0) {
        Some(total) => {
            let percent = downloaded_bytes
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100);
            parts.push(format!(
                "{}% {}/{}",
                percent,
                format_bytes(downloaded_bytes),
                format_bytes(total)
            ));
        }
        None => parts.push(format_bytes(downloaded_bytes)),
    }

    if let Some(rate) = bytes_per_second.filter(|rate| *rate > 0) {
        parts.push(format!("{}/s", format_bytes(rate)));
    }
    if let Some(eta) = eta_seconds {
        parts.push(format!("ETA {}", format_duration_seconds(eta)));
    }
    parts.join("  ")
}

fn progress_bar(done: u64, total: u64, width: usize, unicode: bool) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((done.min(total) as u128 * width as u128) / total as u128) as usize
    };
    let full = if unicode { '█' } else { '#' };
    let empty = if unicode { '░' } else { '-' };
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    bar.extend(std::iter::repeat_n(full, filled));
    bar.extend(std::iter::repeat_n(empty, width.saturating_sub(filled)));
    bar.push(']');
    bar
}

fn symbol<'a>(style: TerminalStyle, unicode: &'a str, ascii: &'a str) -> &'a str {
    if style.unicode {
        unicode
    } else {
        ascii
    }
}

fn paint(style: TerminalStyle, code: u8, value: &str) -> String {
    if style.ansi {
        format!("\u{1b}[{}m{}\u{1b}[0m", code, value)
    } else {
        value.to_string()
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{} B", bytes)
    }
}

fn format_duration_seconds(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn format_duration_millis(millis: u64) -> String {
    if millis < 1000 {
        format!("{} ms", millis)
    } else {
        format!("{:.2} s", millis as f64 / 1000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedArchive {
    Zip,
    TarGz,
    TarXz,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeArtifact {
    pub host: HostTarget,
    pub version: String,
    pub source: Url,
    pub sha256: String,
    pub archive: ManagedArchive,
    pub entrypoint: String,
}

impl RuntimeArtifact {
    pub fn new(
        host: HostTarget,
        version: impl Into<String>,
        source: impl AsRef<str>,
        sha256: impl Into<String>,
        archive: ManagedArchive,
        entrypoint: impl Into<String>,
    ) -> Result<Self, PackageManagerError> {
        let version = version.into();
        Version::parse(&version).map_err(|source| PackageManagerError::InvalidVersion {
            value: version.clone(),
            source,
        })?;
        let source = match PackageRequest::url(source.as_ref())? {
            PackageRequest::Url(url) => url,
            _ => unreachable!("PackageRequest::url always returns Url"),
        };
        let sha256 = canonical_sha256(sha256.into())?;
        let entrypoint = entrypoint.into();
        validate_relative_entry(&entrypoint)?;
        Ok(Self {
            host,
            version,
            source,
            sha256,
            archive,
            entrypoint,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPythonPlan {
    pub scope: ToolingScope,
    pub version: String,
    pub host: HostTarget,
    pub source: Url,
    pub sha256: String,
    pub archive: ManagedArchive,
    pub install_dir: PathBuf,
    pub executable: PathBuf,
    pub mutate_system_path: bool,
}

impl ManagedPythonPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        requirement: &str,
        scope: ToolingScope,
        host: HostTarget,
        layout: &ProjectLayout,
        user_rbe_root: &Path,
        catalog: &VersionCatalog,
        artifact: RuntimeArtifact,
    ) -> Result<Self, PackageManagerError> {
        let version = catalog.resolve_runtime("python", requirement)?;
        if artifact.version != version {
            return Err(PackageManagerError::RuntimeArtifactVersionMismatch {
                expected: version,
                actual: artifact.version,
            });
        }
        if artifact.host != host {
            return Err(PackageManagerError::RuntimeHostMismatch);
        }
        validate_shared_root(user_rbe_root)?;

        let install_dir = match scope {
            ToolingScope::Project => layout.runtime_dir("python", &artifact.version)?,
            ToolingScope::UserShared => user_rbe_root
                .join("runtimes")
                .join("python")
                .join(&artifact.version)
                .join(host_target_id(&host)),
        };
        let executable = install_dir.join(&artifact.entrypoint);

        Ok(Self {
            scope,
            version: artifact.version,
            host,
            source: artifact.source,
            sha256: artifact.sha256,
            archive: artifact.archive,
            install_dir,
            executable,
            mutate_system_path: false,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythonScriptingPolicy {
    pub allow_project_tool_scripts: bool,
    pub allow_user_shared_scripts: bool,
    pub allow_system_install: bool,
    pub mutate_system_path: bool,
}

impl Default for PythonScriptingPolicy {
    fn default() -> Self {
        Self {
            allow_project_tool_scripts: true,
            allow_user_shared_scripts: false,
            allow_system_install: false,
            mutate_system_path: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkFamily {
    Rust,
    Javascript,
    Python,
}

impl SdkFamily {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Javascript => "javascript",
            Self::Python => "python",
        }
    }

    pub const fn package(self) -> &'static str {
        match self {
            Self::Rust => "rbe-sdk",
            Self::Javascript => "@rbe/sdk",
            Self::Python => "rbe-sdk",
        }
    }

    pub const fn import_hint(self) -> &'static str {
        match self {
            Self::Rust => "use rbe_sdk::...;",
            Self::Javascript => "import { ... } from \"@rbe/sdk\";",
            Self::Python => "from rbe_sdk import ...",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkBootstrapPlan {
    pub scope: ToolingScope,
    pub family: SdkFamily,
    pub package: String,
    pub version: String,
    pub install_dir: PathBuf,
    pub registry_files: Vec<ConfigArtifact>,
    pub dependency_hint: String,
    pub import_hint: String,
}

impl SdkBootstrapPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        family: SdkFamily,
        requirement: &str,
        scope: ToolingScope,
        layout: &ProjectLayout,
        user_rbe_root: &Path,
        catalog: &VersionCatalog,
        endpoints: &RegistryEndpoints,
    ) -> Result<Self, PackageManagerError> {
        validate_shared_root(user_rbe_root)?;
        let version = catalog.resolve_sdk(family.package(), requirement)?;
        let install_dir = match scope {
            ToolingScope::Project => layout.sdk_dir(family.name(), &version)?,
            ToolingScope::UserShared => {
                user_rbe_root.join("sdk").join(family.name()).join(&version)
            }
        };
        let dependency_hint = match family {
            SdkFamily::Rust => format!(
                "rbe-sdk = {{ version = \"={}\", registry = \"rbe\" }}",
                version
            ),
            SdkFamily::Javascript => format!("\"@rbe/sdk\": \"{}\"", version),
            SdkFamily::Python => format!("rbe-sdk=={}", version),
        };

        Ok(Self {
            scope,
            family,
            package: family.package().to_string(),
            version,
            install_dir,
            registry_files: SdkRegistryPlan::new(endpoints).files,
            dependency_hint,
            import_hint: family.import_hint().to_string(),
        })
    }

    pub fn local_command(&self) -> String {
        let scope = if self.scope == ToolingScope::UserShared {
            " -shared"
        } else {
            ""
        };
        format!("backend install sdk.{}{}", self.version, scope)
    }
}

fn host_target_id(host: &HostTarget) -> String {
    let os = match host.os {
        HostOs::Windows => "windows",
        HostOs::Linux => "linux",
        HostOs::Macos => "macos",
        HostOs::Other => "other",
    };
    format!("{}-{}", os, host.arch)
}

fn canonical_sha256(value: String) -> Result<String, PackageManagerError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PackageManagerError::InvalidSha256(value));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_relative_entry(value: &str) -> Result<(), PackageManagerError> {
    if value.is_empty() || value.contains('\\') || value.contains(':') {
        return Err(PackageManagerError::InvalidEntrypoint(value.to_string()));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(PackageManagerError::InvalidEntrypoint(value.to_string()));
    }
    Ok(())
}

fn validate_shared_root(value: &Path) -> Result<(), PackageManagerError> {
    if value.as_os_str().is_empty() {
        return Err(PackageManagerError::InvalidUserRbeRoot);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PackageManagerError {
    #[error(transparent)]
    Installer(#[from] InstallerError),
    #[error(transparent)]
    Lock(#[from] rbe_library_lock::LockError),
    #[error("invalid exact version {value:?}: {source}")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid SHA-256 {0:?}")]
    InvalidSha256(String),
    #[error("invalid managed runtime entrypoint {0:?}")]
    InvalidEntrypoint(String),
    #[error("managed runtime artifact version mismatch: expected {expected}, got {actual}")]
    RuntimeArtifactVersionMismatch { expected: String, actual: String },
    #[error("managed runtime artifact does not match the requested host")]
    RuntimeHostMismatch,
    #[error("user RBE root must not be empty")]
    InvalidUserRbeRoot,
    #[error("could not render JSON event: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostTarget {
        HostTarget::new(HostOs::Windows, "x86_64").unwrap()
    }

    fn endpoints() -> RegistryEndpoints {
        RegistryEndpoints::new(
            "https://registry.rbe.invalid/index/",
            "https://registry.rbe.invalid/cargo/",
            "https://registry.rbe.invalid/npm/",
            "https://registry.rbe.invalid/python/",
        )
        .unwrap()
    }

    #[test]
    fn progress_is_truthful_when_total_size_is_known() {
        let line = render_event(
            &InstallEvent::Downloading {
                package: "advancenet".into(),
                version: "2.4.1".into(),
                downloaded_bytes: 5 * 1024 * 1024,
                total_bytes: Some(10 * 1024 * 1024),
                bytes_per_second: Some(2 * 1024 * 1024),
                eta_seconds: Some(3),
            },
            OutputMode::Interactive,
            TerminalStyle::default(),
        )
        .unwrap();
        assert!(line.contains("50%"));
        assert!(line.contains("5.0 MiB/10.0 MiB"));
        assert!(line.contains("2.0 MiB/s"));
        assert!(line.contains("ETA 0:03"));
    }

    #[test]
    fn unknown_download_size_does_not_fake_a_percentage() {
        let line = render_event(
            &InstallEvent::Downloading {
                package: "advancenet".into(),
                version: "2.4.1".into(),
                downloaded_bytes: 4096,
                total_bytes: None,
                bytes_per_second: None,
                eta_seconds: None,
            },
            OutputMode::Interactive,
            TerminalStyle::default(),
        )
        .unwrap();
        assert!(!line.contains('%'));
        assert!(line.contains("4.0 KiB"));
    }

    #[test]
    fn json_lines_mode_is_machine_readable() {
        let line = render_event(
            &InstallEvent::Resolved { packages: 4 },
            OutputMode::JsonLines,
            TerminalStyle::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "resolved");
        assert_eq!(value["packages"], 4);
    }

    #[test]
    fn managed_python_stays_project_local_and_never_mutates_path() {
        let mut catalog = VersionCatalog::default();
        catalog.add_runtime("python", "3.13.7").unwrap();
        let layout = ProjectLayout::new("/work/project");
        let artifact = RuntimeArtifact::new(
            host(),
            "3.13.7",
            "https://runtime.rbe.invalid/python-3.13.7.zip",
            "a".repeat(64),
            ManagedArchive::Zip,
            "python.exe",
        )
        .unwrap();
        let plan = ManagedPythonPlan::resolve(
            ">=3.13,<3.14",
            ToolingScope::Project,
            host(),
            &layout,
            Path::new("/users/kate/.rbe"),
            &catalog,
            artifact,
        )
        .unwrap();
        assert_eq!(
            plan.install_dir,
            PathBuf::from("/work/project/.rbe/runtimes/python/3.13.7")
        );
        assert!(!plan.mutate_system_path);
    }

    #[test]
    fn shared_python_is_host_scoped() {
        let mut catalog = VersionCatalog::default();
        catalog.add_runtime("python", "3.13.7").unwrap();
        let layout = ProjectLayout::new("/work/project");
        let artifact = RuntimeArtifact::new(
            host(),
            "3.13.7",
            "https://runtime.rbe.invalid/python-3.13.7.zip",
            "b".repeat(64),
            ManagedArchive::Zip,
            "python.exe",
        )
        .unwrap();
        let plan = ManagedPythonPlan::resolve(
            "=3.13.7",
            ToolingScope::UserShared,
            host(),
            &layout,
            Path::new("/users/kate/.rbe"),
            &catalog,
            artifact,
        )
        .unwrap();
        assert_eq!(
            plan.install_dir,
            PathBuf::from("/users/kate/.rbe/runtimes/python/3.13.7/windows-x86_64")
        );
    }

    #[test]
    fn scripting_policy_keeps_global_python_off_by_default() {
        let policy = PythonScriptingPolicy::default();
        assert!(policy.allow_project_tool_scripts);
        assert!(!policy.allow_user_shared_scripts);
        assert!(!policy.allow_system_install);
        assert!(!policy.mutate_system_path);
    }

    #[test]
    fn sdk_bootstrap_resolves_exact_local_sdk_and_wiring() {
        let mut catalog = VersionCatalog::default();
        catalog.add_sdk("rbe-sdk", "0.1.0").unwrap();
        let plan = SdkBootstrapPlan::resolve(
            SdkFamily::Rust,
            "^0.1",
            ToolingScope::Project,
            &ProjectLayout::new("/work/project"),
            Path::new("/users/kate/.rbe"),
            &catalog,
            &endpoints(),
        )
        .unwrap();
        assert_eq!(plan.version, "0.1.0");
        assert_eq!(
            plan.install_dir,
            PathBuf::from("/work/project/.rbe/sdk/rust/0.1.0")
        );
        assert!(plan.dependency_hint.contains("registry = \"rbe\""));
        assert_eq!(plan.local_command(), "backend install sdk.0.1.0");
        assert_eq!(plan.registry_files.len(), 3);
    }

    #[test]
    fn remote_runtime_artifacts_require_https() {
        let error = RuntimeArtifact::new(
            host(),
            "3.13.7",
            "http://example.com/python.zip",
            "c".repeat(64),
            ManagedArchive::Zip,
            "python.exe",
        )
        .unwrap_err();
        assert!(matches!(error, PackageManagerError::Installer(_)));
    }
}
