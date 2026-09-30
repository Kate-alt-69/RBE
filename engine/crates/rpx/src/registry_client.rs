//! Network client for the frozen Kastrick RPX read API.
//!
//! This reuses RBE's existing reqwest + rustls stack. It intentionally exposes
//! only the public read surface required by install/index operations; publisher
//! authentication is a separate contract.

use crate::registry_contract::{IndexListResponse, PackageIndexResponse, ResolvedRegistryRelease};
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use std::time::Duration;
use url::Url;

pub const DEFAULT_REGISTRY_URL: &str = "https://kastrick-backend.onrender.com";
pub const REGISTRY_URL_ENV: &str = "RPX_REGISTRY_URL";
pub const LEGACY_REGISTRY_URL_ENV: &str = "RBE_RPX_REGISTRY_URL";
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RegistryClient {
    base: Url,
    client: Client,
}

impl RegistryClient {
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
            .redirect(Policy::limited(5))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .user_agent(concat!("rpx/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to initialize RPX registry HTTP client")?;
        Ok(Self { base, client })
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
        Self::new(DEFAULT_REGISTRY_URL)
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    pub fn fetch_index_list(&self) -> Result<IndexListResponse> {
        let mut url = self.endpoint()?;
        url.query_pairs_mut().append_pair("index-list", "");
        let body = self.get_text(url)?;
        IndexListResponse::parse(&body)
    }

    pub fn resolve_package(
        &self,
        package: &str,
        version: Option<&str>,
    ) -> Result<ResolvedRegistryRelease> {
        let mut url = self.endpoint()?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("package", package);
            if let Some(version) = version {
                query.append_pair("version", version);
            }
        }
        let body = self.get_text(url)?;
        PackageIndexResponse::parse_resolved(&body, package, version)
    }

    fn endpoint(&self) -> Result<Url> {
        self.base
            .join("api/rpx/index")
            .context("failed to construct RPX registry endpoint")
    }

    fn get_text(&self, url: Url) -> Result<String> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .with_context(|| format!("failed to contact RPX registry at {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .unwrap_or_else(|_| "<response body unavailable>".to_string());
            let bounded = truncate_for_error(&body, 4096);
            bail!("RPX registry returned HTTP {status} for {url}: {bounded}");
        }
        if let Some(length) = response.content_length() {
            if length > MAX_INDEX_BYTES as u64 {
                bail!("RPX registry response exceeds {MAX_INDEX_BYTES} bytes");
            }
        }
        let bytes = response
            .bytes()
            .with_context(|| format!("failed to read RPX registry response from {url}"))?;
        if bytes.len() > MAX_INDEX_BYTES {
            bail!("RPX registry response exceeds {MAX_INDEX_BYTES} bytes");
        }
        String::from_utf8(bytes.to_vec()).context("RPX registry response was not valid UTF-8")
    }
}

fn validate_registry_base(url: &Url) -> Result<()> {
    if url.username() != "" || url.password().is_some() {
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

fn is_loopback(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

fn truncate_for_error(input: &str, limit: usize) -> String {
    if input.len() <= limit {
        return input.to_string();
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
    fn default_registry_is_valid_production_https() {
        let client = RegistryClient::new(DEFAULT_REGISTRY_URL).unwrap();
        assert_eq!(client.base_url().as_str(), "https://kastrick-backend.onrender.com/");
    }

    #[test]
    fn production_registry_requires_https() {
        assert!(RegistryClient::new("https://registry.example").is_ok());
        assert!(RegistryClient::new("http://registry.example").is_err());
    }

    #[test]
    fn loopback_http_is_allowed_for_local_authoring() {
        assert!(RegistryClient::new("http://127.0.0.1:8080").is_ok());
        assert!(RegistryClient::new("http://localhost:8080/base").is_ok());
    }

    #[test]
    fn embedded_credentials_are_rejected() {
        assert!(RegistryClient::new("https://user:pass@registry.example").is_err());
    }

    #[test]
    fn registry_base_cannot_smuggle_query_or_fragment() {
        assert!(RegistryClient::new("https://registry.example?package=evil").is_err());
        assert!(RegistryClient::new("https://registry.example/#evil").is_err());
    }
}
