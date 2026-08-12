//! Extracts the real client IP from proxy/CDN headers, and normalizes
//! it to a consistent string key so bans/rate-limits apply the same
//! way regardless of whether a request arrived with an IPv4 address,
//! an IPv4-mapped IPv6 address (`::ffff:1.2.3.4`), or a native IPv6
//! address — matches the Node backend's stated reasoning (bans should
//! apply universally across network types, not be bypassable by
//! switching which address family a header reports).

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;

/// Headers checked, in trust-priority order, before falling back to
/// the raw TCP peer address. Ordered roughly most-specific/least-
/// spoofable-in-practice first:
///
/// - `cf-connecting-ipv6` / `cf-connecting-ip` — Cloudflare
/// - `true-client-ip` — Cloudflare Enterprise, Akamai
/// - `x-azure-clientip` — Azure Front Door / App Service
/// - `fastly-client-ip` — Fastly
/// - `x-real-ip` — common nginx reverse-proxy convention
/// - `forwarded` — RFC 7239 standard (`for=...`), parsed below
/// - `x-forwarded-for` — most generic/common, checked last because
///   it's the easiest for a client to pad with fake entries; only the
///   first (leftmost/original-client) hop is used, never the last
const SIMPLE_IP_HEADERS: &[&str] = &[
    "cf-connecting-ipv6",
    "cf-connecting-ip",
    "true-client-ip",
    "x-azure-clientip",
    "fastly-client-ip",
    "x-real-ip",
];

/// Real client IP. **Only trust these headers when
/// `security.trusted_proxy_headers` is true** — an internet-facing
/// deployment with this on but no actual trusted proxy in front of it
/// lets any client spoof its own ban/rate-limit identity by setting
/// these headers itself. Callers are responsible for checking that
/// config flag before calling this; it isn't checked here since a
/// middleware layer already needs it for other reasons.
pub fn extract_real_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy_headers: bool) -> IpAddr {
    if trust_proxy_headers {
        for header_name in SIMPLE_IP_HEADERS {
            if let Some(ip) = header_ip(headers, header_name) {
                return ip;
            }
        }

        if let Some(value) = headers.get("forwarded").and_then(|v| v.to_str().ok()) {
            if let Some(ip) = parse_forwarded(value) {
                return ip;
            }
        }

        if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = forwarded.split(',').next() {
                if let Some(ip) = parse_possibly_bracketed_host(first.trim()) {
                    return ip;
                }
            }
        }
    }

    peer.ip()
}

fn header_ip(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_possibly_bracketed_host(s.trim()))
}

/// Parses a value that might be a bare IP (`1.2.3.4`), an IP with a
/// port (`1.2.3.4:8080`), or a bracketed IPv6 with a port
/// (`[2001:db8::1]:8080`) — the forms these headers show up in across
/// different providers/proxies.
fn parse_possibly_bracketed_host(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_matches('"');

    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6, optionally with a trailing :port after the ']'.
        let end = rest.find(']')?;
        return rest[..end].parse().ok();
    }

    // Bare IP (v4 or v6, no brackets) first...
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }

    // ...otherwise assume `ip:port` (IPv4 case — bracketless IPv6 with
    // a port is ambiguous with the address itself, so only strip a
    // trailing port segment when what's left of the last ':' parses).
    if let Some((host, _port)) = s.rsplit_once(':') {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Some(ip);
        }
    }

    None
}

/// Minimal RFC 7239 `Forwarded` header parser — extracts the `for=`
/// parameter from the first entry (the original client, per the
/// spec's ordering) and parses it as an IP. Obfuscated identifiers
/// (`for=unknown`, `for=_hidden`) intentionally return `None` rather
/// than being treated as an IP.
fn parse_forwarded(value: &str) -> Option<IpAddr> {
    let first_entry = value.split(',').next()?;
    for param in first_entry.split(';') {
        let (key, val) = param.trim().split_once('=')?;
        if key.trim().eq_ignore_ascii_case("for") {
            return parse_possibly_bracketed_host(val.trim());
        }
    }
    None
}

/// Normalizes an IP to a consistent string key: IPv4 addresses become
/// their IPv4-mapped IPv6 form (`::ffff:a.b.c.d`), so `1.2.3.4` and a
/// client that happens to present as `::ffff:1.2.3.4` hash to the same
/// bucket for rate-limiting/ban purposes.
pub fn normalize_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().to_string(),
        IpAddr::V6(v6) => v6.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_and_mapped_v6_normalize_to_the_same_key() {
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        let mapped_v6: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        assert_eq!(normalize_key(v4), normalize_key(mapped_v6));
    }

    #[test]
    fn parses_bare_and_ported_ipv4() {
        assert_eq!(
            parse_possibly_bracketed_host("1.2.3.4"),
            Some("1.2.3.4".parse().unwrap())
        );
        assert_eq!(
            parse_possibly_bracketed_host("1.2.3.4:8080"),
            Some("1.2.3.4".parse().unwrap())
        );
    }

    #[test]
    fn parses_bracketed_ipv6_with_port() {
        assert_eq!(
            parse_possibly_bracketed_host("[2001:db8::1]:8080"),
            Some("2001:db8::1".parse().unwrap())
        );
    }

    #[test]
    fn parses_rfc7239_forwarded_header() {
        assert_eq!(
            parse_forwarded("for=192.0.2.60;proto=http;by=203.0.113.43"),
            Some("192.0.2.60".parse().unwrap())
        );
        assert_eq!(
            parse_forwarded("for=\"[2001:db8:cafe::17]:4711\""),
            Some("2001:db8:cafe::17".parse().unwrap())
        );
    }

    #[test]
    fn obfuscated_forwarded_identifier_is_not_an_ip() {
        assert_eq!(parse_forwarded("for=unknown"), None);
        assert_eq!(parse_forwarded("for=_hidden"), None);
    }
}
