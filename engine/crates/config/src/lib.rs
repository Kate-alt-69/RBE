//! Typed, validated loader for `settings.json` plus root Server REL policy.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub runtime: RuntimeConfig,
    pub api: ApiConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub uac: UacConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub services: ServicesConfig,
    #[serde(default)]
    pub video_manager: VideoManagerConfig,
    #[serde(default)]
    pub containers: ContainersConfig,
    #[serde(default)]
    pub bootstrap: BootstrapConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub dashboards: DashboardsConfig,
}

/// Result of the deterministic configuration bootstrap pass. Callers that only
/// need the typed settings can continue to use [`Config::load`]. RELC/runtime
/// boot can retain `server_policy` for ENV, middleware and embedded-source work.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub server_policy: Option<server_rel::ServerPolicy>,
    pub server_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoCount {
    #[default]
    Auto,
    Fixed(usize),
}

impl AutoCount {
    pub fn resolve(self, automatic: usize) -> usize {
        match self {
            Self::Auto => automatic.max(1),
            Self::Fixed(value) => value.max(1),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::Fixed(value) => value.to_string(),
        }
    }

    pub fn fixed(self) -> Option<usize> {
        match self {
            Self::Auto => None,
            Self::Fixed(value) => Some(value),
        }
    }
}

impl<'de> Deserialize<'de> for AutoCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AutoCountVisitor;
        impl<'de> Visitor<'de> for AutoCountVisitor {
            type Value = AutoCount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("\"auto\", null, or a positive integer")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                usize::try_from(value)
                    .map(AutoCount::Fixed)
                    .map_err(|_| E::custom("count does not fit usize"))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value < 0 {
                    return Err(E::custom("count must not be negative"));
                }
                self.visit_u64(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.trim().eq_ignore_ascii_case("auto") || value.trim().is_empty() {
                    Ok(AutoCount::Auto)
                } else {
                    value
                        .trim()
                        .parse::<usize>()
                        .map(AutoCount::Fixed)
                        .map_err(E::custom)
                }
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(AutoCount::Auto)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(AutoCount::Auto)
            }
        }
        deserializer.deserialize_any(AutoCountVisitor)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub environment: String,
    pub worker_threads: usize,
    pub graceful_shutdown_timeout_ms: u64,
    pub reclaim_port: bool,
    pub process_refresh_hours: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            environment: "development".into(),
            worker_threads: 0,
            graceful_shutdown_timeout_ms: 10_000,
            reclaim_port: true,
            process_refresh_hours: 500,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_max_body_size_bytes")]
    pub max_body_size_bytes: usize,
}

fn default_request_timeout_ms() -> u64 {
    30_000
}

fn default_max_body_size_bytes() -> usize {
    10 * 1024 * 1024
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct SecurityConfig {
    pub cors_allowed_origins: Vec<String>,
    pub debug_cors_origins: Vec<String>,
    pub global_rate_limit: RateLimitTierConfig,
    pub api_rate_limit: RateLimitTierConfig,
    pub ip_ban: IpBanConfig,
    pub max_json_payload_bytes: usize,
    pub csp_policy: String,
    pub trusted_proxy_headers: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct RateLimitTierConfig {
    pub window_secs: u64,
    pub max_requests: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct IpBanConfig {
    pub strike_threshold: u32,
    pub strike_window_secs: u64,
    pub ban_duration_secs: u64,
}

impl Default for RateLimitTierConfig {
    fn default() -> Self {
        Self {
            window_secs: 30,
            max_requests: 90,
        }
    }
}

impl Default for IpBanConfig {
    fn default() -> Self {
        Self {
            strike_threshold: 5,
            strike_window_secs: 900,
            ban_duration_secs: 3600,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            cors_allowed_origins: Vec::new(),
            debug_cors_origins: vec![
                "http://localhost:8080".into(),
                "http://127.0.0.1:3000".into(),
            ],
            global_rate_limit: RateLimitTierConfig::default(),
            api_rate_limit: RateLimitTierConfig {
                window_secs: 60,
                max_requests: 120,
            },
            ip_ban: IpBanConfig::default(),
            max_json_payload_bytes: 1024 * 1024,
            csp_policy: "default-src 'self'; script-src 'self' 'unsafe-inline'".into(),
            trusted_proxy_headers: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct UacConfig {
    pub session_ttl_minutes: u32,
    pub require_email_verification: bool,
    pub oauth_providers: Vec<String>,
}

impl Default for UacConfig {
    fn default() -> Self {
        Self {
            session_ttl_minutes: 1440,
            require_email_verification: true,
            oauth_providers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct StorageConfig {
    pub driver: String,
    pub sqlite_path: String,
    pub supabase_sync_enabled: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            driver: "sqlite".into(),
            sqlite_path: "./data/app.db".into(),
            supabase_sync_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct StreamingConfig {
    pub rtmp_port: u16,
    pub whip_enabled: bool,
    pub vod_cache_dir: String,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            rtmp_port: 1935,
            whip_enabled: false,
            vod_cache_dir: "./cache/vod".into(),
        }
    }
}

/// Runtime policy for user-authored `.service` files. Paths are resolved
/// relative to the backend binary rather than the launching shell's CWD.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ServicesConfig {
    pub enabled: bool,
    pub directory: String,
    pub default_memory_limit_mb: u64,
    pub startup_timeout_ms: u64,
    pub default_idle_timeout_ms: u64,
    pub monitor_interval_ms: u64,
    pub max_restart_backoff_ms: u64,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "service".into(),
            default_memory_limit_mb: 256,
            startup_timeout_ms: 10_000,
            default_idle_timeout_ms: 300_000,
            monitor_interval_ms: 1_000,
            max_restart_backoff_ms: 30_000,
        }
    }
}

/// Lightweight Video Manager control-plane settings. Heavy FFmpeg/live workers
/// are intentionally lazy and use `live_idle_secs` before being torn down.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct VideoManagerConfig {
    pub enabled: bool,
    pub data_dir: String,
    pub default_database: String,
    pub live_idle_secs: u64,
    pub download_max_bytes: u64,
    pub download_worker_enabled: bool,
    pub ffprobe_executable: String,
    pub ffmpeg_executable: String,
    pub worker_recovery_scan_secs: u64,
}

impl Default for VideoManagerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: "data/video".into(),
            default_database: "default".into(),
            live_idle_secs: 2 * 60 * 60,
            download_max_bytes: 8 * 1024 * 1024 * 1024,
            download_worker_enabled: false,
            ffprobe_executable: String::new(),
            ffmpeg_executable: String::new(),
            worker_recovery_scan_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ContainersConfig {
    pub environments: usize,
    pub swamps_per_environment: AutoCount,
    pub workers_per_swamp: AutoCount,
    pub warm_pool_size: u32,
    pub max_concurrent: u32,
    pub default_timeout_ms: u64,
    pub memory_limit_mb: u64,
}

impl Default for ContainersConfig {
    fn default() -> Self {
        Self {
            environments: 5,
            swamps_per_environment: AutoCount::Auto,
            workers_per_swamp: AutoCount::Auto,
            warm_pool_size: 4,
            max_concurrent: 16,
            default_timeout_ms: 5_000,
            memory_limit_mb: 256,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct BootstrapConfig {
    pub services: Vec<String>,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            services: vec!["email".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "json".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct DashboardsConfig {
    pub enabled: bool,
    pub auto_open: bool,
    pub admin_path_prefix: String,
}

impl Default for DashboardsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_open: true,
            admin_path_prefix: "/admin".into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to compile Server REL {path}: {source}")]
    ServerRel {
        path: String,
        #[source]
        source: server_rel::ServerRelError,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Load the effective configuration. If a root `server.server` exists next
    /// to `settings.json` (or `SERVER_REL_PATH` points at one), its defaults and
    /// forced settings are applied automatically.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(Self::load_bundle(path)?.config)
    }

    /// Load effective typed configuration and retain the compiled Server REL
    /// policy for later RELC/runtime stages.
    pub fn load_bundle(path: impl AsRef<Path>) -> Result<LoadedConfig, ConfigError> {
        let path_ref = path.as_ref();
        let server_path = discover_server_rel_path(path_ref)?;
        let Some(server_path) = server_path else {
            return Ok(LoadedConfig {
                config: Self::load_settings_only(path_ref)?,
                server_policy: None,
                server_path: None,
            });
        };

        let source = std::fs::read_to_string(&server_path).map_err(|source| ConfigError::Read {
            path: server_path.display().to_string(),
            source,
        })?;
        let policy = server_rel::compile_server_source(&source).map_err(|source| {
            ConfigError::ServerRel {
                path: server_path.display().to_string(),
                source,
            }
        })?;
        let config = Self::load_with_overlays(path_ref, &policy.defaults, &policy.forced)?;
        Ok(LoadedConfig {
            config,
            server_policy: Some(policy),
            server_path: Some(server_path),
        })
    }

    fn load_settings_only(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.display().to_string();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        let mut config: Config =
            serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
                path: path_str,
                source,
            })?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    /// Load `settings.json` with deterministic Server REL policy overlays.
    ///
    /// Precedence within this method is:
    /// built-in serde defaults < Server REL defaults < settings.json < operator
    /// environment overrides < Server REL forced values. Hard engine invariants
    /// are still enforced by `validate` after the final merge.
    pub fn load_with_overlays(
        path: impl AsRef<Path>,
        defaults: &BTreeMap<String, JsonValue>,
        forced: &BTreeMap<String, JsonValue>,
    ) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.display().to_string();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        let mut value: JsonValue =
            serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
                path: path_str.clone(),
                source,
            })?;

        for (key, default) in defaults {
            apply_json_path(&mut value, key, default.clone(), false)?;
        }
        apply_env_overrides_to_json(&mut value)?;
        for (key, forced_value) in forced {
            apply_json_path(&mut value, key, forced_value.clone(), true)?;
        }

        let config: Config =
            serde_json::from_value(value).map_err(|source| ConfigError::Parse {
                path: path_str,
                source,
            })?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(port) = std::env::var("API_PORT") {
            match port.parse::<u16>() {
                Ok(port) => self.api.port = port,
                Err(_) => {
                    tracing::warn!(value = %port, "API_PORT env var is not a valid u16, ignoring")
                }
            }
        }
        if let Ok(host) = std::env::var("API_HOST") {
            self.api.host = host;
        }
        if let Ok(environment) = std::env::var("RUNTIME_ENVIRONMENT") {
            self.runtime.environment = environment;
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(self.storage.driver.as_str(), "sqlite" | "postgres") {
            return Err(ConfigError::Invalid(format!(
                "storage.driver must be \"sqlite\" or \"postgres\", got {:?}",
                self.storage.driver
            )));
        }
        if self.storage.driver == "postgres" && std::env::var("DATABASE_URL").is_err() {
            return Err(ConfigError::Invalid(
                "storage.driver is \"postgres\" but DATABASE_URL is not set".into(),
            ));
        }
        if !matches!(
            self.logging.level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            return Err(ConfigError::Invalid(format!(
                "invalid logging.level {:?}",
                self.logging.level
            )));
        }
        if self.api.port == 0 {
            return Err(ConfigError::Invalid(
                "api.port must be a nonzero port".into(),
            ));
        }
        if self.runtime.process_refresh_hours == 0 {
            return Err(ConfigError::Invalid(
                "runtime.processRefreshHours must be greater than zero".into(),
            ));
        }
        if !(1..=5).contains(&self.containers.environments) {
            return Err(ConfigError::Invalid(
                "containers.environments must be between 1 and 5 general environments; payment is always separate".into(),
            ));
        }
        for (name, value) in [
            (
                "swampsPerEnvironment",
                self.containers.swamps_per_environment,
            ),
            ("workersPerSwamp", self.containers.workers_per_swamp),
        ] {
            if let Some(value) = value.fixed() {
                if value == 0 || value > 4096 {
                    return Err(ConfigError::Invalid(format!(
                        "containers.{name} must be auto or 1..=4096"
                    )));
                }
            }
        }
        if self.services.enabled {
            if self.services.directory.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "services.directory must not be empty".into(),
                ));
            }
            if self.services.startup_timeout_ms == 0
                || self.services.default_idle_timeout_ms == 0
                || self.services.monitor_interval_ms == 0
            {
                return Err(ConfigError::Invalid(
                    "service startup/default idle/monitor intervals must be greater than zero"
                        .into(),
                ));
            }
        }
        if self.video_manager.enabled {
            if self.video_manager.data_dir.trim().is_empty()
                || self.video_manager.default_database.trim().is_empty()
            {
                return Err(ConfigError::Invalid(
                    "videoManager dataDir/defaultDatabase must not be empty".into(),
                ));
            }
            if self.video_manager.live_idle_secs == 0 {
                return Err(ConfigError::Invalid(
                    "videoManager.liveIdleSecs must be greater than zero".into(),
                ));
            }
            if self.video_manager.download_max_bytes == 0 {
                return Err(ConfigError::Invalid(
                    "videoManager.downloadMaxBytes must be greater than zero".into(),
                ));
            }
            if self.video_manager.worker_recovery_scan_secs == 0
                || self.video_manager.worker_recovery_scan_secs > 3600
            {
                return Err(ConfigError::Invalid(
                    "videoManager.workerRecoveryScanSecs must be between 1 and 3600".into(),
                ));
            }
            if self.video_manager.download_worker_enabled
                && (self.video_manager.ffprobe_executable.trim().is_empty()
                    || self.video_manager.ffmpeg_executable.trim().is_empty())
            {
                return Err(ConfigError::Invalid(
                    "videoManager download worker requires ffprobeExecutable and ffmpegExecutable"
                        .into(),
                ));
            }
        }
        if !self.dashboards.admin_path_prefix.starts_with('/') {
            return Err(ConfigError::Invalid(
                "dashboards.adminPathPrefix must start with '/'".into(),
            ));
        }
        Ok(())
    }
}

fn discover_server_rel_path(settings_path: &Path) -> Result<Option<PathBuf>, ConfigError> {
    if let Ok(explicit) = std::env::var("SERVER_REL_PATH") {
        if explicit.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "SERVER_REL_PATH is set but empty".into(),
            ));
        }
        let path = PathBuf::from(explicit);
        let metadata = std::fs::metadata(&path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(ConfigError::Invalid(format!(
                "SERVER_REL_PATH must point to a file, got {}",
                path.display()
            )));
        }
        return Ok(Some(path));
    }

    let parent = settings_path.parent().unwrap_or_else(|| Path::new("."));
    let candidate = parent.join("server.server");
    match std::fs::metadata(&candidate) {
        Ok(metadata) if metadata.is_file() => Ok(Some(candidate)),
        Ok(_) => Err(ConfigError::Invalid(format!(
            "root Server REL path exists but is not a file: {}",
            candidate.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: candidate.display().to_string(),
            source,
        }),
    }
}

fn apply_env_overrides_to_json(value: &mut JsonValue) -> Result<(), ConfigError> {
    if let Ok(port) = std::env::var("API_PORT") {
        match port.parse::<u16>() {
            Ok(port) => apply_json_path(value, "api.port", JsonValue::from(port), true)?,
            Err(_) => {
                tracing::warn!(value = %port, "API_PORT env var is not a valid u16, ignoring")
            }
        }
    }
    if let Ok(host) = std::env::var("API_HOST") {
        apply_json_path(value, "api.host", JsonValue::String(host), true)?;
    }
    if let Ok(environment) = std::env::var("RUNTIME_ENVIRONMENT") {
        apply_json_path(
            value,
            "runtime.environment",
            JsonValue::String(environment),
            true,
        )?;
    }
    Ok(())
}

fn apply_json_path(
    root: &mut JsonValue,
    path: &str,
    value: JsonValue,
    overwrite: bool,
) -> Result<(), ConfigError> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return Err(ConfigError::Invalid(format!(
            "Server REL config path {path:?} is invalid"
        )));
    }

    let mut current = root.as_object_mut().ok_or_else(|| {
        ConfigError::Invalid("settings.json root must be a JSON object".into())
    })?;
    for segment in &segments[..segments.len() - 1] {
        if !current.contains_key(*segment) {
            current.insert((*segment).to_string(), JsonValue::Object(Default::default()));
        }
        let next = current.get_mut(*segment).expect("inserted or existing key");
        current = next.as_object_mut().ok_or_else(|| {
            ConfigError::Invalid(format!(
                "Server REL config path {path:?} crosses non-object key {segment:?}"
            ))
        })?;
    }

    let leaf = segments[segments.len() - 1];
    if overwrite || !current.contains_key(leaf) {
        current.insert(leaf.to_string(), value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("rbe-config-{name}-{nonce}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn minimal_valid_config_loads() {
        let config: Config =
            serde_json::from_str(r#"{ "api": { "host": "0.0.0.0", "port": 8080 } }"#).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.containers.environments, 5);
        assert_eq!(config.containers.swamps_per_environment, AutoCount::Auto);
        assert!(config.services.enabled);
        assert_eq!(config.services.default_idle_timeout_ms, 300_000);
        assert_eq!(config.video_manager.live_idle_secs, 7200);
        assert!(!config.video_manager.download_worker_enabled);
        assert_eq!(config.video_manager.worker_recovery_scan_secs, 30);
    }

    #[test]
    fn auto_count_accepts_auto_null_and_number() {
        assert_eq!(
            serde_json::from_str::<AutoCount>("\"auto\"").unwrap(),
            AutoCount::Auto
        );
        assert_eq!(
            serde_json::from_str::<AutoCount>("null").unwrap(),
            AutoCount::Auto
        );
        assert_eq!(
            serde_json::from_str::<AutoCount>("4").unwrap(),
            AutoCount::Fixed(4)
        );
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let result: Result<Config, _> = serde_json::from_str(
            r#"{ "api": { "host": "0.0.0.0", "port": 8080 }, "typo_field": true }"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn video_worker_requires_explicit_trusted_executables() {
        let config: Config = serde_json::from_str(
            r#"{
                "api": { "host": "0.0.0.0", "port": 8080 },
                "videoManager": { "downloadWorkerEnabled": true }
            }"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("ffprobeExecutable"));
        assert!(error.contains("ffmpegExecutable"));
    }

    #[test]
    fn server_defaults_only_fill_missing_settings_and_force_wins() {
        let mut value = serde_json::json!({
            "api": { "host": "0.0.0.0", "port": 8080 }
        });
        apply_json_path(&mut value, "api.port", JsonValue::from(9000), false).unwrap();
        apply_json_path(
            &mut value,
            "services.monitorIntervalMs",
            JsonValue::from(2500),
            false,
        )
        .unwrap();
        apply_json_path(&mut value, "api.port", JsonValue::from(9090), true).unwrap();

        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.api.port, 9090);
        assert_eq!(config.services.monitor_interval_ms, 2500);
    }

    #[test]
    fn overlay_rejects_paths_through_scalar_values() {
        let mut value = serde_json::json!({ "api": "broken" });
        let error = apply_json_path(&mut value, "api.port", JsonValue::from(8080), true)
            .expect_err("scalar parent must fail");
        assert!(error.to_string().contains("crosses non-object"));
    }

    #[test]
    fn load_bundle_discovers_server_rel_and_applies_precedence() {
        if std::env::var_os("SERVER_REL_PATH").is_some() {
            return;
        }
        let directory = temp_dir("server-rel");
        let settings = directory.join("settings.json");
        let server = directory.join("server.server");
        std::fs::write(
            &settings,
            r#"{ "api": { "host": "127.0.0.1", "port": 8080 } }"#,
        )
        .unwrap();
        std::fs::write(
            &server,
            r#"
            server Main {
                api { port 7000; }
                services { monitorIntervalMs 2500; }
                force { api { port 9090; } }
            }
            "#,
        )
        .unwrap();

        let loaded = Config::load_bundle(&settings).expect("Server REL config should load");
        assert_eq!(loaded.config.api.host, "127.0.0.1");
        assert_eq!(loaded.config.api.port, 9090);
        assert_eq!(loaded.config.services.monitor_interval_ms, 2500);
        assert_eq!(loaded.server_path.as_deref(), Some(server.as_path()));
        assert_eq!(loaded.server_policy.as_ref().unwrap().name, "Main");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn forced_server_policy_still_passes_hard_config_validation() {
        let directory = temp_dir("invalid-force");
        let settings = directory.join("settings.json");
        std::fs::write(
            &settings,
            r#"{ "api": { "host": "127.0.0.1", "port": 8080 } }"#,
        )
        .unwrap();

        let mut forced = BTreeMap::new();
        forced.insert("api.port".to_string(), JsonValue::from(0));
        let error = Config::load_with_overlays(&settings, &BTreeMap::new(), &forced)
            .expect_err("hard validation must reject a forced zero port");
        assert!(error.to_string().contains("nonzero port"));

        let _ = std::fs::remove_dir_all(directory);
    }
}
