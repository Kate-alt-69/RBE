use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE_NAME: &str = "setting.node.cn.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NodeMode {
    #[default]
    Primary,
    Replica,
    Relay,
    Archive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "amazon-s3", alias = "aws-s3", alias = "s3")]
    AmazonS3,
    #[serde(rename = "supabase")]
    Supabase,
    #[serde(rename = "azure-blob", alias = "azure")]
    AzureBlob,
    #[serde(rename = "google-cloud-storage", alias = "gcs", alias = "google-cloud")]
    GoogleCloudStorage,
    #[serde(rename = "http")]
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderConflictPolicy {
    #[default]
    Fail,
    PreferLocal,
    PreferRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderAuthMode {
    #[default]
    Auto,
    None,
    ApiKey,
    Bearer,
    Basic,
    Header,
    AwsSigV4,
    AzureSas,
    OAuthBearer,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAuthSettings {
    #[serde(default)]
    pub mode: ProviderAuthMode,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    #[serde(default)]
    pub username_env: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub header_value_env: Option<String>,
    #[serde(default)]
    pub access_key_env: Option<String>,
    #[serde(default)]
    pub secret_key_env: Option<String>,
    #[serde(default)]
    pub session_token_env: Option<String>,
    #[serde(default)]
    pub sas_token_env: Option<String>,
    #[serde(default)]
    pub oauth_token_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudNodeSettings {
    #[serde(default = "default_format_version")]
    pub format_version: u16,
    pub node: NodeSettings,
    #[serde(default)]
    pub upstream: Option<UpstreamSettings>,
    #[serde(default)]
    pub provider: Option<ProviderSettings>,
    #[serde(default)]
    pub replication: ReplicationSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSettings {
    pub id: String,
    #[serde(default)]
    pub mode: NodeMode,
    pub storage_root: PathBuf,
    #[serde(default = "default_backup_versions")]
    pub backup_versions: usize,
    #[serde(default = "default_true")]
    pub preserve_original: bool,
    #[serde(default = "default_video_chunk_bytes")]
    pub video_chunk_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamSettings {
    pub url: String,
    pub node_id: String,
    pub public_key: String,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_true")]
    pub sync_on_connect: bool,
    #[serde(default = "default_reconnect_delay_ms")]
    pub reconnect_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettings {
    pub kind: ProviderKind,
    pub namespace: String,
    pub bucket: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub auth: ProviderAuthSettings,
    // Compatibility fields for the first provider prototype. New configs should
    // use provider.auth.*Env instead.
    #[serde(default)]
    pub credential_env: Option<String>,
    #[serde(default)]
    pub access_key_env: Option<String>,
    #[serde(default)]
    pub secret_key_env: Option<String>,
    #[serde(default)]
    pub session_token_env: Option<String>,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub conflict_policy: ProviderConflictPolicy,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
    #[serde(default = "default_true")]
    pub sync_on_connect: bool,
    #[serde(default = "default_reconnect_delay_ms")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_provider_max_reconnect_delay_ms")]
    pub max_reconnect_delay_ms: u64,
    #[serde(default = "default_provider_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_provider_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_provider_read_timeout_ms")]
    pub read_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSettings {
    #[serde(default)]
    pub require_boot_recovery: bool,
    #[serde(default = "default_boot_recovery_timeout_ms")]
    pub boot_recovery_timeout_ms: u64,
    #[serde(default)]
    pub targets: Vec<ReplicationTarget>,
}

impl Default for ReplicationSettings {
    fn default() -> Self {
        Self {
            require_boot_recovery: false,
            boot_recovery_timeout_ms: default_boot_recovery_timeout_ms(),
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationTarget {
    pub node_id: String,
    pub url: String,
    pub public_key: String,
    #[serde(default)]
    pub durable: bool,
}

impl CloudNodeSettings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))?;
        let settings: Self = serde_json::from_str(&source)
            .map_err(|error| anyhow::anyhow!("invalid {}: {error}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.format_version != 1 {
            anyhow::bail!(
                "unsupported Cloud Node settings format {}",
                self.format_version
            );
        }
        validate_node_id(&self.node.id)?;
        if self.node.storage_root.as_os_str().is_empty() {
            anyhow::bail!("Cloud Node storageRoot cannot be empty");
        }
        if self.node.backup_versions == 0 || self.node.backup_versions > 1024 {
            anyhow::bail!("Cloud Node backupVersions must be between 1 and 1024");
        }
        if !(1024 * 1024..=64 * 1024 * 1024).contains(&self.node.video_chunk_bytes) {
            anyhow::bail!("Cloud Node videoChunkBytes must be between 1 MiB and 64 MiB");
        }
        if self.upstream.is_some() && self.provider.is_some() {
            anyhow::bail!(
                "Cloud Node settings must choose either upstream peer mode or provider mode, not both"
            );
        }
        if let Some(upstream) = &self.upstream {
            validate_peer_url(&upstream.url)?;
            validate_node_id(&upstream.node_id)?;
            validate_public_key(&upstream.public_key)?;
            validate_reconnect_delay(upstream.reconnect_delay_ms)?;
        }
        if let Some(provider) = &self.provider {
            validate_provider(provider)?;
        }
        if !(1_000..=3_600_000).contains(&self.replication.boot_recovery_timeout_ms) {
            anyhow::bail!("Cloud Node bootRecoveryTimeoutMs must be between 1000 and 3600000");
        }
        if self.replication.require_boot_recovery && self.replication.targets.is_empty() {
            anyhow::bail!(
                "Cloud Node requireBootRecovery needs at least one trusted replication target"
            );
        }
        for target in &self.replication.targets {
            validate_node_id(&target.node_id)?;
            validate_peer_url(&target.url)?;
            validate_public_key(&target.public_key)?;
        }
        Ok(())
    }
}

fn validate_provider(provider: &ProviderSettings) -> anyhow::Result<()> {
    validate_namespace(&provider.namespace)?;
    validate_bucket(&provider.bucket)?;
    validate_prefix(&provider.prefix)?;
    validate_reconnect_delay(provider.reconnect_delay_ms)?;
    validate_provider_max_reconnect_delay(
        provider.reconnect_delay_ms,
        provider.max_reconnect_delay_ms,
    )?;
    validate_provider_poll_interval(provider.poll_interval_ms)?;
    validate_provider_timeout("connectTimeoutMs", provider.connect_timeout_ms)?;
    validate_provider_timeout("readTimeoutMs", provider.read_timeout_ms)?;

    if let Some(endpoint) = &provider.endpoint {
        validate_provider_endpoint(endpoint)?;
    }
    match provider.kind {
        ProviderKind::AmazonS3 => {
            if provider.region.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("Cloud Node amazon-s3 provider requires region");
            }
            validate_auth_mode(
                provider,
                &[ProviderAuthMode::Auto, ProviderAuthMode::AwsSigV4],
            )?;
        }
        ProviderKind::Supabase => {
            if provider.endpoint.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("Cloud Node supabase provider requires endpoint");
            }
            validate_auth_mode(
                provider,
                &[
                    ProviderAuthMode::Auto,
                    ProviderAuthMode::ApiKey,
                    ProviderAuthMode::Bearer,
                    ProviderAuthMode::Header,
                ],
            )?;
        }
        ProviderKind::AzureBlob => {
            let has_endpoint = provider
                .endpoint
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let has_account = provider
                .account
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            if !has_endpoint && !has_account {
                anyhow::bail!("Cloud Node azure-blob provider requires account or endpoint");
            }
            validate_auth_mode(
                provider,
                &[ProviderAuthMode::Auto, ProviderAuthMode::AzureSas],
            )?;
        }
        ProviderKind::GoogleCloudStorage => {
            validate_auth_mode(
                provider,
                &[
                    ProviderAuthMode::Auto,
                    ProviderAuthMode::OAuthBearer,
                    ProviderAuthMode::Bearer,
                ],
            )?;
        }
        ProviderKind::Http => {
            if provider.endpoint.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("Cloud Node http provider requires endpoint");
            }
        }
    }

    for env_name in [
        provider.credential_env.as_deref(),
        provider.access_key_env.as_deref(),
        provider.secret_key_env.as_deref(),
        provider.session_token_env.as_deref(),
        provider.auth.api_key_env.as_deref(),
        provider.auth.bearer_token_env.as_deref(),
        provider.auth.username_env.as_deref(),
        provider.auth.password_env.as_deref(),
        provider.auth.header_value_env.as_deref(),
        provider.auth.access_key_env.as_deref(),
        provider.auth.secret_key_env.as_deref(),
        provider.auth.session_token_env.as_deref(),
        provider.auth.sas_token_env.as_deref(),
        provider.auth.oauth_token_env.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_env_name(env_name)?;
    }

    if let Some(header_name) = &provider.auth.header_name {
        validate_header_name(header_name)?;
    }
    match provider.auth.mode {
        ProviderAuthMode::Basic => {
            if provider.auth.username_env.is_some() ^ provider.auth.password_env.is_some() {
                anyhow::bail!(
                    "Cloud Node basic provider auth must configure both usernameEnv and passwordEnv or neither"
                );
            }
        }
        ProviderAuthMode::Header
            if provider
                .auth
                .header_name
                .as_deref()
                .is_none_or(str::is_empty) =>
        {
            anyhow::bail!("Cloud Node header provider auth requires headerName");
        }
        _ => {}
    }
    Ok(())
}

fn validate_auth_mode(
    provider: &ProviderSettings,
    allowed: &[ProviderAuthMode],
) -> anyhow::Result<()> {
    if !allowed.contains(&provider.auth.mode) {
        anyhow::bail!(
            "Cloud Node {:?} provider does not support {:?} authentication",
            provider.kind,
            provider.auth.mode
        );
    }
    Ok(())
}

fn validate_namespace(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!(
            "Cloud Node provider namespace must use 1..=128 ASCII [A-Za-z0-9_.-] characters and cannot be . or .."
        );
    }
    Ok(())
}

fn validate_bucket(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.bytes().any(|byte| byte <= b' ' || byte == b'/')
    {
        anyhow::bail!(
            "Cloud Node provider bucket/container must be a non-empty name without spaces or slashes"
        );
    }
    Ok(())
}

fn validate_prefix(value: &str) -> anyhow::Result<()> {
    if value.len() > 512 || value.starts_with('/') || value.contains("..") || value.contains('\\') {
        anyhow::bail!(
            "Cloud Node provider prefix must be relative, <=512 bytes, and cannot contain '..' or backslashes"
        );
    }
    Ok(())
}

fn validate_env_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        anyhow::bail!(
            "Cloud Node provider credential environment names must use ASCII [A-Za-z0-9_] characters"
        );
    }
    Ok(())
}

fn validate_header_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("Cloud Node provider auth headerName contains invalid characters");
    }
    Ok(())
}

fn validate_reconnect_delay(value: u64) -> anyhow::Result<()> {
    if !(250..=300_000).contains(&value) {
        anyhow::bail!("Cloud Node reconnectDelayMs must be between 250 and 300000");
    }
    Ok(())
}

fn validate_provider_max_reconnect_delay(initial: u64, maximum: u64) -> anyhow::Result<()> {
    if !(250..=3_600_000).contains(&maximum) {
        anyhow::bail!("Cloud Node provider maxReconnectDelayMs must be between 250 and 3600000");
    }
    if maximum < initial {
        anyhow::bail!(
            "Cloud Node provider maxReconnectDelayMs cannot be smaller than reconnectDelayMs"
        );
    }
    Ok(())
}

fn validate_provider_poll_interval(value: u64) -> anyhow::Result<()> {
    if !(1_000..=3_600_000).contains(&value) {
        anyhow::bail!("Cloud Node provider pollIntervalMs must be between 1000 and 3600000");
    }
    Ok(())
}

fn validate_provider_timeout(label: &str, value: u64) -> anyhow::Result<()> {
    if !(250..=300_000).contains(&value) {
        anyhow::bail!("Cloud Node provider {label} must be between 250 and 300000");
    }
    Ok(())
}

pub(crate) fn validate_node_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!(
            "Cloud Node id must use 1..=128 ASCII [A-Za-z0-9_.-] characters and cannot be . or .."
        );
    }
    Ok(())
}

fn validate_peer_url(value: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid Cloud Node peer URL: {error}"))?;
    let host = parsed
        .host()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node peer URL must include a host"))?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("Cloud Node peer URL cannot embed credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("Cloud Node peer URL cannot include a query or fragment");
    }

    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = match host {
                url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
            };
            if !loopback {
                anyhow::bail!(
                    "Cloud Node peer URL must use HTTPS outside exact loopback development hosts"
                );
            }
            Ok(())
        }
        "ws" | "wss" => anyhow::bail!(
            "Cloud Node RBE-CN/1 currently uses HTTP POST transport; ws/wss peer URLs are not supported"
        ),
        scheme => anyhow::bail!(
            "Cloud Node peer URL uses unsupported scheme {scheme:?}; expected https or loopback http"
        ),
    }
}

fn validate_provider_endpoint(value: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid Cloud Node provider endpoint: {error}"))?;
    let host = parsed
        .host()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider endpoint must include a host"))?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("Cloud Node provider endpoint cannot embed credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("Cloud Node provider endpoint cannot include a query or fragment");
    }

    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = match host {
                url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
            };
            if !loopback {
                anyhow::bail!(
                    "Cloud Node provider endpoint must use HTTPS outside exact loopback development hosts"
                );
            }
            Ok(())
        }
        scheme => anyhow::bail!(
            "Cloud Node provider endpoint uses unsupported scheme {scheme:?}; expected https or loopback http"
        ),
    }
}

fn validate_public_key(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("Cloud Node public keys must be 32-byte hexadecimal Ed25519 keys");
    }
    Ok(())
}

const fn default_format_version() -> u16 {
    1
}
const fn default_backup_versions() -> usize {
    5
}
const fn default_true() -> bool {
    true
}
const fn default_video_chunk_bytes() -> usize {
    4 * 1024 * 1024
}
const fn default_reconnect_delay_ms() -> u64 {
    2_000
}
const fn default_provider_max_reconnect_delay_ms() -> u64 {
    60_000
}
const fn default_provider_poll_interval_ms() -> u64 {
    30_000
}
const fn default_provider_connect_timeout_ms() -> u64 {
    10_000
}
const fn default_provider_read_timeout_ms() -> u64 {
    60_000
}
const fn default_boot_recovery_timeout_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_and_provider_namespaces_reject_filesystem_aliases() {
        assert!(validate_node_id(".").is_err());
        assert!(validate_node_id("..").is_err());
        assert!(validate_namespace(".").is_err());
        assert!(validate_namespace("..").is_err());
        validate_node_id("nas.main-1").unwrap();
        validate_namespace("prod.main-1").unwrap();
    }

    #[test]
    fn peer_url_plaintext_requires_an_exact_loopback_host() {
        validate_peer_url("http://localhost:8080/base").unwrap();
        validate_peer_url("http://127.0.0.1:8080").unwrap();
        validate_peer_url("http://127.12.34.56:8080").unwrap();
        validate_peer_url("http://[::1]:8080").unwrap();
        validate_peer_url("https://cloud.example.test").unwrap();

        assert!(validate_peer_url("http://localhost.evil.example").is_err());
        assert!(validate_peer_url("http://127.0.0.1.evil.example").is_err());
        assert!(validate_peer_url("http://192.168.1.10:8080").is_err());
    }

    #[test]
    fn peer_url_matches_the_implemented_http_transport() {
        assert!(validate_peer_url("ws://localhost:8080").is_err());
        assert!(validate_peer_url("wss://cloud.example.test").is_err());
        assert!(validate_peer_url("ftp://cloud.example.test").is_err());
        assert!(validate_peer_url("https://user:pass@cloud.example.test").is_err());
        assert!(validate_peer_url("https://cloud.example.test?token=nope").is_err());
        assert!(validate_peer_url("https://cloud.example.test/#fragment").is_err());
    }

    #[test]
    fn provider_network_timeouts_have_safe_defaults_and_bounds() {
        let provider: ProviderSettings = serde_json::from_value(serde_json::json!({
            "kind":"http",
            "namespace":"prod",
            "bucket":"rbe",
            "endpoint":"https://storage.example.test"
        }))
        .unwrap();
        assert_eq!(provider.max_reconnect_delay_ms, 60_000);
        assert_eq!(provider.poll_interval_ms, 30_000);
        assert_eq!(provider.connect_timeout_ms, 10_000);
        assert_eq!(provider.read_timeout_ms, 60_000);
        validate_provider(&provider).unwrap();

        let mut invalid = provider.clone();
        invalid.connect_timeout_ms = 249;
        assert!(validate_provider(&invalid).is_err());
        invalid = provider.clone();
        invalid.read_timeout_ms = 300_001;
        assert!(validate_provider(&invalid).is_err());
        invalid = provider.clone();
        invalid.poll_interval_ms = 999;
        assert!(validate_provider(&invalid).is_err());
        invalid = provider.clone();
        invalid.max_reconnect_delay_ms = provider.reconnect_delay_ms - 1;
        assert!(validate_provider(&invalid).is_err());
        invalid = provider.clone();
        invalid.max_reconnect_delay_ms = 3_600_001;
        assert!(validate_provider(&invalid).is_err());
    }

    #[test]
    fn provider_endpoint_uses_parsed_exact_loopback_rules() {
        validate_provider_endpoint("http://localhost:9000/api").unwrap();
        validate_provider_endpoint("http://127.0.0.1:9000").unwrap();
        validate_provider_endpoint("http://[::1]:9000").unwrap();
        validate_provider_endpoint("https://storage.example.test/base").unwrap();

        assert!(validate_provider_endpoint("http://localhost.evil.example").is_err());
        assert!(validate_provider_endpoint("http://192.168.1.10:9000").is_err());
        assert!(validate_provider_endpoint("https://user:pass@storage.example.test").is_err());
        assert!(validate_provider_endpoint("https://storage.example.test?token=nope").is_err());
    }

    #[test]
    fn defaults_backup_history_to_five() {
        let settings: CloudNodeSettings =
            serde_json::from_str(r#"{"node":{"id":"nas-main","storageRoot":"/srv/nas"}}"#).unwrap();
        assert_eq!(settings.node.backup_versions, 5);
        assert!(settings.node.preserve_original);
        assert!(!settings.replication.require_boot_recovery);
        assert_eq!(settings.replication.boot_recovery_timeout_ms, 60_000);
        settings.validate().unwrap();
    }

    #[test]
    fn provider_mode_rejects_peer_upstream_at_the_same_time() {
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node": {"id":"nas-main","storageRoot":"/srv/nas"},
            "upstream": {
                "url":"https://peer.example",
                "nodeId":"peer",
                "publicKey":"0000000000000000000000000000000000000000000000000000000000000000"
            },
            "provider": {
                "kind":"google-cloud-storage",
                "namespace":"prod",
                "bucket":"rbe-backups"
            }
        }))
        .unwrap();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn provider_aliases_and_defaults_are_stable() {
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node": {"id":"nas-main","storageRoot":"/srv/nas"},
            "provider": {
                "kind":"s3",
                "namespace":"prod",
                "bucket":"rbe-backups",
                "region":"ap-south-1"
            }
        }))
        .unwrap();
        settings.validate().unwrap();
        let provider = settings.provider.unwrap();
        assert_eq!(provider.kind, ProviderKind::AmazonS3);
        assert_eq!(provider.auth.mode, ProviderAuthMode::Auto);
        assert_eq!(provider.conflict_policy, ProviderConflictPolicy::Fail);
        assert!(provider.auto_reconnect);
        assert!(provider.sync_on_connect);
    }

    #[test]
    fn provider_rejects_invalid_fixed_auth_mode() {
        let settings: CloudNodeSettings = serde_json::from_value(serde_json::json!({
            "node": {"id":"nas-main","storageRoot":"/srv/nas"},
            "provider": {
                "kind":"amazon-s3",
                "namespace":"prod",
                "bucket":"rbe-backups",
                "region":"ap-south-1",
                "auth":{"mode":"basic"}
            }
        }))
        .unwrap();
        assert!(settings.validate().is_err());
    }
}
