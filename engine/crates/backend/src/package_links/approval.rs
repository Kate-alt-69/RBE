use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use rbe_install_runtime::{VerifiedRootGraph, VerifiedRpxRootSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const APPROVAL_FORMAT: u32 = 1;
const APPROVAL_FILE: &str = "package-capabilities.json";
const PROJECT_LOCK: &str = "package.lock.rbe.yaml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPrivilegeRequest {
    pub package: String,
    pub version: String,
    pub artifact_sha256: String,
    pub capability: String,
    pub description: &'static str,
}

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

/// Return the currently implemented RBE-owned runtime privileges requested by
/// the verified root package.
///
/// Custom/package-private names intentionally do not appear here. Private
/// dependencies also cannot widen root authority: the root package is the host
/// security principal and must request any RBE-owned privilege it needs.
pub(crate) fn install_requests(graph: &VerifiedRootGraph) -> anyhow::Result<Vec<HostPrivilegeRequest>> {
    let verified = graph
        .packages
        .get(&graph.root)
        .with_context(|| format!("verified graph is missing root package {:?}", graph.root))?;
    let locked = graph
        .lock
        .packages
        .get(&graph.root)
        .with_context(|| format!("verified graph lock is missing root package {:?}", graph.root))?;

    let mut requests = Vec::new();
    for (capability, enabled) in &verified.manifest.capabilities {
        if !enabled {
            continue;
        }
        let Some(description) = explicit_host_privilege_description(capability) else {
            continue;
        };
        requests.push(HostPrivilegeRequest {
            package: graph.root.clone(),
            version: locked.version.clone(),
            artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
            capability: capability.clone(),
            description,
        });
    }
    Ok(requests)
}

/// Load exact approvals for one verified package snapshot.
///
/// Missing or stale approval is equivalent to no authority. Malformed approval
/// state, or an approval for a capability the verified artifact no longer
/// requests, fails closed instead of being silently widened.
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

    let requested = snapshot
        .worker
        .read_requested_capabilities(project_root)
        .with_context(|| {
            format!(
                "re-verify capability requests for approved package {:?}",
                snapshot.package
            )
        })?;

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

/// Persist approvals after successful package activation.
///
/// The approval is bound to the exact target lock hash as well as the root
/// package version/artifact identity. Any root or private dependency graph
/// change therefore invalidates the record on the next Backend boot.
pub(crate) fn persist_install_approval(
    project_root: &Path,
    graph: &VerifiedRootGraph,
    project_lock_sha256: &str,
    approved_runtime: &[String],
) -> anyhow::Result<()> {
    validate_sha256(project_lock_sha256, "project lock")?;
    let locked = graph
        .lock
        .packages
        .get(&graph.root)
        .with_context(|| format!("verified graph lock is missing root package {:?}", graph.root))?;

    let requested = graph
        .packages
        .get(&graph.root)
        .with_context(|| format!("verified graph is missing root package {:?}", graph.root))?
        .manifest
        .capabilities
        .iter()
        .filter(|(_, enabled)| **enabled)
        .map(|(capability, _)| capability.as_str())
        .collect::<Vec<_>>();

    let mut runtime = approved_runtime.to_vec();
    runtime.sort();
    runtime.dedup();
    for capability in &runtime {
        if !requested.iter().any(|request| *request == capability) {
            bail!(
                "cannot persist unrequested host privilege {:?} for package {:?}",
                capability,
                graph.root
            );
        }
        if explicit_host_privilege_description(capability).is_none() {
            bail!(
                "cannot persist unsupported or package-private privilege {:?} as RBE host authority",
                capability
            );
        }
    }

    let mut state = read_state(project_root)?.unwrap_or_default();
    validate_state(&state)?;
    state.packages.insert(
        graph.root.clone(),
        PackageApproval {
            version: locked.version.clone(),
            artifact_sha256: locked.artifact_sha256.to_ascii_lowercase(),
            project_lock_sha256: project_lock_sha256.to_ascii_lowercase(),
            runtime,
        },
    );
    write_state(project_root, &state)
}

pub(crate) fn explicit_host_privilege_description(capability: &str) -> Option<&'static str> {
    match capability {
        "net:http" => Some(
            "make public HTTP/HTTPS requests through RBE's hardened network broker",
        ),
        // `log` is an implicit package-scoped host capability and never needs a
        // privilege prompt. Unknown/custom names remain package-private until a
        // trusted RBE host provider explicitly registers them.
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
                bail!("RBE package approval state is not a regular file: {}", path.display());
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

fn write_state(project_root: &Path, state: &ApprovalState) -> anyhow::Result<()> {
    validate_state(state)?;
    let rbe = project_root.join(".rbe");
    fs::create_dir_all(&rbe)
        .with_context(|| format!("create project-local RBE state directory: {}", rbe.display()))?;
    let metadata = fs::symlink_metadata(&rbe)
        .with_context(|| format!("inspect project-local RBE state directory: {}", rbe.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("project-local RBE state path is not a regular directory: {}", rbe.display());
    }

    let path = approval_path(project_root);
    let temporary = rbe.join(format!(".{APPROVAL_FILE}.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state).context("serialize RBE package approval state")?;
    fs::write(&temporary, bytes)
        .with_context(|| format!("write temporary RBE package approval state: {}", temporary.display()))?;

    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("replace old RBE package approval state: {}", path.display()))?;
    }
    fs::rename(&temporary, &path).with_context(|| {
        format!(
            "commit project-local RBE package approval state: {}",
            path.display()
        )
    })?;
    Ok(())
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
        bail!("active project package lock is not a regular file: {}", path.display());
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
        assert!(explicit_host_privilege_description("log").is_none());
        assert!(explicit_host_privilege_description("mail:smtp").is_none());
        assert!(explicit_host_privilege_description("net:tcp").is_none());
    }
}
