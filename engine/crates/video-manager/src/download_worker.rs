use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use tokio::io::AsyncWriteExt;

use crate::{
    parse_download_target, resolve_download_target, DownloadTarget, QueuedDownload, VideoManager,
};

const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_REDIRECTS: usize = 5;
const MAX_REDIRECTS_HARD_LIMIT: usize = 10;

#[derive(Debug, Clone)]
pub struct DownloadPolicy {
    pub max_bytes: u64,
    pub max_redirects: usize,
    pub connect_timeout: Duration,
    pub total_timeout: Duration,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(300),
        }
    }
}

impl DownloadPolicy {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        if self.max_bytes == 0 {
            anyhow::bail!("Video Manager download byte limit must be greater than zero");
        }
        if self.max_redirects > MAX_REDIRECTS_HARD_LIMIT {
            anyhow::bail!("Video Manager redirect limit cannot exceed {MAX_REDIRECTS_HARD_LIMIT}");
        }
        if self.connect_timeout.is_zero() || self.total_timeout.is_zero() {
            anyhow::bail!("Video Manager download timeouts must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadReceipt {
    pub final_url: String,
    pub bytes: u64,
    pub content_type: Option<String>,
    pub redirects: usize,
}

impl VideoManager {
    /// Execute a queued download into its Rust-generated quarantine file.
    ///
    /// This transport never accepts a caller-controlled filesystem path. DNS
    /// is resolved and vetted before each connection, reqwest is pinned to the
    /// vetted addresses, system proxies are disabled, and redirects repeat the
    /// complete URL + DNS policy before another request is sent.
    pub async fn execute_queued_download(
        &self,
        queued: &QueuedDownload,
        policy: DownloadPolicy,
    ) -> anyhow::Result<DownloadReceipt> {
        let quarantine_path = self.quarantine_path(&queued.asset.id, &queued.job.id)?;
        let result = async {
            policy.validate()?;
            if queued.asset.id != queued.job.asset_id {
                anyhow::bail!("Video Manager queued download asset/job identity mismatch");
            }
            let source_url = queued.asset.source_uri.as_deref().ok_or_else(|| {
                anyhow::anyhow!("Video Manager queued download has no source URL")
            })?;
            let target = parse_download_target(source_url)?;
            match tokio::time::timeout(
                policy.total_timeout,
                download_into_quarantine(target, &quarantine_path, &self.quarantine_root, &policy),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => anyhow::bail!(
                    "Video Manager download exceeded total timeout of {:?}",
                    policy.total_timeout
                ),
            }
        }
        .await;

        if result.is_err() {
            let _ = tokio::fs::remove_file(&quarantine_path).await;
        }
        result
    }
}

async fn download_into_quarantine(
    mut target: DownloadTarget,
    quarantine_path: &Path,
    quarantine_root: &Path,
    policy: &DownloadPolicy,
) -> anyhow::Result<DownloadReceipt> {
    let mut redirects = 0usize;

    loop {
        let target_for_resolution = target.clone();
        let resolved =
            tokio::task::spawn_blocking(move || resolve_download_target(&target_for_resolution))
                .await
                .context("Video Manager DNS validation task failed")??;

        let mut client_builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(policy.connect_timeout)
            .pool_max_idle_per_host(0)
            .user_agent("RBE-VideoManager/0.1");
        if target.host().parse::<IpAddr>().is_err() {
            client_builder = client_builder.resolve_to_addrs(target.host(), resolved.addresses());
        }
        let client = client_builder
            .build()
            .context("build pinned Video Manager download client")?;
        let response = client
            .get(target.normalized_url())
            .send()
            .await
            .with_context(|| format!("download video target {}", target.normalized_url()))?;

        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects >= policy.max_redirects {
                anyhow::bail!(
                    "Video Manager download exceeded redirect limit of {}",
                    policy.max_redirects
                );
            }
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| anyhow::anyhow!("Video Manager redirect is missing Location"))?
                .to_str()
                .context("Video Manager redirect Location is not valid ASCII")?;
            target = redirect_target(&target, location)?;
            redirects += 1;
            continue;
        }

        if !response.status().is_success() {
            anyhow::bail!("Video Manager download returned HTTP {}", response.status());
        }

        validate_content_length(response.headers().get(CONTENT_LENGTH), policy.max_bytes)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let final_url = target.normalized_url().to_string();
        let mut response = response;
        let mut file = open_reserved_quarantine(quarantine_path, quarantine_root).await?;
        let mut bytes = 0u64;

        while let Some(chunk) = response
            .chunk()
            .await
            .context("read Video Manager download body")?
        {
            bytes = bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("Video Manager download byte count overflowed"))?;
            if bytes > policy.max_bytes {
                anyhow::bail!(
                    "Video Manager download exceeded byte limit of {}",
                    policy.max_bytes
                );
            }
            file.write_all(&chunk)
                .await
                .context("write Video Manager quarantine bytes")?;
        }
        if bytes == 0 {
            anyhow::bail!("Video Manager download returned an empty body");
        }
        file.flush()
            .await
            .context("flush Video Manager quarantine file")?;
        file.sync_data()
            .await
            .context("sync Video Manager quarantine file")?;

        return Ok(DownloadReceipt {
            final_url,
            bytes,
            content_type,
            redirects,
        });
    }
}

async fn open_reserved_quarantine(
    quarantine_path: &Path,
    quarantine_root: &Path,
) -> anyhow::Result<tokio::fs::File> {
    let before = tokio::fs::symlink_metadata(quarantine_path)
        .await
        .with_context(|| {
            format!(
                "inspect reserved Video Manager quarantine file {}",
                quarantine_path.display()
            )
        })?;
    if !before.file_type().is_file() {
        anyhow::bail!("Video Manager quarantine entry is not a reserved regular file");
    }

    let canonical = tokio::fs::canonicalize(quarantine_path)
        .await
        .with_context(|| {
            format!(
                "canonicalize reserved Video Manager quarantine file {}",
                quarantine_path.display()
            )
        })?;
    if !canonical.starts_with(quarantine_root) {
        anyhow::bail!("Video Manager quarantine file escaped its storage root");
    }

    let path = quarantine_path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "open reserved Video Manager quarantine file {}",
                    path.display()
                )
            })
    })
    .await
    .context("Video Manager quarantine open task failed")??;

    let opened = file
        .metadata()
        .context("inspect opened Video Manager quarantine handle")?;
    let after = std::fs::symlink_metadata(quarantine_path).with_context(|| {
        format!(
            "reinspect reserved Video Manager quarantine file {}",
            quarantine_path.display()
        )
    })?;
    if !opened.file_type().is_file()
        || !after.file_type().is_file()
        || !same_file_identity(&before, &opened)
        || !same_file_identity(&opened, &after)
    {
        anyhow::bail!("Video Manager quarantine file identity changed while opening");
    }

    file.set_len(0)
        .context("truncate verified Video Manager quarantine file")?;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

fn redirect_target(current: &DownloadTarget, location: &str) -> anyhow::Result<DownloadTarget> {
    if location.is_empty()
        || location.len() > 8 * 1024
        || !location.is_ascii()
        || location.contains('\\')
        || location
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        anyhow::bail!("Video Manager redirect Location is malformed or ambiguous");
    }

    let base = reqwest::Url::parse(current.normalized_url())
        .context("parse current Video Manager redirect base URL")?;
    let joined = base
        .join(location)
        .context("resolve Video Manager redirect Location")?;
    let next = parse_download_target(joined.as_str())?;
    if current.scheme() == "https" && next.scheme() == "http" {
        anyhow::bail!("Video Manager refuses HTTPS-to-HTTP download redirects");
    }
    Ok(next)
}

fn validate_content_length(
    value: Option<&reqwest::header::HeaderValue>,
    max_bytes: u64,
) -> anyhow::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let raw = value
        .to_str()
        .context("Video Manager Content-Length is not valid ASCII")?;
    let bytes = raw
        .parse::<u64>()
        .context("Video Manager Content-Length is not a valid integer")?;
    if bytes > max_bytes {
        anyhow::bail!("Video Manager Content-Length {bytes} exceeds byte limit of {max_bytes}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn quarantine_test_root(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rbe-video-quarantine-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    #[tokio::test]
    async fn reserved_quarantine_open_requires_existing_regular_file() {
        let root = quarantine_test_root("missing");
        let missing = root.join("missing.part");
        assert!(open_reserved_quarantine(&missing, &root).await.is_err());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reserved_quarantine_open_truncates_only_verified_file() {
        let root = quarantine_test_root("regular");
        let path = root.join("reserved.part");
        std::fs::write(&path, b"stale bytes").unwrap();
        let file = open_reserved_quarantine(&path, &root).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 0);
        drop(file);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reserved_quarantine_open_rejects_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let root = quarantine_test_root("symlink");
        let outside_root = quarantine_test_root("outside");
        let target = outside_root.join("target.bin");
        std::fs::write(&target, b"do not truncate").unwrap();
        let link = root.join("reserved.part");
        symlink(&target, &link).unwrap();

        assert!(open_reserved_quarantine(&link, &root).await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not truncate");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn redirect_policy_accepts_relative_public_targets() {
        let current = parse_download_target("https://example.com/a/video.mp4").unwrap();
        let next = redirect_target(&current, "../b/video.mp4?x=1").unwrap();
        assert_eq!(next.normalized_url(), "https://example.com/b/video.mp4?x=1");
    }

    #[test]
    fn redirect_policy_blocks_downgrades_and_private_targets() {
        let current = parse_download_target("https://example.com/video.mp4").unwrap();
        assert!(redirect_target(&current, "http://example.com/video.mp4").is_err());
        assert!(redirect_target(&current, "https://127.0.0.1/video.mp4").is_err());
        assert!(redirect_target(&current, "//169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn content_length_policy_is_fail_closed() {
        assert!(validate_content_length(Some(&HeaderValue::from_static("101")), 100).is_err());
        assert!(validate_content_length(Some(&HeaderValue::from_static("100")), 100).is_ok());
        assert!(validate_content_length(Some(&HeaderValue::from_static("wat")), 100).is_err());
        assert!(validate_content_length(None, 100).is_ok());
    }

    #[test]
    fn policy_rejects_unbounded_values() {
        let policy = DownloadPolicy {
            max_bytes: 0,
            ..DownloadPolicy::default()
        };
        assert!(policy.validate().is_err());
        let policy = DownloadPolicy {
            max_bytes: 1,
            max_redirects: MAX_REDIRECTS_HARD_LIMIT + 1,
            ..DownloadPolicy::default()
        };
        assert!(policy.validate().is_err());
    }
}
