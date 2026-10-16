use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct BackendOidVault {
    client: Arc<vault_process::VaultClient>,
    namespace: String,
}

impl BackendOidVault {
    pub fn new(
        client: Arc<vault_process::VaultClient>,
        project_root: &Path,
    ) -> anyhow::Result<Self> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        Ok(Self {
            client,
            namespace: format!("oid-{}", hex::encode(digest)),
        })
    }
}

impl route_engine::OidVaultAuthority for BackendOidVault {
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
        self.client
            .seal_internal(&self.namespace, aad, plaintext)
            .map_err(|error| format!("{error:#}"))
    }

    fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
        self.client
            .open_internal(&self.namespace, aad, sealed)
            .map_err(|error| format!("{error:#}"))
    }

    fn trusted_head(&self) -> Result<Option<String>, String> {
        self.client
            .internal_head(&self.namespace)
            .map_err(|error| format!("{error:#}"))
    }

    fn set_trusted_head(&self, head: &str) -> Result<(), String> {
        self.client
            .set_internal_head(&self.namespace, head)
            .map_err(|error| format!("{error:#}"))
    }
}
