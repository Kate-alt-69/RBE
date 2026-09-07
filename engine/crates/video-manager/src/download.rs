use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use anyhow::Context;

const MAX_DOWNLOAD_URL_LEN: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTarget {
    normalized_url: String,
    scheme: String,
    host: String,
    port: u16,
}

impl DownloadTarget {
    pub fn normalized_url(&self) -> &str {
        &self.normalized_url
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDownloadTarget {
    target: DownloadTarget,
    addresses: Vec<SocketAddr>,
}

impl ResolvedDownloadTarget {
    pub fn target(&self) -> &DownloadTarget {
        &self.target
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

/// Parse and normalize a URL before it is persisted as a download source.
///
/// This parser is intentionally stricter than a browser URL parser. Video
/// downloads are a server-side capability, so ambiguous host spellings,
/// credentials, fragments, non-ASCII input, and backslashes are rejected
/// rather than normalized into potentially surprising network targets.
pub fn parse_download_target(input: &str) -> anyhow::Result<DownloadTarget> {
    if input.is_empty() || input.len() > MAX_DOWNLOAD_URL_LEN {
        anyhow::bail!(
            "video download URL length must be between 1 and {MAX_DOWNLOAD_URL_LEN} bytes"
        );
    }
    if input != input.trim()
        || !input.is_ascii()
        || input
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || input.contains('\\')
    {
        anyhow::bail!(
            "video download URL contains unsupported whitespace, Unicode, or backslashes"
        );
    }
    if input.contains('#') {
        anyhow::bail!("video download URL fragments are not allowed");
    }

    let (raw_scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("video download URL must include http:// or https://"))?;
    let scheme = raw_scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => anyhow::bail!("video download URL scheme must be http or https"),
    };
    if rest.is_empty() {
        anyhow::bail!("video download URL is missing a host");
    }

    let authority_end = rest
        .char_indices()
        .find_map(|(index, character)| matches!(character, '/' | '?').then_some(index))
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let suffix = &rest[authority_end..];
    if authority.is_empty() {
        anyhow::bail!("video download URL is missing a host");
    }
    if authority.contains('@') {
        anyhow::bail!("video download URL credentials are not allowed");
    }

    let (host, display_host, port, explicit_port) = parse_authority(authority, default_port)?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure_public_ip(ip)?;
    }

    let port_suffix = if explicit_port && port != default_port {
        format!(":{port}")
    } else {
        String::new()
    };
    let normalized_url = format!("{scheme}://{display_host}{port_suffix}{suffix}");

    Ok(DownloadTarget {
        normalized_url,
        scheme,
        host,
        port,
    })
}

/// Resolve a previously validated target and reject the entire resolution if
/// any answer is non-public. The returned addresses are the vetted addresses
/// a worker must pin its connection to; callers must not resolve the hostname
/// a second time after this check.
pub fn resolve_download_target(target: &DownloadTarget) -> anyhow::Result<ResolvedDownloadTarget> {
    let addresses = if let Ok(ip) = target.host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, target.port)]
    } else {
        (target.host.as_str(), target.port)
            .to_socket_addrs()
            .with_context(|| format!("resolve video download host {:?}", target.host))?
            .collect()
    };
    let addresses = validate_resolved_addresses(&target.host, addresses)?;

    Ok(ResolvedDownloadTarget {
        target: target.clone(),
        addresses,
    })
}

fn parse_authority(
    authority: &str,
    default_port: u16,
) -> anyhow::Result<(String, String, u16, bool)> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("video download IPv6 host is missing ']'"))?;
        let raw_host = &bracketed[..close];
        if raw_host.contains('%') {
            anyhow::bail!("video download IPv6 zone identifiers are not allowed");
        }
        let address = raw_host
            .parse::<Ipv6Addr>()
            .map_err(|_| anyhow::anyhow!("video download URL contains an invalid IPv6 host"))?;
        let tail = &bracketed[close + 1..];
        let (port, explicit_port) = parse_port_tail(tail, default_port)?;
        let host = address.to_string();
        return Ok((host.clone(), format!("[{host}]"), port, explicit_port));
    }

    if authority.contains('[') || authority.contains(']') {
        anyhow::bail!("video download URL contains malformed host brackets");
    }
    if authority.matches(':').count() > 1 {
        anyhow::bail!("video download IPv6 hosts must use square brackets");
    }

    let (raw_host, port, explicit_port) = match authority.rsplit_once(':') {
        Some((host, raw_port)) => {
            if host.is_empty() || raw_port.is_empty() {
                anyhow::bail!("video download URL contains an invalid host or port");
            }
            (host, parse_port(raw_port)?, true)
        }
        None => (authority, default_port, false),
    };

    if let Ok(address) = raw_host.parse::<Ipv4Addr>() {
        let host = address.to_string();
        return Ok((host.clone(), host, port, explicit_port));
    }

    let host = normalize_dns_host(raw_host)?;
    Ok((host.clone(), host, port, explicit_port))
}

fn parse_port_tail(tail: &str, default_port: u16) -> anyhow::Result<(u16, bool)> {
    if tail.is_empty() {
        return Ok((default_port, false));
    }
    let raw_port = tail
        .strip_prefix(':')
        .ok_or_else(|| anyhow::anyhow!("video download URL contains data after the IPv6 host"))?;
    if raw_port.is_empty() {
        anyhow::bail!("video download URL contains an empty port");
    }
    Ok((parse_port(raw_port)?, true))
}

fn parse_port(raw_port: &str) -> anyhow::Result<u16> {
    if !raw_port.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("video download URL port must contain only decimal digits");
    }
    let port = raw_port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("video download URL port is outside 1..=65535"))?;
    if port == 0 {
        anyhow::bail!("video download URL port is outside 1..=65535");
    }
    Ok(port)
}

fn normalize_dns_host(raw_host: &str) -> anyhow::Result<String> {
    let host = raw_host.strip_suffix('.').unwrap_or(raw_host);
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        anyhow::bail!("video download URL contains an invalid DNS host");
    }

    let labels = host.split('.').collect::<Vec<_>>();
    if labels.len() < 2
        || labels
            .iter()
            .any(|label| label.is_empty() || label.len() > 63)
    {
        anyhow::bail!("video download DNS host must be a fully-qualified, valid hostname");
    }
    for label in &labels {
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            anyhow::bail!("video download URL contains an invalid DNS label {label:?}");
        }
    }
    if labels
        .iter()
        .all(|label| looks_numeric_host_component(label))
    {
        anyhow::bail!("video download URL rejects ambiguous non-canonical numeric hosts");
    }

    Ok(host.to_ascii_lowercase())
}

fn looks_numeric_host_component(label: &str) -> bool {
    label.bytes().all(|byte| byte.is_ascii_digit())
        || label
            .strip_prefix("0x")
            .or_else(|| label.strip_prefix("0X"))
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn validate_resolved_addresses(
    host: &str,
    addresses: Vec<SocketAddr>,
) -> anyhow::Result<Vec<SocketAddr>> {
    if addresses.is_empty() {
        anyhow::bail!("video download host {host:?} resolved to no addresses");
    }

    let mut unique = BTreeSet::new();
    for address in addresses {
        if !is_public_download_ip(address.ip()) {
            anyhow::bail!(
                "video download host {host:?} resolved to blocked address {}",
                address.ip()
            );
        }
        unique.insert(address);
    }
    Ok(unique.into_iter().collect())
}

/// Conservative SSRF allow-policy for connection targets.
///
/// IPv4 special-use ranges are rejected explicitly. IPv6 is restricted to
/// ordinary 2000::/3 global unicast and additionally excludes documented,
/// transition, benchmark, and ORCHID ranges. This is intentionally fail-closed
/// if new special-use ranges appear.
pub fn is_public_download_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn ensure_public_ip(address: IpAddr) -> anyhow::Result<()> {
    if !is_public_download_ip(address) {
        anyhow::bail!("video download URL targets blocked address {address}");
    }
    Ok(())
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    if octets[..10].iter().all(|byte| *byte == 0) && octets[10] == 0xff && octets[11] == 0xff {
        return is_public_ipv4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }

    let segments = address.segments();
    if segments[0] & 0xe000 != 0x2000 {
        return false;
    }

    !matches!(
        (segments[0], segments[1]),
        (0x2001, 0x0000) | (0x2001, 0x0002) | (0x2001, 0x0db8) | (0x2002, _)
    ) && !(segments[0] == 0x2001 && matches!(segments[1] & 0xfff0, 0x0010 | 0x0020))
        && !(segments[0] == 0x3fff && segments[1] & 0xf000 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_http_targets_without_browser_style_coercion() {
        let target =
            parse_download_target("HTTPS://Example.COM:443/media/video.mp4?download=1").unwrap();
        assert_eq!(
            target.normalized_url(),
            "https://example.com/media/video.mp4?download=1"
        );
        assert_eq!(target.scheme(), "https");
        assert_eq!(target.host(), "example.com");
        assert_eq!(target.port(), 443);

        let http = parse_download_target("http://example.com:8080/video").unwrap();
        assert_eq!(http.normalized_url(), "http://example.com:8080/video");
    }

    #[test]
    fn rejects_credentials_fragments_and_ambiguous_hosts() {
        for url in [
            "https://user:secret@example.com/video",
            "https://example.com/video#fragment",
            "https://127.0.0.1/video",
            "https://0177.0.0.1/video",
            "https://2130706433/video",
            "https://localhost/video",
            "https://example.com\\@127.0.0.1/video",
            "ftp://example.com/video",
        ] {
            assert!(
                parse_download_target(url).is_err(),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn blocks_special_use_ipv4_ranges() {
        for address in [
            "0.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let address = address.parse::<IpAddr>().unwrap();
            assert!(
                !is_public_download_ip(address),
                "{address} should be blocked"
            );
        }
        assert!(is_public_download_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_download_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn blocks_special_use_and_mapped_ipv6_ranges() {
        for address in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "2002::1",
            "3fff::1",
            "ff02::1",
        ] {
            let address = address.parse::<IpAddr>().unwrap();
            assert!(
                !is_public_download_ip(address),
                "{address} should be blocked"
            );
        }
        assert!(is_public_download_ip(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }

    #[test]
    fn rejects_mixed_public_and_private_dns_answers() {
        let addresses = vec![
            "1.1.1.1:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ];
        let error = validate_resolved_addresses("mixed.example", addresses).unwrap_err();
        assert!(error.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn deduplicates_vetted_dns_answers() {
        let addresses = vec![
            "1.1.1.1:443".parse().unwrap(),
            "8.8.8.8:443".parse().unwrap(),
            "1.1.1.1:443".parse().unwrap(),
        ];
        let vetted = validate_resolved_addresses("public.example", addresses).unwrap();
        assert_eq!(vetted.len(), 2);
    }
}
