use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::header::{HeaderValue, LOCATION, RANGE};
use url::Url;

use crate::InstallRuntimeError;

const MAX_RESOLVED_ADDRESSES: usize = 16;

pub(crate) fn validate_remote_url(url: &Url) -> Result<(), InstallRuntimeError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(InstallRuntimeError::UnsafeUrl(url.to_string()));
    }
    Ok(())
}

fn forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip == Ipv4Addr::BROADCAST
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn forbidden_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || ip.to_ipv4_mapped().is_some_and(forbidden_ipv4)
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_ipv4(ip),
        IpAddr::V6(ip) => forbidden_ipv6(ip),
    }
}

async fn resolve_public_destination(url: &Url) -> Result<Option<SocketAddr>, InstallRuntimeError> {
    validate_remote_url(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| InstallRuntimeError::UnsafeUrl(url.to_string()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| InstallRuntimeError::UnsafeUrl(url.to_string()))?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        if forbidden_ip(ip) {
            return Err(InstallRuntimeError::NonPublicDestination(host.to_string()));
        }
        return Ok(None);
    }

    let mut resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|source| InstallRuntimeError::Dns {
            host: host.to_string(),
            source,
        })?
        .collect::<Vec<_>>();
    resolved.sort_unstable();
    resolved.dedup();
    if resolved.is_empty() {
        return Err(InstallRuntimeError::EmptyDns(host.to_string()));
    }
    if resolved.len() > MAX_RESOLVED_ADDRESSES {
        return Err(InstallRuntimeError::TooManyDnsAddresses(host.to_string()));
    }
    if resolved.iter().any(|address| forbidden_ip(address.ip())) {
        return Err(InstallRuntimeError::NonPublicDestination(host.to_string()));
    }
    Ok(resolved.into_iter().next())
}

async fn send_once(
    url: &Url,
    range: Option<&str>,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<reqwest::Response, InstallRuntimeError> {
    let pinned = resolve_public_destination(url).await?;
    let host = url.host_str().map(str::to_string);
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout);
    if let (Some(host), Some(address)) = (host.as_deref(), pinned) {
        builder = builder.resolve(host, address);
    }
    let client = builder.build().map_err(InstallRuntimeError::HttpClient)?;
    let mut request = client.get(url.clone());
    if let Some(range) = range {
        let value = HeaderValue::from_str(range)
            .map_err(|_| InstallRuntimeError::ResumeRejected)?;
        request = request.header(RANGE, value);
    }
    request.send().await.map_err(InstallRuntimeError::HttpRequest)
}

pub(crate) async fn get_following_redirects(
    mut url: Url,
    range: Option<&str>,
    connect_timeout_seconds: u64,
    request_timeout_seconds: u64,
    maximum_redirects: u8,
) -> Result<reqwest::Response, InstallRuntimeError> {
    validate_remote_url(&url)?;
    let connect_timeout = Duration::from_secs(connect_timeout_seconds.max(1));
    let request_timeout = Duration::from_secs(request_timeout_seconds.max(1));

    for redirect_count in 0..=maximum_redirects {
        let response = send_once(&url, range, connect_timeout, request_timeout).await?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == maximum_redirects {
            return Err(InstallRuntimeError::TooManyRedirects);
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(InstallRuntimeError::InvalidRedirect)?;
        url = url
            .join(location)
            .map_err(|source| InstallRuntimeError::InvalidUrl {
                value: location.to_string(),
                source,
            })?;
        validate_remote_url(&url)?;
    }

    Err(InstallRuntimeError::TooManyRedirects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_special_network_destinations() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(forbidden_ip(value.parse().unwrap()), "{value} must be blocked");
        }
        assert!(!forbidden_ip("1.1.1.1".parse().unwrap()));
        assert!(!forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn network_urls_are_https_credential_free_and_fragment_free() {
        assert!(validate_remote_url(&Url::parse("https://example.com/a").unwrap()).is_ok());
        for value in [
            "http://example.com/a",
            "https://user:pass@example.com/a",
            "https://example.com/a#fragment",
        ] {
            assert!(validate_remote_url(&Url::parse(value).unwrap()).is_err());
        }
    }
}
