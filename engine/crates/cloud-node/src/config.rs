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
            anyhow::bail!("Cloud Node settings must choose either upstream peer mode or provider mode, not both");
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

    if let Some(endpoint) = &provider.endpoint {
        validate_provider_endpoint(endpoint)?;
    }
    match provider.kind {
        ProviderKind::AmazonS3 => {
            if provider.region.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("Cloud Node amazon-s3 provider requires region");
            }
        }
        ProviderKind::Supabase => {
            if provider.endpoint.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("Cloud Node supabase provider requires endpoint");
            }
        }
        ProviderKind::AzureBlob => {
            let has_endpoint = provider.endpoint.as_deref().is_some_and(|value| !value.is_empty());
            let has_account = provider.account.as_deref().is_some_and(|value| !value.is_empty());
            if !has_endpoint && !has_account {
                anyhow::bail!("Cloud Node azure-blob provider requires account or endpoint");
            }
        }
        ProviderKind::GoogleCloudStorage => {}
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
    ]
    .into_iter()
    .flatten()
    {
        validate_env_name(env_name)?;
    }
    Ok(())
}

fn validate_namespace(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("Cloud Node provider namespace must use 1..=128 ASCII [A-Za-z0-9_.-] characters");
    }
    Ok(())
}

fn validate_bucket(value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 255 || value.bytes().any(|byte| byte <= b' ' || byte == b'/') {
        anyhow::bail!("Cloud Node provider bucket/container must be a non-empty name without spaces or slashes");
    }
    Ok(())
}

fn validate_prefix(value: &str) -> anyhow::Result<()> {
    if value.len() > 512 || value.starts_with('/') || value.contains("..") || value.contains('\\') {
        anyhow::bail!("Cloud Node provider prefix must be relative, <=512 bytes, and cannot contain '..' or backslashes");
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
        anyhow::bail!("Cloud Node provider credential environment names must use ASCII [A-Za-z0-9_] characters");
    }
    Ok(())
}

fn validate_reconnect_delay(value: u64) -> anyhow::Result<()> {
    if !(250..=300_000).contains(&value) {
        anyhow::bail!("Cloud Node reconnectDelayMs must be between 250 and 300000");
    }
    Ok(())
}

fn validate_node_id(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("Cloud Node id must use 1..=128 ASCII [A-Za-z0-9_.-] characters");
    }
    Ok(())
}

fn validate_peer_url(value: &str) -> anyhow::Result<()> {
    let secure = value.starts_with("https://") || value.starts_with("wss://");
    let local_dev = value.starts_with("http://127.0.0.1")
        || value.starts_with("http://localhost")
        || value.starts_with("ws://127.0.0.1")
        || value.starts_with("ws://localhost");
    if !secure && !local_dev {
        anyhow::bail!("Cloud Node peer URL must use TLS outside localhost development");
    }
    Ok(())
}

fn validate_provider_endpoint(value: &str) -> anyhow::Result<()> {
    let secure = value.starts_with("https://");
    let local_dev = value.starts_with("http://127.0.0.1") || value.starts_with("http://localhost");
    if !secure && !local_dev {
        anyhow::bail!("Cloud Node provider endpoint must use HTTPS outside localhost development");
    }
    Ok(())
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
const fn default_boot_recovery_timeout_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(provider.conflict_policy, ProviderConflictPolicy::Fail);
        assert!(provider.auto_reconnect);
        assert!(provider.sync_on_connect);
    }
}
