use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use rbe_install_request::{RegistryPackageIndex, RegistryPackageIndexRequest};
use rbe_library_registry::add_registry_index;
use rbe_library_resolver::PackageCatalog;
use url::Url;

use crate::http::{get_following_redirects, validate_remote_url};
use crate::InstallRuntimeError;

pub const DEFAULT_MAX_REGISTRY_INDEX_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REGISTRY_INDEX_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES: usize = 1024;

#[derive(Debug, Clone)]
pub struct RegistryClient {
    base: Url,
    max_index_bytes: usize,
    max_graph_packages: usize,
    connect_timeout_seconds: u64,
    idle_timeout_seconds: u64,
    maximum_redirects: u8,
}

impl RegistryClient {
    pub fn new(registry_base: &str) -> Result<Self, InstallRuntimeError> {
        let base = Url::parse(registry_base).map_err(|source| InstallRuntimeError::InvalidUrl {
            value: registry_base.to_string(),
            source,
        })?;
        validate_remote_url(&base)?;
        Ok(Self {
            base,
            max_index_bytes: DEFAULT_MAX_REGISTRY_INDEX_BYTES,
            max_graph_packages: DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES,
            connect_timeout_seconds: 10,
            idle_timeout_seconds: 30,
            maximum_redirects: 5,
        })
    }

    pub fn with_index_limit(mut self, maximum_bytes: usize) -> Result<Self, InstallRuntimeError> {
        if maximum_bytes == 0 || maximum_bytes > MAX_REGISTRY_INDEX_BYTES {
            return Err(InstallRuntimeError::InvalidRegistryIndexLimit {
                requested: maximum_bytes,
                maximum: MAX_REGISTRY_INDEX_BYTES,
            });
        }
        self.max_index_bytes = maximum_bytes;
        Ok(self)
    }

    pub fn with_graph_limit(mut self, maximum_packages: usize) -> Result<Self, InstallRuntimeError> {
        if maximum_packages == 0 || maximum_packages > DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES {
            return Err(InstallRuntimeError::InvalidRegistryGraphLimit {
                requested: maximum_packages,
                maximum: DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES,
            });
        }
        self.max_graph_packages = maximum_packages;
        Ok(self)
    }

    pub async fn fetch_index(
        &self,
        package: &str,
    ) -> Result<RegistryPackageIndex, InstallRuntimeError> {
        let request = RegistryPackageIndexRequest::new(self.base.as_str(), package)?;
        let mut response = get_following_redirects(
            request.endpoint,
            None,
            self.connect_timeout_seconds,
            self.idle_timeout_seconds,
            self.maximum_redirects,
        )
        .await?;
        if !response.status().is_success() {
            return Err(InstallRuntimeError::HttpStatus(response.status().as_u16()));
        }
        if let Some(length) = response.content_length() {
            if length > self.max_index_bytes as u64 {
                return Err(InstallRuntimeError::RegistryIndexTooLarge {
                    limit: self.max_index_bytes,
                    observed: length.min(usize::MAX as u64) as usize,
                });
            }
        }

        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_secs(self.idle_timeout_seconds.max(1)),
                response.chunk(),
            )
            .await
            .map_err(|_| InstallRuntimeError::IdleTimeout(self.idle_timeout_seconds))?
            .map_err(InstallRuntimeError::HttpBody)?;
            let Some(chunk) = chunk else {
                break;
            };
            let next = bytes.len().saturating_add(chunk.len());
            if next > self.max_index_bytes {
                return Err(InstallRuntimeError::RegistryIndexTooLarge {
                    limit: self.max_index_bytes,
                    observed: next,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        let text = String::from_utf8(bytes).map_err(|_| InstallRuntimeError::RegistryIndexUtf8)?;
        RegistryPackageIndex::parse_json(&text, package).map_err(Into::into)
    }

    /// Fetch the root package index plus every dependency package named by any
    /// fetched release. Transport expansion is bounded independently from the
    /// resolver so a registry cannot force an unbounded metadata crawl.
    pub async fn hydrate_catalog(
        &self,
        root_package: &str,
    ) -> Result<(PackageCatalog, BTreeMap<String, RegistryPackageIndex>), InstallRuntimeError> {
        let mut catalog = PackageCatalog::default();
        let mut indexes = BTreeMap::new();
        let mut queued = BTreeSet::new();
        let mut queue = VecDeque::from([root_package.to_string()]);
        queued.insert(root_package.to_string());

        while let Some(package) = queue.pop_front() {
            if indexes.len() >= self.max_graph_packages {
                return Err(InstallRuntimeError::RegistryGraphTooLarge {
                    maximum: self.max_graph_packages,
                });
            }
            let index = self.fetch_index(&package).await?;
            for release in &index.releases {
                for dependency in release.dependencies.keys() {
                    if !indexes.contains_key(dependency) && queued.insert(dependency.clone()) {
                        queue.push_back(dependency.clone());
                    }
                }
            }
            add_registry_index(&mut catalog, &package, &index)?;
            indexes.insert(package, index);
        }

        Ok((catalog, indexes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_client_requires_hardened_https_base() {
        assert!(RegistryClient::new("https://registry.example.com/api/").is_ok());
        assert!(RegistryClient::new("http://registry.example.com/api/").is_err());
        assert!(RegistryClient::new("https://user:pass@registry.example.com/").is_err());
    }

    #[test]
    fn registry_limits_are_bounded() {
        assert!(RegistryClient::new("https://registry.example.com/")
            .unwrap()
            .with_index_limit(0)
            .is_err());
        assert!(RegistryClient::new("https://registry.example.com/")
            .unwrap()
            .with_index_limit(MAX_REGISTRY_INDEX_BYTES + 1)
            .is_err());
        assert!(RegistryClient::new("https://registry.example.com/")
            .unwrap()
            .with_graph_limit(DEFAULT_MAX_REGISTRY_GRAPH_PACKAGES + 1)
            .is_err());
    }
}
