//! Local RPX credential storage.
//!
//! Only the scoped RPX bearer token is persisted. UAC identity data and
//! registry owner keys are deliberately not part of this format.

use crate::publisher_client::AccessTokenResponse;
use anyhow::{bail, Context, Result};
use atomic_io::AtomicIo;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const TOKEN_ENV: &str = "RPX_TOKEN";
pub const AUTH_FILE_ENV: &str = "RPX_AUTH_FILE";
const AUTH_FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthFile {
    format: u32,
    #[serde(default)]
    registries: BTreeMap<String, StoredCredential>,
}

impl Default for AuthFile {
    fn default() -> Self {
        Self { format: AUTH_FORMAT, registries: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredCredential {
    pub token_type: String,
    pub access_token: String,
    pub service_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    Store,
}

#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub authorization: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<u64>,
    pub source: CredentialSource,
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn discover() -> Result<Self> {
        if let Ok(path) = std::env::var(AUTH_FILE_ENV) {
            if !path.trim().is_empty() {
                return Ok(Self { path: PathBuf::from(path) });
            }
        }
        let home = user_home().context(
            "could not determine user home directory; set RPX_AUTH_FILE to choose a credential file",
        )?;
        Ok(Self { path: home.join(".rbe").join("rpx").join("auth.json") })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resolve(&self, registry_key: &str) -> Result<Option<ResolvedCredential>> {
        validate_registry_key(registry_key)?;
        if let Ok(token) = std::env::var(TOKEN_ENV) {
            if !token.trim().is_empty() {
                validate_token(&token)?;
                return Ok(Some(ResolvedCredential {
                    authorization: format!("Bearer {token}"),
                    scopes: Vec::new(),
                    expires_at: None,
                    source: CredentialSource::Environment,
                }));
            }
        }

        let file = self.load()?;
        let Some(credential) = file.registries.get(registry_key) else {
            return Ok(None);
        };
        credential.validate()?;
        if credential.expires_at <= unix_now() {
            return Ok(None);
        }
        Ok(Some(ResolvedCredential {
            authorization: format!("Bearer {}", credential.access_token),
            scopes: credential.scopes.clone(),
            expires_at: Some(credential.expires_at),
            source: CredentialSource::Store,
        }))
    }

    pub fn save(&self, registry_key: &str, token: &AccessTokenResponse) -> Result<()> {
        validate_registry_key(registry_key)?;
        let credential = StoredCredential {
            token_type: token.token_type.clone(),
            access_token: token.access_token.clone(),
            service_id: token.service_id.clone(),
            scopes: token.scopes.clone(),
            expires_at: token.expires_at,
        };
        credential.validate()?;
        let mut file = self.load()?;
        file.registries.insert(registry_key.to_owned(), credential);
        self.write(&file)
    }

    pub fn remove(&self, registry_key: &str) -> Result<bool> {
        validate_registry_key(registry_key)?;
        let mut file = self.load()?;
        let removed = file.registries.remove(registry_key).is_some();
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    fn load(&self) -> Result<AuthFile> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthFile::default())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.path.display()))
            }
        };
        let file: AuthFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid RPX credential file {}", self.path.display()))?;
        if file.format != AUTH_FORMAT {
            bail!(
                "unsupported RPX credential file format {}; expected {}",
                file.format,
                AUTH_FORMAT
            );
        }
        for (registry, credential) in &file.registries {
            validate_registry_key(registry)?;
            credential.validate()?;
        }
        Ok(file)
    }

    fn write(&self, file: &AuthFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            restrict_directory(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(file).context("failed to serialize RPX credentials")?;
        AtomicIo::new()
            .write_atomic(&self.path, &bytes)
            .with_context(|| format!("failed to atomically write {}", self.path.display()))?;
        restrict_file(&self.path)?;
        Ok(())
    }
}

impl StoredCredential {
    fn validate(&self) -> Result<()> {
        if self.token_type != "Bearer" || self.service_id != "rpx" {
            bail!("stored RPX credential has an unsupported identity");
        }
        validate_token(&self.access_token)?;
        if self.expires_at == 0 {
            bail!("stored RPX credential has an invalid expiry");
        }
        if self.scopes.len() > 64
            || self
                .scopes
                .iter()
                .any(|scope| scope.is_empty() || scope.len() > 128)
        {
            bail!("stored RPX credential has invalid scopes");
        }
        Ok(())
    }
}

fn validate_registry_key(value: &str) -> Result<()> {
    let url = url::Url::parse(value).context("stored RPX registry key is not a URL")?;
    if value.len() > 4096
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("stored RPX registry key is invalid");
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")) => Ok(()),
        _ => bail!("stored RPX registry key must use HTTPS outside loopback development"),
    }
}

fn validate_token(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid RPX bearer token");
    }
    Ok(())
}

fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(value) = std::env::var_os("USERPROFILE") {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
        match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            (Some(drive), Some(path)) => {
                let mut home = PathBuf::from(drive);
                home.push(path);
                Some(home)
            }
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").filter(|value| !value.is_empty()).map(PathBuf::from)
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to restrict {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_auth_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rpx-auth-{name}-{}-{}.json",
            std::process::id(),
            unix_now()
        ))
    }

    #[test]
    fn credentials_are_registry_scoped_and_owner_free() {
        let path = temp_auth_file("scope");
        let store = CredentialStore::at(&path);
        let token = AccessTokenResponse {
            ok: true,
            token_type: "Bearer".into(),
            access_token: "a".repeat(64),
            service_id: "rpx".into(),
            scopes: vec!["package.publish".into()],
            expires_at: unix_now().saturating_add(60),
        };
        store.save("https://registry.example/", &token).unwrap();
        assert!(store.resolve("https://registry.example/").unwrap().is_some());
        assert!(store.resolve("https://other.example/").unwrap().is_none());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ownerKey"));
        assert!(!text.contains("privateId"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn expired_credentials_are_not_resolved() {
        let path = temp_auth_file("expired");
        let store = CredentialStore::at(&path);
        let token = AccessTokenResponse {
            ok: true,
            token_type: "Bearer".into(),
            access_token: "b".repeat(64),
            service_id: "rpx".into(),
            scopes: vec![],
            expires_at: 1,
        };
        store.save("https://registry.example/", &token).unwrap();
        assert!(store.resolve("https://registry.example/").unwrap().is_none());
        let _ = fs::remove_file(path);
    }
}
