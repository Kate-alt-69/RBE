//! Typed, validated loader for `settings.json`.

use std::fmt;
use std::path::Path;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

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
}

impl Default for VideoManagerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: "data/video".into(),
            default_database: "default".into(),
            live_idle_secs: 2 * 60 * 60,
            download_max_bytes: 8 * 1024 * 1024 * 1024,
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
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.display().to_string();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        let mut config: Config = serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path_str,
            source,
        })?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(port) = std::env::var("API_PORT") {
            match port.parse::<u16>() {
                Ok(port) => self.api.port = port,
                Err(_) => tracing::warn!(value = %port, "API_PORT env var is not a valid u16, ignoring"),
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
            ("swampsPerEnvironment", self.containers.swamps_per_environment),
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
            if self.services.startup_timeout_ms == 0 || self.services.monitor_interval_ms == 0 {
                return Err(ConfigError::Invalid(
                    "service startup/monitor intervals must be greater than zero".into(),
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
        }
        if !self.dashboards.admin_path_prefix.starts_with('/') {
            return Err(ConfigError::Invalid(
                "dashboards.adminPathPrefix must start with '/'".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_valid_config_loads() {
        let config: Config = serde_json::from_str(
            r#"{ "api": { "host": "0.0.0.0", "port": 8080 } }"#,
        )
        .unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.containers.environments, 5);
        assert_eq!(config.containers.swamps_per_environment, AutoCount::Auto);
        assert!(config.services.enabled);
        assert_eq!(config.video_manager.live_idle_secs, 7200);
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
}
