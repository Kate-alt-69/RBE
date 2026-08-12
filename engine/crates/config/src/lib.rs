//! Typed, validated loader for `settings.json`.
//!
//! Implements migration-plan §4.1 and the "hand-roll on serde" default
//! from §2.1: this is a deliberately simple layered loader (defaults ->
//! file -> env -> nothing-else-yet), not a `figment` adoption. Revisit
//! that choice if the merge/precedence logic here grows past what's
//! comfortable to hand-maintain.
//!
//! Precedence, matching the handbook's loading sequence:
//! defaults (serde `#[serde(default)]`) -> settings.json -> environment
//! variable overrides -> (runtime overrides / hot reload: not yet wired,
//! see `crates/supervisor` for where that would plug in later).
//!
//! `#[serde(deny_unknown_fields)]` on every struct here is deliberate:
//! a typo'd config key should fail startup loudly, not be silently
//! ignored — matches the handbook's "Configuration Validation" step.

use std::path::Path;

use serde::Deserialize;

/// Top-level settings, one field per namespace — mirrors the handbook's
/// Configuration Namespaces (`runtime/`, `security/`, `uac/`, `storage/`,
/// `streaming/`, `containers/`, `bootstrap/`, `logging/`, `dashboards/`).
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
    pub containers: ContainersConfig,
    #[serde(default)]
    pub bootstrap: BootstrapConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub dashboards: DashboardsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub environment: String,
    /// 0 = auto-detect (num CPUs) — see migration-plan §9.1.
    pub worker_threads: usize,
    pub graceful_shutdown_timeout_ms: u64,
    /// Before binding, check whether `api.port` is already held by a
    /// *previous crashed run of this same binary* and kill it if so —
    /// this is NOT obsolete the way the migration plan originally
    /// assumed (see backend/src/port_guard.rs's doc comment for the
    /// correction). Scoped to only kill processes whose image name
    /// matches ours, never an arbitrary process holding the port.
    pub reclaim_port: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            environment: "development".into(),
            worker_threads: 0,
            graceful_shutdown_timeout_ms: 10_000,
            reclaim_port: true,
        }
    }
}

/// No `Default` here on purpose — the API port is the one thing every
/// deployment must actually decide, so it's a required field rather than
/// silently defaulting to something that might not be what's wanted.
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
    /// Only appended to `cors_allowed_origins` when
    /// `runtime.environment != "production"` — mirrors the Node
    /// backend's `debugCorsOrigins` behavior (local dev ports allowed
    /// only outside prod), applied in `security::build_cors_layer`.
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
        // Global tier default: matches the Node backend's
        // express-rate-limit baseline (90 requests / 30s).
        Self {
            window_secs: 30,
            max_requests: 90,
        }
    }
}

impl Default for IpBanConfig {
    fn default() -> Self {
        // Matches the Node backend's ADMIN_AUTH_STRIKE_* defaults
        // (5 strikes / 15 min window / 1 hour ban) — reused here as
        // the general-purpose default. v1 tracks strikes in one
        // tracker with a `category` tag (api/admin/cpu/logic) but
        // applies this single threshold uniformly across categories;
        // per-category tuning (and real CPU/logic-abuse cost analysis,
        // not just counting) is a real gap, not implemented yet — see
        // `security::ip_strikes`.
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
                "http://localhost:8080".to_string(),
                "http://127.0.0.1:3000".to_string(),
            ],
            global_rate_limit: RateLimitTierConfig::default(),
            api_rate_limit: RateLimitTierConfig {
                // API tier is tighter than the global net, matching the
                // Node backend's split (global catches spam broadly,
                // the per-route API guard is the precise one).
                window_secs: 60,
                max_requests: 120,
            },
            ip_ban: IpBanConfig::default(),
            max_json_payload_bytes: 1024 * 1024, // 1MB, matches express.json({limit:'1mb'})
            csp_policy: "default-src 'self'; script-src 'self' 'unsafe-inline'".to_string(),
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
    /// "sqlite" | "postgres" — validated in `Config::validate`.
    pub driver: String,
    pub sqlite_path: String,
    /// Deliberately absent from settings.json in normal operation — real
    /// connection strings belong in the vault (migration-plan §8), not
    /// this file. Wired here only so `validate()` can check it resolves
    /// from *somewhere* (env, for now) when `driver = "postgres"`.
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
pub struct ContainersConfig {
    pub warm_pool_size: u32,
    pub max_concurrent: u32,
    pub default_timeout_ms: u64,
    pub memory_limit_mb: u64,
}

impl Default for ContainersConfig {
    fn default() -> Self {
        Self {
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
    /// trace | debug | info | warn | error
    pub level: String,
    /// "json" | "pretty"
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
    pub admin_path_prefix: String,
}

impl Default for DashboardsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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
    /// Load `settings.json` from `path`, apply the small set of
    /// documented env overrides, then validate. Fails loudly on the
    /// first problem — matches the handbook's "fail fast before any
    /// service starts" boot-sequence principle (see §3.2 in the plan).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.display().to_string();

        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;

        let mut config: Config =
            serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
                path: path_str.clone(),
                source,
            })?;

        config.apply_env_overrides();
        config.validate()?;

        Ok(config)
    }

    /// Env overrides are intentionally a short, explicit list — not a
    /// generic "any config key can come from ANY_ENV_VAR" mechanism.
    /// Add to this as real deployment needs come up, per the handbook's
    /// "Environment Variables provide deployment-specific configuration"
    /// principle (not secrets — those go through the vault, §8).
    fn apply_env_overrides(&mut self) {
        if let Ok(port) = std::env::var("API_PORT") {
            match port.parse::<u16>() {
                Ok(p) => self.api.port = p,
                Err(_) => {
                    tracing::warn!(value = %port, "API_PORT env var is not a valid u16, ignoring");
                }
            }
        }

        if let Ok(host) = std::env::var("API_HOST") {
            self.api.host = host;
        }

        if let Ok(env) = std::env::var("RUNTIME_ENVIRONMENT") {
            self.runtime.environment = env;
        }
    }

    /// Structural validation — matches the handbook's Configuration
    /// Validation step. Keep growing this as more of the config gets
    /// load-bearing (e.g. once the vault, in Phase 1, needs its own
    /// checks here for whether a real Postgres URL resolves).
    fn validate(&self) -> Result<(), ConfigError> {
        match self.storage.driver.as_str() {
            "sqlite" | "postgres" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "storage.driver must be \"sqlite\" or \"postgres\", got {other:?}"
                )))
            }
        }

        if self.storage.driver == "postgres" && std::env::var("DATABASE_URL").is_err() {
            return Err(ConfigError::Invalid(
                "storage.driver is \"postgres\" but DATABASE_URL is not set (real connection \
                 strings belong in env/vault, not settings.json)"
                    .into(),
            ));
        }

        match self.logging.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "logging.level must be one of trace|debug|info|warn|error, got {other:?}"
                )))
            }
        }

        if self.api.port == 0 {
            return Err(ConfigError::Invalid(
                "api.port must be a nonzero port".into(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_top_level_field() {
        let bad = r#"{ "api": { "host": "0.0.0.0", "port": 8080 }, "typo_field": true }"#;
        let result: Result<Config, _> = serde_json::from_str(bad);
        assert!(result.is_err(), "unknown field should fail to deserialize");
    }

    #[test]
    fn rejects_invalid_storage_driver() {
        let raw = r#"{
            "api": { "host": "0.0.0.0", "port": 8080 },
            "storage": { "driver": "mongodb" }
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn minimal_valid_config_loads() {
        let raw = r#"{ "api": { "host": "0.0.0.0", "port": 8080 } }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.api.port, 8080);
        assert_eq!(config.storage.driver, "sqlite"); // default applied
    }
}
