//! Trusted DNS broker for external RBE libraries.
//!
//! Libraries receive DNS records, never resolver handles or sockets. Queries are
//! forced to fully-qualified public-looking domain names so the host resolver's
//! local search suffixes cannot accidentally expose internal names. Address
//! lookups fail closed if any returned address is not publicly routable.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use hickory_resolver::proto::rr::RData;
use hickory_resolver::Resolver;
use serde_json::{json, Value};

pub const PUBLIC_DNS_TARGET: &str = "net:dns";
pub const PUBLIC_DNS_MAX_RESULTS: usize = 32;
pub const PUBLIC_DNS_MAX_QUERY_BYTES: usize = 4 * 1024;
const PUBLIC_DNS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicDnsError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for PublicDnsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicDnsError {}

fn dns_error(message: impl Into<String>) -> PublicDnsError {
    PublicDnsError {
        code: "DNS3000",
        message: message.into(),
    }
}

pub async fn call_public_dns(operation: &str, payload: &[u8]) -> Result<Value, PublicDnsError> {
    if payload.len() > PUBLIC_DNS_MAX_QUERY_BYTES {
        return Err(dns_error("DNS request exceeds 4 KiB"));
    }
    let name = parse_query_name(payload)?;
    let resolver = Resolver::builder_tokio()
        .map_err(|error| dns_error(format!("initialize system DNS resolver: {error}")))?
        .build()
        .map_err(|error| dns_error(format!("build system DNS resolver: {error}")))?;

    match operation {
        "lookup" | "ip" => {
            let lookup = tokio::time::timeout(PUBLIC_DNS_TIMEOUT, resolver.lookup_ip(&name))
                .await
                .map_err(|_| dns_error("DNS address lookup timed out"))?
                .map_err(|error| dns_error(format!("DNS address lookup failed: {error}")))?;
            let mut addresses = lookup.iter().collect::<Vec<_>>();
            addresses.sort_unstable();
            addresses.dedup();
            if addresses.is_empty() {
                return Err(dns_error("DNS address lookup returned no addresses"));
            }
            if addresses.len() > PUBLIC_DNS_MAX_RESULTS {
                return Err(dns_error("DNS address lookup returned too many addresses"));
            }
            if addresses.iter().copied().any(forbidden_ip) {
                return Err(dns_error(
                    "DNS hostname resolves to a non-public address; result rejected by RBE",
                ));
            }
            Ok(json!({
                "name": display_name(&name),
                "addresses": addresses.into_iter().map(|ip| ip.to_string()).collect::<Vec<_>>()
            }))
        }
        "mx" => {
            let lookup = tokio::time::timeout(PUBLIC_DNS_TIMEOUT, resolver.mx_lookup(&name))
                .await
                .map_err(|_| dns_error("DNS MX lookup timed out"))?
                .map_err(|error| dns_error(format!("DNS MX lookup failed: {error}")))?;
            let mut records = lookup
                .answers()
                .iter()
                .filter_map(|record| match record.data() {
                    RData::MX(mx) => Some((mx.preference, mx.exchange.to_string())),
                    _ => None,
                })
                .map(|(preference, exchange)| {
                    let exchange = if exchange == "." {
                        exchange
                    } else {
                        display_name(&normalize_dns_name(&exchange)?)
                    };
                    Ok((preference, exchange))
                })
                .collect::<Result<Vec<_>, PublicDnsError>>()?;
            records.sort_unstable();
            records.dedup();
            if records.is_empty() {
                return Err(dns_error("DNS MX lookup returned no MX records"));
            }
            if records.len() > PUBLIC_DNS_MAX_RESULTS {
                return Err(dns_error("DNS MX lookup returned too many records"));
            }
            Ok(json!({
                "name": display_name(&name),
                "records": records
                    .into_iter()
                    .map(|(preference, exchange)| json!({
                        "preference": preference,
                        "exchange": exchange
                    }))
                    .collect::<Vec<_>>()
            }))
        }
        other => Err(dns_error(format!("dns.{other}() does not exist"))),
    }
}

fn parse_query_name(payload: &[u8]) -> Result<String, PublicDnsError> {
    if payload.is_empty() {
        return Err(dns_error("DNS query name is required"));
    }
    if let Ok(value) = serde_json::from_slice::<Value>(payload) {
        let candidate = match value {
            Value::String(value) => Some(value),
            Value::Array(values) => values.first().and_then(Value::as_str).map(str::to_string),
            Value::Object(values) => values
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        if let Some(candidate) = candidate {
            return normalize_dns_name(&candidate);
        }
    }
    let candidate = std::str::from_utf8(payload)
        .map_err(|_| dns_error("DNS query name must be UTF-8 text or JSON"))?;
    normalize_dns_name(candidate)
}

fn normalize_dns_name(value: &str) -> Result<String, PublicDnsError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 253 {
        return Err(dns_error("DNS query name has an invalid length"));
    }
    if value.chars().any(char::is_control) {
        return Err(dns_error("DNS query name contains control characters"));
    }
    let bare = value.trim_end_matches('.');
    if bare.is_empty() || !bare.contains('.') {
        return Err(dns_error(
            "DNS package queries must use a fully-qualified public domain name",
        ));
    }
    let lower = bare.to_ascii_lowercase();
    if matches!(lower.as_str(), "localhost" | "localhost.localdomain")
        || [".local", ".localhost", ".internal", ".home", ".lan"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    {
        return Err(dns_error("DNS package query targets a local-only domain"));
    }
    for label in bare.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(dns_error("DNS query name contains an invalid label"));
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_alphanumeric()
            || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err(dns_error("DNS query name contains an invalid label"));
        }
    }
    Ok(format!("{lower}."))
}

fn display_name(value: &str) -> String {
    value.trim_end_matches('.').to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_names_are_forced_to_fqdn_form() {
        assert_eq!(normalize_dns_name("GMAIL.COM").unwrap(), "gmail.com.");
        assert_eq!(
            normalize_dns_name("mx.example.com.").unwrap(),
            "mx.example.com."
        );
    }

    #[test]
    fn local_and_single_label_names_fail_closed() {
        for value in [
            "localhost",
            "printer",
            "smtp.local",
            "mx.internal",
            "mail.home",
        ] {
            assert!(
                normalize_dns_name(value).is_err(),
                "{value} should be rejected"
            );
        }
    }

    #[test]
    fn malformed_labels_fail_closed() {
        for value in [
            "-mail.example",
            "mail-.example",
            "mail..example",
            "mail_1.example",
        ] {
            assert!(
                normalize_dns_name(value).is_err(),
                "{value} should be rejected"
            );
        }
    }

    #[test]
    fn private_and_documentation_addresses_are_forbidden() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(
                forbidden_ip(ip.parse().unwrap()),
                "{ip} should be forbidden"
            );
        }
        assert!(!forbidden_ip("1.1.1.1".parse().unwrap()));
        assert!(!forbidden_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn payload_accepts_raw_and_json_name_forms() {
        assert_eq!(parse_query_name(b"gmail.com").unwrap(), "gmail.com.");
        assert_eq!(parse_query_name(br#""gmail.com""#).unwrap(), "gmail.com.");
        assert_eq!(
            parse_query_name(br#"{"name":"gmail.com"}"#).unwrap(),
            "gmail.com."
        );
        assert_eq!(parse_query_name(br#"["gmail.com"]"#).unwrap(), "gmail.com.");
    }
}
