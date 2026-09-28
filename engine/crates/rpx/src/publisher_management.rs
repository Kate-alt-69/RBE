//! RPX package-management and public package-status operations.

use crate::publisher_client::PublisherClient;
use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::Duration;
use url::Url;

const MAX_API_BYTES: usize = 1024 * 1024;
const MAX_YANK_REASON_BYTES: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YankResponse {
    pub ok: bool,
    pub revision: String,
    pub package: String,
    pub version: String,
    pub yanked: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageStatusResponse {
    pub ok: bool,
    pub revision: String,
    pub package: PublicPackageMetadata,
    pub analytics: PublicAnalytics,
    pub release_stats: ReleaseStats,
    #[serde(default)]
    pub versions: Vec<PublicVersionStatus>,
    #[serde(default)]
    pub history: Vec<HistoryEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicPackageMetadata {
    pub name: String,
    pub description: Option<String>,
    pub latest_stable: Option<String>,
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAnalytics {
    pub downloads_total: u64,
    pub downloads_last_at: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseStats {
    pub published: usize,
    pub yanked: usize,
    pub active: usize,
    pub stable_active: usize,
    pub publishes_total: u64,
    pub yanks_total: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicVersionStatus {
    pub version: String,
    pub published_at: u64,
    pub yanked: bool,
    pub yanked_at: Option<u64>,
    pub downloads: u64,
    pub archive_sha256: String,
    pub manifest_sha256: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    pub event_id: String,
    pub action: String,
    pub version: String,
    pub at: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct YankRequest<'a> {
    package: &'a str,
    version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

pub fn yank_package(
    publisher: &PublisherClient,
    authorization: &str,
    package: &str,
    version: &str,
    reason: Option<&str>,
) -> Result<YankResponse> {
    validate_authorization(authorization)?;
    validate_package_name(package)?;
    semver::Version::parse(version).context("package version is not valid SemVer")?;
    let reason = normalize_reason(reason)?;

    let url = publisher
        .base_url()
        .join("api/rpx/package/yank")
        .context("failed to construct RPX yank endpoint")?;
    let client = management_client()?;
    let response = client
        .post(url.clone())
        .header(reqwest::header::AUTHORIZATION, authorization)
        .json(&YankRequest {
            package,
            version,
            reason,
        })
        .send()
        .with_context(|| format!("failed to yank {package}@{version} at {url}"))?;
    let payload: YankResponse = decode_success(response, &url)?;
    if !payload.ok
        || !payload.yanked
        || payload.package != package
        || payload.version != version
        || payload.revision.trim().is_empty()
    {
        bail!("RPX registry returned inconsistent yank metadata for {package}@{version}");
    }
    Ok(payload)
}

pub fn package_status(publisher: &PublisherClient, package: &str) -> Result<PackageStatusResponse> {
    validate_package_name(package)?;
    let mut url = publisher
        .base_url()
        .join("api/rpx/index/package/status")
        .context("failed to construct RPX package-status endpoint")?;
    url.query_pairs_mut().append_pair("pk", package);

    let response = management_client()?
        .get(url.clone())
        .send()
        .with_context(|| format!("failed to read RPX package status for {package} at {url}"))?;
    let payload: PackageStatusResponse = decode_success(response, &url)?;
    validate_status_response(&payload, package)?;
    Ok(payload)
}

fn management_client() -> Result<Client> {
    Client::builder()
        .https_only(false)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("rpx/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to initialize RPX package-management HTTP client")
}

fn validate_status_response(payload: &PackageStatusResponse, expected_package: &str) -> Result<()> {
    if !payload.ok || payload.revision.trim().is_empty() || payload.package.name != expected_package {
        bail!("RPX registry returned inconsistent package status for {expected_package}");
    }

    if let Some(latest) = payload.package.latest_stable.as_deref() {
        let version = semver::Version::parse(latest)
            .context("RPX package status returned an invalid latestStable version")?;
        if !version.pre.is_empty() {
            bail!("RPX package status returned a prerelease as latestStable");
        }
    }

    let mut metadata_versions = BTreeSet::new();
    for version in &payload.package.versions {
        semver::Version::parse(version)
            .with_context(|| format!("RPX package status returned invalid version {version:?}"))?;
        if !metadata_versions.insert(version.as_str()) {
            bail!("RPX package status returned duplicate version {version:?}");
        }
    }

    let mut status_versions = BTreeSet::new();
    for version in &payload.versions {
        semver::Version::parse(&version.version).with_context(|| {
            format!(
                "RPX package status returned invalid version {:?}",
                version.version
            )
        })?;
        if !metadata_versions.contains(version.version.as_str())
            || !status_versions.insert(version.version.as_str())
            || !valid_sha256(&version.archive_sha256)
            || !valid_sha256(&version.manifest_sha256)
            || version.size_bytes == 0
            || (version.yanked && version.yanked_at.is_none())
        {
            bail!(
                "RPX package status returned inconsistent release metadata for {}@{}",
                expected_package,
                version.version
            );
        }
    }
    if status_versions != metadata_versions {
        bail!("RPX package status returned an incomplete version set for {expected_package}");
    }

    let yanked = payload
        .versions
        .iter()
        .filter(|version| version.yanked)
        .count();
    let active = payload.versions.len().saturating_sub(yanked);
    if payload.release_stats.published != payload.versions.len()
        || payload.release_stats.yanked != yanked
        || payload.release_stats.active != active
        || payload.release_stats.publishes_total < payload.release_stats.published as u64
        || payload.release_stats.yanks_total < payload.release_stats.yanked as u64
    {
        bail!("RPX package status returned inconsistent release statistics for {expected_package}");
    }

    for event in &payload.history {
        if event.event_id.trim().is_empty()
            || !matches!(event.action.as_str(), "publish" | "yank")
            || semver::Version::parse(&event.version).is_err()
            || event.at == 0
        {
            bail!("RPX package status returned malformed history for {expected_package}");
        }
    }
    Ok(())
}

fn normalize_reason(reason: Option<&str>) -> Result<Option<&str>> {
    let Some(reason) = reason else {
        return Ok(None);
    };
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("--reason must not be empty");
    }
    if reason.len() > MAX_YANK_REASON_BYTES {
        bail!("--reason exceeds {MAX_YANK_REASON_BYTES} bytes");
    }
    if reason.contains('\0') {
        bail!("--reason contains a NUL byte");
    }
    Ok(Some(reason))
}

fn validate_package_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 192 {
        bail!("invalid package name {value:?}");
    }
    for segment in value.split('.') {
        if segment.is_empty()
            || !segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("invalid package name {value:?}");
        }
    }
    Ok(())
}

fn validate_authorization(value: &str) -> Result<()> {
    let Some(token) = value.strip_prefix("Bearer ") else {
        bail!("invalid RPX bearer credential");
    };
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid RPX bearer credential");
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decode_success<T: DeserializeOwned>(response: Response, url: &Url) -> Result<T> {
    let status = response.status();
    let body = read_bounded(response, url)?;
    if !status.is_success() {
        bail!(
            "RPX registry returned HTTP {status} for {url}: {}",
            truncate_for_error(&body, 4096)
        );
    }
    serde_json::from_str(&body)
        .with_context(|| format!("RPX registry response from {url} was invalid JSON"))
}

fn read_bounded(response: Response, url: &Url) -> Result<String> {
    if let Some(length) = response.content_length() {
        if length > MAX_API_BYTES as u64 {
            bail!("RPX registry response from {url} exceeds {MAX_API_BYTES} bytes");
        }
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read RPX registry response from {url}"))?;
    if bytes.len() > MAX_API_BYTES {
        bail!("RPX registry response from {url} exceeds {MAX_API_BYTES} bytes");
    }
    String::from_utf8(bytes.to_vec()).context("RPX registry response was not valid UTF-8")
}

fn truncate_for_error(input: &str, limit: usize) -> String {
    if input.len() <= limit {
        return input.to_owned();
    }
    let mut end = limit;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &input[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_fixture() -> PackageStatusResponse {
        PackageStatusResponse {
            ok: true,
            revision: "rev-1".to_owned(),
            package: PublicPackageMetadata {
                name: "advancenet".to_owned(),
                description: Some("network helpers".to_owned()),
                latest_stable: Some("1.1.0".to_owned()),
                versions: vec!["1.0.0".to_owned(), "1.1.0".to_owned()],
            },
            analytics: PublicAnalytics {
                downloads_total: 9,
                downloads_last_at: Some(10),
            },
            release_stats: ReleaseStats {
                published: 2,
                yanked: 1,
                active: 1,
                stable_active: 1,
                publishes_total: 2,
                yanks_total: 1,
            },
            versions: vec![
                PublicVersionStatus {
                    version: "1.0.0".to_owned(),
                    published_at: 1,
                    yanked: true,
                    yanked_at: Some(3),
                    downloads: 4,
                    archive_sha256: "a".repeat(64),
                    manifest_sha256: "b".repeat(64),
                    size_bytes: 10,
                },
                PublicVersionStatus {
                    version: "1.1.0".to_owned(),
                    published_at: 2,
                    yanked: false,
                    yanked_at: None,
                    downloads: 5,
                    archive_sha256: "c".repeat(64),
                    manifest_sha256: "d".repeat(64),
                    size_bytes: 11,
                },
            ],
            history: vec![HistoryEvent {
                event_id: "event-1".to_owned(),
                action: "publish".to_owned(),
                version: "1.1.0".to_owned(),
                at: 2,
                reason: None,
            }],
        }
    }

    #[test]
    fn package_name_validation_matches_rpx_registry_rules() {
        assert!(validate_package_name("advancenet").is_ok());
        assert!(validate_package_name("rbe.tools_v2").is_ok());
        assert!(validate_package_name("Bad/Name").is_err());
        assert!(validate_package_name("bad..name").is_err());
    }

    #[test]
    fn yank_reason_is_bounded_and_nonempty() {
        assert_eq!(
            normalize_reason(Some(" broken build ")).unwrap(),
            Some("broken build")
        );
        assert!(normalize_reason(Some("   ")).is_err());
        let too_large = "x".repeat(MAX_YANK_REASON_BYTES + 1);
        assert!(normalize_reason(Some(&too_large)).is_err());
    }

    #[test]
    fn bearer_credentials_are_fail_closed() {
        assert!(validate_authorization(&format!("Bearer {}", "a".repeat(64))).is_ok());
        assert!(validate_authorization("Bearer nope").is_err());
        assert!(validate_authorization(&"a".repeat(64)).is_err());
    }

    #[test]
    fn package_status_requires_complete_consistent_release_state() {
        let status = status_fixture();
        assert!(validate_status_response(&status, "advancenet").is_ok());

        let mut wrong_identity = status_fixture();
        wrong_identity.package.name = "other".to_owned();
        assert!(validate_status_response(&wrong_identity, "advancenet").is_err());

        let mut missing_release = status_fixture();
        missing_release.versions.pop();
        assert!(validate_status_response(&missing_release, "advancenet").is_err());
    }
}
