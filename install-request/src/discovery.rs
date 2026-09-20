use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::{
    InstallRequestError, KASTRICK_EXTERNAL_LOOKUP_PATH, KASTRICK_INDEX_OBSERVATION_PATH,
    RBE_INDEX_LINK_REL, RBE_ROOT_INDEX, RBE_SYSTEM_PYTHON, RBE_WELL_KNOWN_INDEX,
    SYSTEM_PYTHON_SCRAPER_ID,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLocator {
    pub original: String,
    pub requested: Url,
    pub origin: Url,
    pub public_identity: String,
    pub upgraded_from_http: bool,
    pub inferred_scheme: bool,
}

impl ExternalLocator {
    pub fn parse(input: &str) -> Result<Self, InstallRequestError> {
        let original = input.trim();
        if original.is_empty() || original.len() > 4096 {
            return Err(InstallRequestError::InvalidExternalLocator(
                original.to_string(),
            ));
        }
        let (candidate, inferred_scheme) = if original.contains("://") {
            (original.to_string(), false)
        } else {
            (format!("https://{original}"), true)
        };
        let mut requested =
            Url::parse(&candidate).map_err(|source| InstallRequestError::InvalidExternalUrl {
                value: original.to_string(),
                source,
            })?;
        if !matches!(requested.scheme(), "http" | "https")
            || requested.host_str().is_none()
            || !requested.username().is_empty()
            || requested.password().is_some()
        {
            return Err(InstallRequestError::InvalidExternalLocator(
                original.to_string(),
            ));
        }
        requested.set_fragment(None);
        let upgraded_from_http = requested.scheme() == "http";
        if upgraded_from_http {
            requested
                .set_scheme("https")
                .map_err(|_| InstallRequestError::InvalidExternalLocator(original.to_string()))?;
        }
        if requested.path().is_empty() {
            requested.set_path("/");
        }
        let mut origin = requested.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        let public_identity = match requested.query() {
            Some(query) => format!(
                "{}{}?{}",
                requested.host_str().unwrap_or_default(),
                requested.path(),
                query
            ),
            None => format!(
                "{}{}",
                requested.host_str().unwrap_or_default(),
                requested.path()
            ),
        };
        Ok(Self {
            original: original.to_string(),
            requested,
            origin,
            public_identity,
            upgraded_from_http,
            inferred_scheme,
        })
    }

    pub fn is_public(&self) -> bool {
        is_public_host(&self.requested)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIndexDiscoveryPlan {
    pub cache_path: PathBuf,
    pub website_candidates: Vec<Url>,
    pub html_probe: Url,
    pub html_link_rel: &'static str,
    pub kastrick_lookup: Url,
    pub scraper: PythonScrapePlan,
    pub observation: Option<IndexObservationPlan>,
}

impl ExternalIndexDiscoveryPlan {
    pub fn new(
        locator: &ExternalLocator,
        cache_root: &Path,
        kastrick_registry: &str,
    ) -> Result<Self, InstallRequestError> {
        if cache_root.as_os_str().is_empty() {
            return Err(InstallRequestError::InvalidCacheRoot);
        }
        let kastrick_registry = parse_https_base(kastrick_registry)?;
        let cache_key = sha256_hex(locator.requested.as_str().as_bytes());
        let cache_path = cache_root
            .join("external")
            .join(format!("{cache_key}.json"));
        let mut candidates = Vec::new();
        if locator.requested.path().ends_with('/') {
            candidates.push(
                locator
                    .requested
                    .join("rbe-index.json")
                    .map_err(InstallRequestError::JoinUrl)?,
            );
        }
        candidates.push(
            locator
                .origin
                .join(RBE_WELL_KNOWN_INDEX.trim_start_matches('/'))
                .map_err(InstallRequestError::JoinUrl)?,
        );
        candidates.push(
            locator
                .origin
                .join(RBE_ROOT_INDEX.trim_start_matches('/'))
                .map_err(InstallRequestError::JoinUrl)?,
        );
        dedup_urls(&mut candidates);
        let kastrick_lookup = kastrick_registry
            .join(KASTRICK_EXTERNAL_LOOKUP_PATH.trim_start_matches('/'))
            .map_err(InstallRequestError::JoinUrl)?;
        let observation = if locator.is_public() {
            Some(IndexObservationPlan {
                endpoint: kastrick_registry
                    .join(KASTRICK_INDEX_OBSERVATION_PATH.trim_start_matches('/'))
                    .map_err(InstallRequestError::JoinUrl)?,
                mode: ObservationMode::BestEffortPublicOnly,
                public_identity: locator.public_identity.clone(),
            })
        } else {
            None
        };
        Ok(Self {
            cache_path,
            website_candidates: candidates,
            html_probe: locator.requested.clone(),
            html_link_rel: RBE_INDEX_LINK_REL,
            kastrick_lookup,
            scraper: PythonScrapePlan::for_locator(locator),
            observation,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonScrapePlan {
    pub runtime_key: &'static str,
    pub script_id: &'static str,
    pub target: Url,
    pub allowed_origin: Url,
    pub timeout_seconds: u32,
    pub max_output_bytes: u64,
}

impl PythonScrapePlan {
    fn for_locator(locator: &ExternalLocator) -> Self {
        Self {
            runtime_key: RBE_SYSTEM_PYTHON,
            script_id: SYSTEM_PYTHON_SCRAPER_ID,
            target: locator.requested.clone(),
            allowed_origin: locator.origin.clone(),
            timeout_seconds: 30,
            max_output_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationMode {
    BestEffortPublicOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexObservationPlan {
    pub endpoint: Url,
    pub mode: ObservationMode,
    pub public_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrapedIndexCandidate {
    pub source_page: String,
    pub index_url: String,
    #[serde(default)]
    pub package_hint: Option<String>,
}

impl ScrapedIndexCandidate {
    pub fn validate_for(&self, plan: &PythonScrapePlan) -> Result<Url, InstallRequestError> {
        let source_page = ExternalLocator::parse(&self.source_page)?;
        if source_page.origin != plan.allowed_origin {
            return Err(InstallRequestError::ScraperOriginEscape);
        }
        let index = ExternalLocator::parse(&self.index_url)?;
        if index.origin != plan.allowed_origin {
            return Err(InstallRequestError::ScraperOriginEscape);
        }
        Ok(index.requested)
    }
}

fn parse_https_base(value: &str) -> Result<Url, InstallRequestError> {
    let mut url = Url::parse(value).map_err(|source| InstallRequestError::InvalidExternalUrl {
        value: value.to_string(),
        source,
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(InstallRequestError::HttpsRequired(value.to_string()));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn dedup_urls(values: &mut Vec<Url>) {
    let mut seen = BTreeSet::new();
    values.retain(|url| seen.insert(url.as_str().to_string()));
}

fn is_public_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => {
            let lower = domain.to_ascii_lowercase();
            !matches!(lower.as_str(), "localhost")
                && !lower.ends_with(".localhost")
                && !lower.ends_with(".local")
                && !lower.ends_with(".internal")
        }
        Some(Host::Ipv4(ip)) => {
            let address = IpAddr::V4(ip);
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !ip.is_private()
                && !ip.is_link_local()
        }
        Some(Host::Ipv6(ip)) => {
            let address = IpAddr::V6(ip);
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && (ip.segments()[0] & 0xfe00) != 0xfc00
                && (ip.segments()[0] & 0xffc0) != 0xfe80
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_http_urls_are_normalized_to_https() {
        let bare = ExternalLocator::parse("python.org/downloads/").unwrap();
        assert_eq!(bare.requested.as_str(), "https://python.org/downloads/");
        assert!(bare.inferred_scheme);
        let http = ExternalLocator::parse("http://python.org/downloads/").unwrap();
        assert_eq!(http.requested.scheme(), "https");
        assert!(http.upgraded_from_http);
    }

    #[test]
    fn discovery_uses_cache_kastrick_and_system_python() {
        let locator = ExternalLocator::parse("mycooldevwebsite.here/advancenet/download").unwrap();
        let plan = ExternalIndexDiscoveryPlan::new(
            &locator,
            Path::new("/work/project/.rbe/registry"),
            "https://registry.kastrick.invalid/",
        )
        .unwrap();
        assert!(plan.cache_path.extension().is_some_and(|ext| ext == "json"));
        assert!(plan
            .website_candidates
            .iter()
            .any(|url| url.path() == RBE_WELL_KNOWN_INDEX));
        assert_eq!(plan.scraper.runtime_key, RBE_SYSTEM_PYTHON);
        assert!(plan.observation.is_some());
    }

    #[test]
    fn private_urls_are_not_silently_observed() {
        let locator = ExternalLocator::parse("127.0.0.1:8080/download").unwrap();
        let plan = ExternalIndexDiscoveryPlan::new(
            &locator,
            Path::new("/work/project/.rbe/registry"),
            "https://registry.kastrick.invalid/",
        )
        .unwrap();
        assert!(plan.observation.is_none());
    }

    #[test]
    fn scraper_candidate_cannot_escape_origin() {
        let locator = ExternalLocator::parse("example.com/download").unwrap();
        let plan = PythonScrapePlan::for_locator(&locator);
        let candidate = ScrapedIndexCandidate {
            source_page: "example.com/download".into(),
            index_url: "evil.example/rbe-index.json".into(),
            package_hint: None,
        };
        assert!(matches!(
            candidate.validate_for(&plan),
            Err(InstallRequestError::ScraperOriginEscape)
        ));
    }
}
