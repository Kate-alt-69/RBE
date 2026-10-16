use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, Context, Result};

const MAX_RESOLVED_ADDRESSES: usize = 16;
const MAX_URL_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPublicHttpsTarget {
    pub host: String,
    pub port: u16,
    pub host_header: String,
    pub request_target: String,
    pub address: SocketAddr,
}

/// Resolve one trusted HTTPS transfer target without exposing package-level
/// socket authority. This is intended for host-owned flows such as presigned
/// deployment artifact transfers; it is not reachable through REL `http`.
pub async fn resolve_public_https_target(value: &str) -> Result<ResolvedPublicHttpsTarget> {
    if value.is_empty() || value.len() > MAX_URL_BYTES || value.chars().any(char::is_control) {
        bail!("trusted HTTPS target URL is invalid");
    }
    let url = reqwest::Url::parse(value).context("parse trusted HTTPS target URL")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("trusted transfer target must be credential-free HTTPS without a fragment");
    }
    let host = url
        .host_str()
        .context("trusted HTTPS target is missing a host")?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || !host.contains('.')
        || host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || host.parse::<IpAddr>().is_ok()
    {
        bail!("trusted HTTPS target must use a public DNS hostname");
    }
    let port = url
        .port_or_known_default()
        .context("trusted HTTPS target has no usable port")?;
    if port == 0 {
        bail!("trusted HTTPS target port must be positive");
    }

    let mut addresses = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("resolve trusted HTTPS target {host:?}"))?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("trusted HTTPS target DNS resolution returned no addresses");
    }
    if addresses.len() > MAX_RESOLVED_ADDRESSES {
        bail!("trusted HTTPS target DNS resolution returned too many addresses");
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        bail!("trusted HTTPS target hostname resolves to a non-public address");
    }

    let mut request_target = url.path().to_string();
    if request_target.is_empty() {
        request_target.push('/');
    }
    if let Some(query) = url.query() {
        request_target.push('?');
        request_target.push_str(query);
    }
    if request_target.contains('\r') || request_target.contains('\n') {
        bail!("trusted HTTPS request target contains control characters");
    }
    let host_header = if port == 443 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };

    Ok(ResolvedPublicHttpsTarget {
        host,
        port,
        host_header,
        request_target,
        address: addresses[0],
    })
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
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
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
