use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use rbe_install_runtime::VerifiedRpxRootSnapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const APPROVAL_FORMAT: u32 = 1;
const APPROVAL_FILE: &str = "package-capabilities.json";
const PROJECT_LOCK: &str = "package.lock.rbe.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalState {
    format: u32,
    #[serde(default)]
    packages: BTreeMap<String, PackageApproval>,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self {
            format: APPROVAL_FORMAT,
            packages: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageApproval {
    version: String,
    artifact_sha256: String,
    project_lock_sha256: String,
    #[serde(default)]
    runtime: Vec<String>,
}

pub(crate) fn approved_runtime_capabilities(
    project_root: &Path,
    snapshot: &VerifiedRpxRootSnapshot,
) -> anyhow::Result<Vec<String>> {
    let Some(state) = read_state(project_root)? else {
        return Ok(Vec::new());
    };
    validate_state(&state)?;
    let Some(approval) = state.packages.get(&snapshot.package) else {
        return Ok(Vec::new());
    };

    let current_lock_sha256 = current_project_lock_sha256(project_root)?;
    if approval.version != snapshot.version
        || !approval
            .artifact_sha256
            .eq_ignore_ascii_case(&snapshot.artifact_sha256)
        || !approval
            .project_lock_sha256
            .eq_ignore_ascii_case(&current_lock_sha256)
    {
        return Ok(Vec::new());
    }

    let requested = requested_runtime_capabilities(project_root, snapshot)?;
    for capability in &approval.runtime {
        if !requested.iter().any(|request| request == capability) {
            bail!(
                "project approval for package {:?} contains capability {:?} that its verified artifact no longer requests",
                snapshot.package,
                capability
            );
        }
        if explicit_host_privilege_description(capability).is_none() {
            bail!(
                "project approval for package {:?} contains capability {:?} that is not an explicitly approvable RBE host privilege",
                snapshot.package,
                capability
            );
        }
    }

    Ok(approval.runtime.clone())
}

pub(crate) fn requested_runtime_capabilities(
    project_root: &Path,
    snapshot: &VerifiedRpxRootSnapshot,
) -> anyhow::Result<Vec<String>> {
    let mut requested = snapshot
        .worker
        .read_requested_capabilities(project_root)
        .with_context(|| {
            format!(
                "re-verify capability requests for package {:?}",
                snapshot.package
            )
        })?;
    requested.sort();
    requested.dedup();
    Ok(requested)
}

#[allow(dead_code)]
pub(crate) fn replace_runtime_approval(
    project_root: &Path,
    snapshot: &VerifiedRpxRootSnapshot,
    capabilities: &[String],
) -> anyhow::Result<Vec<String>> {
    let requested = requested_runtime_capabilities(project_root, snapshot)?;
    let requested = requested.into_iter().collect::<BTreeSet<_>>();
    let mut approved = BTreeSet::new();

    for capability in capabilities {
        if !requested.contains(capability) {
            bail!(
                "package {:?} does not request host capability {:?}",
                snapshot.package,
                capability
            );
        }
        if explicit_host_privilege_description(capability).is_none() {
            bail!(
                "capability {:?} is not an explicitly approvable RBE host privilege",
                capability
            );
        }
        approved.insert(capability.clone());
    }

    let mut state = read_state(project_root)?.unwrap_or_default();
    validate_state(&state)?;
    if approved.is_empty() {
        state.packages.remove(&snapshot.package);
    } else {
        state.packages.insert(
            snapshot.package.clone(),
            PackageApproval {
                version: snapshot.version.clone(),
                artifact_sha256: snapshot.artifact_sha256.to_ascii_lowercase(),
                project_lock_sha256: current_project_lock_sha256(project_root)?,
                runtime: approved.iter().cloned().collect(),
            },
        );
    }
    write_state(project_root, &state)?;
    Ok(approved.into_iter().collect())
}

pub(crate) fn explicit_host_privilege_description(capability: &str) -> Option<&'static str> {
    match capability {
        "net:http" => Some("make public HTTP/HTTPS requests through RBE's hardened network broker"),
        "net:dns" => Some("resolve public DNS address and MX records through RBE's bounded DNS broker"),
        "net:tcp" => Some(
            "open bounded stateful connections to public TCP destinations through RBE's session-scoped network broker",
        ),
        "net:tcp-listen" => Some(
            "bind bounded inbound TCP listeners through RBE; this can expose network services and may require host permission for privileged ports",
        ),
        "net:tls" => Some(
            "open bounded public TLS/STARTTLS transports with RBE-owned certificate validation; net:tcp approval is also required",
        ),
        "storage" => Some(
            "read and write package-scoped durable blobs under this project's RBE state directory",
        ),
        "crypto" => Some(
            "use bounded RBE-owned cryptographic primitives including secure randomness, SHA-256 and HMAC-SHA256",
        ),
        "service:package" => Some(
            "register verified package-owned .service programs with Service Mother under the package namespace",
        ),
        _ => None,
    }
}

fn approval_path(project_root: &Path) -> PathBuf {
    project_root.join(".rbe").join(APPROVAL_FILE)
}

fn read_state(project_root: &Path) -> anyhow::Result<Option<ApprovalState>> {
    let path = approval_path(project_root);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "RBE package approval state is not a regular file: {}",
                    path.display()
                );
            }
            let bytes = fs::read(&path)
                .with_context(|| format!("read RBE package approval state: {}", path.display()))?;
            let state = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse RBE package approval state: {}", path.display()))?;
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("inspect RBE package approval state: {}", path.display())),
    }
}

#[allow(dead_code)]
fn write_state(project_root: &Path, state: &ApprovalState) -> anyhow::Result<()> {
    validate_state(state)?;
    let rbe = project_root.join(".rbe");
    match fs::symlink_metadata(&rbe) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "RBE project state root is not a regular directory: {}",
                rbe.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&rbe)
                .with_context(|| format!("create RBE project state root: {}", rbe.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect RBE project state root: {}", rbe.display()))
        }
    }

    let path = approval_path(project_root);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "RBE package approval state is not a regular file: {}",
                path.display()
            );
        }
    }
    let mut bytes =
        serde_json::to_vec_pretty(state).context("encode RBE package approval state")?;
    bytes.push(b'\n');
    fs::write(&path, bytes)
        .with_context(|| format!("write RBE package approval state: {}", path.display()))
}

fn validate_state(state: &ApprovalState) -> anyhow::Result<()> {
    if state.format != APPROVAL_FORMAT {
        bail!(
            "unsupported RBE package approval format {}; expected {}",
            state.format,
            APPROVAL_FORMAT
        );
    }
    for (package, approval) in &state.packages {
        if package.is_empty() || package.len() > 192 {
            bail!("invalid package name in RBE package approval state: {package:?}");
        }
        validate_sha256(&approval.artifact_sha256, "package artifact")?;
        validate_sha256(&approval.project_lock_sha256, "project lock")?;
        let mut previous: Option<&str> = None;
        for capability in &approval.runtime {
            if capability.is_empty() || capability.len() > 192 {
                bail!("invalid capability in RBE package approval state: {capability:?}");
            }
            if previous.is_some_and(|value| value >= capability.as_str()) {
                bail!(
                    "RBE package approval capabilities must be sorted and unique for package {package:?}"
                );
            }
            previous = Some(capability);
        }
    }
    Ok(())
}

fn current_project_lock_sha256(project_root: &Path) -> anyhow::Result<String> {
    let path = project_root.join(PROJECT_LOCK);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect active project package lock: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "active project package lock is not a regular file: {}",
            path.display()
        );
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("read active project package lock: {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_sha256(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid {label} SHA-256 in RBE package approval state");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_implemented_rbe_privileges_are_promptable() {
        assert!(explicit_host_privilege_description("net:http").is_some());
        assert!(explicit_host_privilege_description("net:dns").is_some());
        assert!(explicit_host_privilege_description("net:tcp").is_some());
        assert!(explicit_host_privilege_description("net:tcp-listen").is_some());
        assert!(explicit_host_privilege_description("net:tls").is_some());
        assert!(explicit_host_privilege_description("storage").is_some());
        assert!(explicit_host_privilege_description("crypto").is_some());
        assert!(explicit_host_privilege_description("service:package").is_some());
        assert!(explicit_host_privilege_description("log").is_none());
        assert!(explicit_host_privilege_description("mail:smtp").is_none());
    }
}
