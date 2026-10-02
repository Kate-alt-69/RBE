use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rbe_install_request::{SystemRuntimeKind, SystemRuntimeManifestRequest};
use rbe_install_runtime::{
    current_system_runtime_host, AdmittedSystemRuntime, GitSourceAcquisitionPlan, GitSourceReceipt,
    PinnedManagedToolchain,
};

use super::git_exec;

pub struct SourceAuthority {
    pinned_git: PinnedManagedToolchain,
    git_version: String,
}

pub struct SourceRequest<'a> {
    pub repository: &'a str,
    pub git_ref: &'a str,
    pub attempt_root: &'a Path,
}

pub struct SourceResult {
    pub receipt: GitSourceReceipt,
    pub git_version: String,
}

impl SourceAuthority {
    pub async fn hydrate(runtime_registry_base: &str, runtime_cache_root: &Path) -> Result<Self> {
        let host = current_system_runtime_host();
        let manifest_request = SystemRuntimeManifestRequest::new(
            runtime_registry_base,
            SystemRuntimeKind::Git,
            &host,
        )?;
        let (git, manifest) =
            AdmittedSystemRuntime::hydrate_from_registry(&manifest_request, runtime_cache_root)
                .await
                .context("hydrate managed rbe.sys.git runtime")?;
        let pinned_git = PinnedManagedToolchain::from_pins([(
            "git".to_owned(),
            git.executable,
            git.executable_sha256,
        )])?;
        Ok(Self {
            pinned_git,
            git_version: manifest.version,
        })
    }

    pub async fn acquire(&self, request: SourceRequest<'_>) -> Result<SourceResult> {
        if !request.attempt_root.is_absolute() {
            anyhow::bail!("deployment attempt root must be absolute");
        }
        if request.attempt_root.exists() {
            anyhow::bail!(
                "deployment attempt root already exists: {}",
                request.attempt_root.display()
            );
        }
        tokio::fs::create_dir_all(request.attempt_root)
            .await
            .context("create deployment attempt root")?;

        let workspace = absolute_child(request.attempt_root, "source-acquisition")?;
        let source_root = absolute_child(request.attempt_root, "source")?;
        let plan = GitSourceAcquisitionPlan::new(
            request.repository,
            request.git_ref,
            &self.pinned_git,
            &workspace,
        )?;
        let resolved = git_exec::fetch(&plan).await?;
        let receipt = git_exec::materialize(&plan, &resolved, &source_root).await?;
        Ok(SourceResult {
            receipt,
            git_version: self.git_version.clone(),
        })
    }
}

fn absolute_child(root: &Path, name: &str) -> Result<PathBuf> {
    if !root.is_absolute() {
        anyhow::bail!("deployment attempt root must be absolute");
    }
    Ok(root.join(name))
}
