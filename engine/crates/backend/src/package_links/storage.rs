use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context};
use atomic_io::AtomicIo;
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibraryHostCallReply, LibrarySessionBinding,
    MAX_LIBRARY_PAYLOAD_BYTES,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

pub const CAPABILITY: &str = "storage";
const STORAGE_ROOT_DIR: &str = "package-storage";
const MAX_KEY_BYTES: usize = 512;
const MAX_KEY_COMPONENT_BYTES: usize = 128;
const MAX_KEY_COMPONENTS: usize = 32;
const MAX_OBJECT_BYTES: usize = 384 * 1024;
const MAX_LIST_RESULTS: usize = 1024;

static PROJECT_STORAGE_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRequest {
    key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRequest {
    key: String,
    data_hex: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub fn grant() -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        CAPABILITY,
        CAPABILITY,
        [
            "get".to_string(),
            "put".to_string(),
            "delete".to_string(),
            "exists".to_string(),
            "list".to_string(),
        ],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package storage capability grant")
}

pub fn configure_project_root(project_root: &Path) -> anyhow::Result<()> {
    let canonical = fs::canonicalize(project_root).with_context(|| {
        format!(
            "canonicalize project root before configuring package storage: {}",
            project_root.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&canonical).with_context(|| {
        format!("inspect canonical project root: {}", canonical.display())
    })?;
    if !metadata.is_dir() {
        bail!("package storage project root is not a directory");
    }

    let rbe = canonical.join(".rbe");
    ensure_directory(&rbe)?;
    let root = rbe.join(STORAGE_ROOT_DIR);
    ensure_directory(&root)?;

    let slot = PROJECT_STORAGE_ROOT.get_or_init(|| Mutex::new(None));
    let mut active = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("package storage root lock is poisoned"))?;
    *active = Some(root);
    Ok(())
}

pub fn dispatch_authorized_call(
    package: &str,
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<LibraryHostCallReply> {
    let grant = binding
        .authorize_host_call(call)
        .context("authorize package storage call against accepted Library Host session")?;
    if call.capability != CAPABILITY || call.target != CAPABILITY {
        bail!("package storage call does not match admitted storage authority");
    }

    let payload = match call.operation.as_str() {
        "get" => get(package, &call.payload)?,
        "put" => put(package, &call.payload)?,
        "delete" => delete(package, &call.payload)?,
        "exists" => exists(package, &call.payload)?,
        "list" => list(package, &call.payload)?,
        other => bail!("unsupported package storage operation {other:?}"),
    };
    if payload.len() > grant.max_response_bytes {
        bail!(
            "package storage response exceeded admitted capability limit: limit={}, observed={}",
            grant.max_response_bytes,
            payload.len()
        );
    }
    LibraryHostCallReply::success(call.call_id, payload)
        .context("encode successful package storage host-call reply")
}

fn get(package: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: KeyRequest =
        serde_json::from_slice(payload).context("decode package storage get request")?;
    let path = object_path(package, &request.key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("package storage object is not a regular file");
            }
            if metadata.len() > MAX_OBJECT_BYTES as u64 {
                bail!("package storage object exceeds maximum readable size");
            }
            let data = fs::read(&path)
                .with_context(|| format!("read package storage object: {}", path.display()))?;
            serde_json::to_vec(&json!({
                "found": true,
                "key": request.key,
                "data_hex": hex::encode(data),
            }))
            .context("encode package storage get response")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_vec(&json!({ "found": false, "key": request.key }))
                .context("encode missing package storage get response")
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect package storage object: {}", path.display())),
    }
}

fn put(package: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: PutRequest =
        serde_json::from_slice(payload).context("decode package storage put request")?;
    if request.data_hex.len() > MAX_OBJECT_BYTES * 2 {
        bail!("package storage object exceeds maximum size");
    }
    let data = hex::decode(&request.data_hex)
        .context("package storage data_hex must contain valid hexadecimal bytes")?;
    if data.len() > MAX_OBJECT_BYTES {
        bail!("package storage object exceeds maximum size");
    }

    let path = object_path(package, &request.key)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("package storage object has no parent directory"))?;
    ensure_directory_tree(parent, &package_root(package)?)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("package storage destination is not a regular file");
        }
    }

    AtomicIo::new()
        .write_atomic(&path, &data)
        .with_context(|| format!("atomically write package storage object: {}", path.display()))?;
    serde_json::to_vec(&json!({
        "key": request.key,
        "bytes": data.len(),
    }))
    .context("encode package storage put response")
}

fn delete(package: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: KeyRequest =
        serde_json::from_slice(payload).context("decode package storage delete request")?;
    let path = object_path(package, &request.key)?;
    let deleted = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("package storage delete target is not a regular file");
            }
            fs::remove_file(&path)
                .with_context(|| format!("delete package storage object: {}", path.display()))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect package storage object: {}", path.display()))
        }
    };
    serde_json::to_vec(&json!({ "key": request.key, "deleted": deleted }))
        .context("encode package storage delete response")
}

fn exists(package: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: KeyRequest =
        serde_json::from_slice(payload).context("decode package storage exists request")?;
    let path = object_path(package, &request.key)?;
    let exists = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("package storage object is not a regular file")
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect package storage object: {}", path.display()))
        }
    };
    serde_json::to_vec(&json!({ "key": request.key, "exists": exists }))
        .context("encode package storage exists response")
}

fn list(package: &str, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: ListRequest = if payload.is_empty() {
        ListRequest {
            prefix: None,
            limit: None,
        }
    } else {
        serde_json::from_slice(payload).context("decode package storage list request")?
    };
    let limit = request.limit.unwrap_or(256);
    if limit == 0 || limit > MAX_LIST_RESULTS {
        bail!("package storage list limit must be in 1..={MAX_LIST_RESULTS}");
    }

    let package_root = package_root(package)?;
    let prefix = match request.prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => Some(validate_key(prefix)?),
        _ => None,
    };
    let start = prefix
        .as_ref()
        .map(|components| components.iter().fold(package_root.clone(), |path, part| path.join(part)))
        .unwrap_or_else(|| package_root.clone());

    let mut keys = Vec::new();
    if start.exists() {
        walk_files(&package_root, &start, &mut keys, limit)?;
    }
    keys.sort();
    keys.truncate(limit);
    serde_json::to_vec(&json!({ "keys": keys }))
        .context("encode package storage list response")
}

fn storage_root() -> anyhow::Result<PathBuf> {
    let slot = PROJECT_STORAGE_ROOT
        .get()
        .ok_or_else(|| anyhow::anyhow!("package storage project root is not configured"))?;
    let active = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("package storage root lock is poisoned"))?;
    active
        .clone()
        .ok_or_else(|| anyhow::anyhow!("package storage project root is not configured"))
}

fn package_root(package: &str) -> anyhow::Result<PathBuf> {
    if package.is_empty() || package.len() > 192 || package.chars().any(char::is_control) {
        bail!("invalid package identity for package storage");
    }
    let root = storage_root()?;
    ensure_directory(&root)?;
    let identity = hex::encode(Sha256::digest(package.as_bytes()));
    let package_root = root.join(identity);
    ensure_directory(&package_root)?;
    Ok(package_root)
}

fn object_path(package: &str, key: &str) -> anyhow::Result<PathBuf> {
    let components = validate_key(key)?;
    let root = package_root(package)?;
    Ok(components.iter().fold(root, |path, part| path.join(part)))
}

fn validate_key(key: &str) -> anyhow::Result<Vec<String>> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\0') || key.starts_with('/') {
        bail!("package storage key is invalid");
    }
    if key.contains('\\') || key.contains(':') {
        bail!("package storage key contains a forbidden path separator or drive marker");
    }
    let parts = key.split('/').collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > MAX_KEY_COMPONENTS {
        bail!("package storage key has too many path components");
    }
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > MAX_KEY_COMPONENT_BYTES
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("package storage key contains an invalid path component");
        }
        result.push(part.to_string());
    }
    Ok(result)
}

fn ensure_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("package storage directory is not a regular directory: {}", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("create package storage directory: {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect package storage directory: {}", path.display()))
        }
    }
    Ok(())
}

fn ensure_directory_tree(path: &Path, package_root: &Path) -> anyhow::Result<()> {
    let relative = path
        .strip_prefix(package_root)
        .context("package storage directory escaped package root")?;
    let mut current = package_root.to_path_buf();
    ensure_directory(&current)?;
    for component in relative.components() {
        let part = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("package storage path is not UTF-8"))?;
        current.push(part);
        ensure_directory(&current)?;
    }
    Ok(())
}

fn walk_files(root: &Path, current: &Path, keys: &mut Vec<String>, limit: usize) -> anyhow::Result<()> {
    if keys.len() >= limit {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(current) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect package storage path: {}", current.display())),
    };
    if metadata.file_type().is_symlink() {
        bail!("package storage list encountered a symlink");
    }
    if metadata.is_file() {
        let relative = current
            .strip_prefix(root)
            .context("package storage list escaped package root")?;
        let key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        keys.push(key);
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!("package storage list encountered a special filesystem object");
    }
    let mut entries = fs::read_dir(current)
        .with_context(|| format!("read package storage directory: {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("enumerate package storage directory: {}", current.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if keys.len() >= limit {
            break;
        }
        walk_files(root, &entry.path(), keys, limit)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_keys_reject_path_escape_and_accept_hierarchies() {
        assert!(validate_key("inbox/2026/message-1.eml").is_ok());
        assert!(validate_key("../secret").is_err());
        assert!(validate_key("inbox//secret").is_err());
        assert!(validate_key("C:/secret").is_err());
        assert!(validate_key("/absolute").is_err());
    }
}
