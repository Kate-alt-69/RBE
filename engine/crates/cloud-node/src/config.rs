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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudNodeSettings {
    #[serde(default = "default_format_version")]
    pub format_version: u16,
    pub node: NodeSettings,
    #[serde(default)]
    pub upstream: Option<UpstreamSettings>,
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
        if let Some(upstream) = &self.upstream {
            validate_peer_url(&upstream.url)?;
            validate_node_id(&upstream.node_id)?;
            validate_public_key(&upstream.public_key)?;
            if upstream.reconnect_delay_ms < 250 || upstream.reconnect_delay_ms > 300_000 {
                anyhow::bail!("Cloud Node reconnectDelayMs must be between 250 and 300000");
            }
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

fn validate_node_id(value: &str) -> anyhow::Result<()> {
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
    fn node_ids_reject_filesystem_aliases() {
        assert!(validate_node_id(".").is_err());
        assert!(validate_node_id("..").is_err());
        validate_node_id("nas.main-1").unwrap();
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
    fn defaults_backup_history_to_five() {
        let settings: CloudNodeSettings =
            serde_json::from_str(r#"{"node":{"id":"nas-main","storageRoot":"/srv/nas"}}"#).unwrap();
        assert_eq!(settings.node.backup_versions, 5);
        assert!(settings.node.preserve_original);
        assert!(!settings.replication.require_boot_recovery);
        assert_eq!(settings.replication.boot_recovery_timeout_ms, 60_000);
        settings.validate().unwrap();
    }
}
