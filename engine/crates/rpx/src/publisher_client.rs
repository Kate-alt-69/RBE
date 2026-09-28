//! Authenticated client for the Kastrick RPX publisher contract.
//!
//! This module intentionally keeps publisher identity opaque. The CLI stores
//! only the scoped RPX bearer token returned by device authorization; UAC
//! private IDs and registry owner keys never cross this boundary.

use crate::registry_client::{LEGACY_REGISTRY_URL_ENV, REGISTRY_URL_ENV};
use anyhow::{bail, Context, Result};
use reqwest::blocking::{Body, Client, Response};
use reqwest::redirect::Policy;
use reqwest::StatusCode;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;
use url::Url;

const MAX_API_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PublisherClient {
    base: Url,
    client: Client,
    upload_client: Client,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStartResponse {
    pub ok: bool,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_at: u64,
    pub interval_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessTokenResponse {
    pub ok: bool,
    pub token_type: String,
    pub access_token: String,
    pub service_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
pub enum DevicePoll {
    Pending,
    Authorized(AccessTokenResponse),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadResponse {
    pub ok: bool,
    pub upload_id: String,
    pub method: String,
    pub upload_url: String,
    pub expires_at_unix: u64,
    pub max_archive_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResponse {
    pub ok: bool,
    pub claimed_name: bool,
    pub revision: String,
    pub release: PublishedRelease,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedRelease {
    pub package: String,
    pub version: String,
    pub archive_sha256: String,
    pub manifest_sha256: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherPackagesResponse {
    pub ok: bool,
    pub revision: String,
    #[serde(default)]
    pub packages: Vec<PublisherPackage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherPackage {
    pub name: String,
    pub latest_stable: Option<String>,
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevicePollRequest<'a> {
    device_code: &'a str,
}

#[derive(Debug, Serialize)]
struct UploadAction<'a> {
    action: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishAction<'a> {
    action: &'a str,
    upload_id: &'a str,
}

impl PublisherClient {
    pub fn new(base: &str) -> Result<Self> {
        let mut base = Url::parse(base).context("invalid RPX registry base URL")?;
        validate_registry_base(&base)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        base.set_query(None);
        base.set_fragment(None);

        let client = Client::builder()
            .https_only(false)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .user_agent(concat!("rpx/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize RPX publisher HTTP client")?;
        let upload_client = Client::builder()
            .https_only(false)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(5 * 60))
            .user_agent(concat!("rpx/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize RPX upload HTTP client")?;
        Ok(Self {
            base,
            client,
            upload_client,
        })
    }

    pub fn from_override_or_env(override_url: Option<&str>) -> Result<Self> {
        if let Some(value) = override_url {
            return Self::new(value);
        }
        if let Ok(value) = std::env::var(REGISTRY_URL_ENV) {
            if !value.trim().is_empty() {
                return Self::new(&value);
            }
        }
        if let Ok(value) = std::env::var(LEGACY_REGISTRY_URL_ENV) {
            if !value.trim().is_empty() {
                return Self::new(&value);
            }
        }
        bail!(
            "RPX registry URL is not configured; pass --registry <https://host> or set {REGISTRY_URL_ENV}"
        )
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    pub fn registry_key(&self) -> String {
        self.base.as_str().to_owned()
    }

    pub fn start_device(&self) -> Result<DeviceStartResponse> {
        let url = self.endpoint("api/rpx/auth/device/start")?;
        let response = self
            .client
            .post(url.clone())
            .send()
            .with_context(|| format!("failed to start RPX device authorization at {url}"))?;
        let payload: DeviceStartResponse = decode_success(response, &url)?;
        if !payload.ok {
            bail!("RPX device authorization returned ok=false");
        }
        if !valid_hex(&payload.device_code, 64) {
            bail!("RPX device authorization returned an invalid deviceCode");
        }
        if payload.user_code.trim().is_empty() || payload.user_code.len() > 128 {
            bail!("RPX device authorization returned an invalid userCode");
        }
        validate_verification_uri(&payload.verification_uri)?;
        if payload.expires_at <= unix_now() {
            bail!("RPX device authorization was already expired");
        }
        Ok(payload)
    }

    pub fn poll_device(&self, device_code: &str) -> Result<DevicePoll> {
        if !valid_hex(device_code, 64) {
            bail!("invalid RPX device code");
        }
        let url = self.endpoint("api/rpx/auth/device/poll")?;
        let response = self
            .client
            .post(url.clone())
            .json(&DevicePollRequest { device_code })
            .send()
            .with_context(|| format!("failed to poll RPX device authorization at {url}"))?;
        if response.status() == StatusCode::ACCEPTED {
            let _ = read_bounded(response, &url)?;
            return Ok(DevicePoll::Pending);
        }
        let token: AccessTokenResponse = decode_success(response, &url)?;
        validate_access_token(&token)?;
        Ok(DevicePoll::Authorized(token))
    }

    pub fn revoke(&self, authorization: &str) -> Result<()> {
        validate_authorization(authorization)?;
        let url = self.endpoint("api/rpx/auth/revoke")?;
        let response = self
            .client
            .post(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization)
            .send()
            .with_context(|| format!("failed to revoke RPX credential at {url}"))?;
        let value: serde_json::Value = decode_success(response, &url)?;
        if value.get("ok").and_then(|value| value.as_bool()) != Some(true) {
            bail!("RPX revoke endpoint returned ok=false");
        }
        Ok(())
    }

    pub fn publisher_packages(&self, authorization: &str) -> Result<PublisherPackagesResponse> {
        validate_authorization(authorization)?;
        let url = self.endpoint("api/rpx/publisher/packages")?;
        let response = self
            .client
            .get(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization)
            .send()
            .with_context(|| format!("failed to validate RPX credential at {url}"))?;
        let payload: PublisherPackagesResponse = decode_success(response, &url)?;
        if !payload.ok {
            bail!("RPX publisher endpoint returned ok=false");
        }
        Ok(payload)
    }

    pub fn prepare_upload(
        &self,
        authorization: &str,
        version: &str,
    ) -> Result<PrepareUploadResponse> {
        validate_authorization(authorization)?;
        semver::Version::parse(version).context("package version is not valid SemVer")?;
        let url = self.upload_endpoint(version)?;
        let response = self
            .client
            .post(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(&UploadAction { action: "prepare" })
            .send()
            .with_context(|| format!("failed to prepare RPX package upload at {url}"))?;
        let payload: PrepareUploadResponse = decode_success(response, &url)?;
        validate_prepare_upload(&payload)?;
        Ok(payload)
    }

    pub fn upload_archive(&self, prepared: &PrepareUploadResponse, archive: &Path) -> Result<()> {
        validate_prepare_upload(prepared)?;
        let metadata = fs::metadata(archive)
            .with_context(|| format!("failed to inspect package archive {}", archive.display()))?;
        if !metadata.is_file() {
            bail!(
                "package archive is not a regular file: {}",
                archive.display()
            );
        }
        if metadata.len() == 0 || metadata.len() > prepared.max_archive_bytes as u64 {
            bail!(
                "package archive size {} is outside publisher limit 1..={} bytes",
                metadata.len(),
                prepared.max_archive_bytes
            );
        }
        let url = validate_upload_url(&prepared.upload_url)?;
        let file = fs::File::open(archive)
            .with_context(|| format!("failed to open package archive {}", archive.display()))?;
        let response = self
            .upload_client
            .put(url.clone())
            .header(reqwest::header::CONTENT_LENGTH, metadata.len())
            .body(Body::new(file))
            .send()
            .with_context(|| format!("failed to upload package archive to {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = read_bounded(response, &url).unwrap_or_else(|_| "<body unavailable>".into());
            bail!(
                "package upload failed with HTTP {status}: {}",
                truncate_for_error(&body, 4096)
            );
        }
        Ok(())
    }

    pub fn finalize_publish(
        &self,
        authorization: &str,
        version: &str,
        upload_id: &str,
    ) -> Result<PublishResponse> {
        validate_authorization(authorization)?;
        semver::Version::parse(version).context("package version is not valid SemVer")?;
        if !valid_hex(upload_id, 64) {
            bail!("publisher returned an invalid uploadId");
        }
        let url = self.upload_endpoint(version)?;
        let response = self
            .client
            .post(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(&PublishAction {
                action: "publish",
                upload_id,
            })
            .send()
            .with_context(|| format!("failed to finalize RPX package publication at {url}"))?;
        let payload: PublishResponse = decode_success(response, &url)?;
        if !payload.ok || payload.release.version != version {
            bail!("RPX publisher returned inconsistent publication metadata");
        }
        Ok(payload)
    }

    fn upload_endpoint(&self, version: &str) -> Result<Url> {
        let mut url = self.endpoint("api/rpx/package/upload")?;
        url.query_pairs_mut().append_pair("version", version);
        Ok(url)
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("failed to construct RPX publisher endpoint for {path}"))
    }
}

fn decode_success<T: DeserializeOwned>(response: Response, url: &Url) -> Result<T> {
    let status = response.status();
    let body = read_bounded(response, url)?;
    if !status.is_success() {
        bail!(
            "RPX publisher returned HTTP {status} for {url}: {}",
            truncate_for_error(&body, 4096)
        );
    }
    serde_json::from_str(&body)
        .with_context(|| format!("RPX publisher response from {url} was invalid JSON"))
}

fn read_bounded(response: Response, url: &Url) -> Result<String> {
    if let Some(length) = response.content_length() {
        if length > MAX_API_BYTES as u64 {
            bail!("RPX publisher response from {url} exceeds {MAX_API_BYTES} bytes");
        }
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read RPX publisher response from {url}"))?;
    if bytes.len() > MAX_API_BYTES {
        bail!("RPX publisher response from {url} exceeds {MAX_API_BYTES} bytes");
    }
    String::from_utf8(bytes.to_vec()).context("RPX publisher response was not valid UTF-8")
}

fn validate_prepare_upload(payload: &PrepareUploadResponse) -> Result<()> {
    if !payload.ok || payload.method != "PUT" || !valid_hex(&payload.upload_id, 64) {
        bail!("RPX publisher returned an invalid upload preparation response");
    }
    if payload.max_archive_bytes == 0 || payload.expires_at_unix <= unix_now() {
        bail!("RPX publisher returned an expired or unusable upload slot");
    }
    validate_upload_url(&payload.upload_url)?;
    Ok(())
}

fn validate_access_token(payload: &AccessTokenResponse) -> Result<()> {
    if !payload.ok
        || payload.token_type != "Bearer"
        || payload.service_id != "rpx"
        || !valid_hex(&payload.access_token, 64)
        || payload.expires_at <= unix_now()
    {
        bail!("RPX authorization returned an invalid access credential");
    }
    Ok(())
}

fn validate_authorization(value: &str) -> Result<()> {
    let Some(token) = value.strip_prefix("Bearer ") else {
        bail!("invalid RPX bearer credential");
    };
    if !valid_hex(token, 64) {
        bail!("invalid RPX bearer credential");
    }
    Ok(())
}

fn validate_registry_base(url: &Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        bail!("RPX registry URL must not contain embedded credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("RPX registry base URL must not contain a query or fragment");
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(url.host_str()) => Ok(()),
        "http" => bail!("RPX registry requires HTTPS except for loopback development hosts"),
        scheme => bail!("unsupported RPX registry URL scheme {scheme:?}"),
    }
}

fn validate_verification_uri(value: &str) -> Result<()> {
    let url = Url::parse(value).context("RPX verificationUri was not a valid URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("RPX verificationUri must be credential-free HTTPS");
    }
    Ok(())
}

fn validate_upload_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("RPX upload URL was not a valid URL")?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("RPX upload URL contains unsupported credentials or fragment");
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(url.host_str()) => Ok(url),
        _ => bail!("RPX upload URL must use HTTPS except for loopback development"),
    }
}

fn is_loopback(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

    #[test]
    fn publisher_base_requires_https_outside_loopback() {
        assert!(PublisherClient::new("https://registry.example").is_ok());
        assert!(PublisherClient::new("http://registry.example").is_err());
        assert!(PublisherClient::new("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn access_token_contract_is_fail_closed() {
        let good = AccessTokenResponse {
            ok: true,
            token_type: "Bearer".into(),
            access_token: "a".repeat(64),
            service_id: "rpx".into(),
            scopes: vec!["package.publish".into()],
            expires_at: unix_now().saturating_add(60),
        };
        assert!(validate_access_token(&good).is_ok());
        let mut bad = good.clone();
        bad.service_id = "other".into();
        assert!(validate_access_token(&bad).is_err());
    }

    #[test]
    fn upload_url_rejects_insecure_remote_transport() {
        assert!(validate_upload_url("https://storage.example/object?sig=1").is_ok());
        assert!(validate_upload_url("http://storage.example/object?sig=1").is_err());
        assert!(validate_upload_url("http://localhost:9000/object?sig=1").is_ok());
    }
}
