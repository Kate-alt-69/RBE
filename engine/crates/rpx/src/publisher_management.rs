//! Authenticated RPX package-management operations.

use crate::publisher_client::PublisherClient;
use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
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
    let client = Client::builder()
        .https_only(false)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("rpx/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to initialize RPX package-management HTTP client")?;

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

    #[test]
    fn package_name_validation_matches_rpx_registry_rules() {
        assert!(validate_package_name("advancenet").is_ok());
        assert!(validate_package_name("rbe.tools_v2").is_ok());
        assert!(validate_package_name("Bad/Name").is_err());
        assert!(validate_package_name("bad..name").is_err());
    }

    #[test]
    fn yank_reason_is_bounded_and_nonempty() {
        assert_eq!(normalize_reason(Some(" broken build ")).unwrap(), Some("broken build"));
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
}
