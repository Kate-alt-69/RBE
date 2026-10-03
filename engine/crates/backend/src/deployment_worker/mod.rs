mod git_exec;
mod publisher;
mod source;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use publisher::{bounded_message, ClaimedDeployment, PublisherClient, WorkerUpdate};
use source::{SourceAuthority, SourceRequest};
use uuid::Uuid;

const DEFAULT_RUNTIME_REGISTRY_BASE: &str = "https://kastrick-backend.onrender.com/api/";
const DEFAULT_POLL_SECONDS: u64 = 3;
const MAX_SOURCE_ATTEMPTS: u32 = 3;

pub async fn run() -> Result<()> {
    let config = WorkerConfig::from_env()?;
    tokio::fs::create_dir_all(&config.work_root)
        .await
        .context("create RBE deployment worker root")?;

    // Infrastructure/runtime failures happen before a deployment lease is
    // claimed. A missing/malformed managed Git manifest must never turn into a
    // user package failure.
    let runtime_cache = config.work_root.join("runtime-cache");
    let source_authority = SourceAuthority::hydrate(&config.runtime_registry_base, &runtime_cache)
        .await
        .context("initialize RBE deployment source authority before claiming work")?;
    let publisher = PublisherClient::new(&config.publisher_base, config.helper_token.clone())?;

    eprintln!(
        "RBE deployment worker {} ready; managed Git admitted before lease claims",
        config.worker_id
    );

    loop {
        let Some(deployment) = publisher.claim_next(&config.worker_id).await? else {
            tokio::time::sleep(config.poll_interval).await;
            continue;
        };

        if let Err(error) =
            process_deployment(&publisher, &source_authority, &config, &deployment).await
        {
            let message = bounded_message(&format!("source acquisition failed: {error:#}"));
            let terminal = deployment.attempt >= MAX_SOURCE_ATTEMPTS;
            let update = if terminal {
                WorkerUpdate {
                    status: "failed",
                    stage: "failed",
                    level: "error",
                    message: &message,
                    blocked_reason: None,
                }
            } else {
                WorkerUpdate {
                    status: "running",
                    stage: "fetch",
                    level: "error",
                    message: &message,
                    blocked_reason: Some("source_acquisition_retry"),
                }
            };
            if let Err(report_error) = publisher.update(&deployment, update).await {
                eprintln!(
                    "failed to report deployment {} source error: {report_error:#}",
                    deployment.deployment_id
                );
            }
        }
    }
}

async fn process_deployment(
    publisher: &PublisherClient,
    source_authority: &SourceAuthority,
    config: &WorkerConfig,
    deployment: &ClaimedDeployment,
) -> Result<()> {
    if publisher.source_receipt_exists(deployment).await? {
        publisher
            .complete_source_handoff(
                deployment,
                "Immutable RBE source receipt already sealed; source acquisition skipped and handed to validation.",
            )
            .await?;
        return Ok(());
    }

    let attempt_root = attempt_root(
        &config.work_root,
        &deployment.deployment_id,
        deployment.attempt,
    )?;
    let result = source_authority
        .acquire(SourceRequest {
            repository: &deployment.repository,
            git_ref: &deployment.git_ref,
            attempt_root: &attempt_root,
        })
        .await?;

    publisher
        .seal_source_receipt(deployment, &result.receipt)
        .await?;

    let message = bounded_message(&format!(
        "Source sealed at commit {} with {} files / {} bytes (tree {}, managed Git {}); handed to validation.",
        result.receipt.resolved_commit,
        result.receipt.source_tree.file_count,
        result.receipt.source_tree.total_bytes,
        result.receipt.source_tree.sha256,
        result.git_version
    ));
    publisher
        .complete_source_handoff(deployment, &message)
        .await?;
    Ok(())
}

struct WorkerConfig {
    publisher_base: String,
    helper_token: String,
    runtime_registry_base: String,
    work_root: PathBuf,
    worker_id: String,
    poll_interval: Duration,
}

impl WorkerConfig {
    fn from_env() -> Result<Self> {
        let publisher_base = required_env("KASTRICK_RPX_PUBLISHER_URL")?;
        if !publisher_base.starts_with("https://") {
            anyhow::bail!("KASTRICK_RPX_PUBLISHER_URL must use public HTTPS");
        }

        let helper_token = required_env("KASTRICK_RPX_HELPER_TOKEN")?;
        if helper_token.len() < 32 {
            anyhow::bail!("KASTRICK_RPX_HELPER_TOKEN must contain at least 32 characters");
        }

        let runtime_registry_base = std::env::var("RBE_DEPLOYMENT_RUNTIME_REGISTRY_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_RUNTIME_REGISTRY_BASE.to_owned());
        if !runtime_registry_base.starts_with("https://") {
            anyhow::bail!("RBE_DEPLOYMENT_RUNTIME_REGISTRY_URL must use HTTPS");
        }

        let work_root = std::env::var_os("RBE_DEPLOYMENT_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_paths::default_admin_dir().join("deployment-worker"));
        if !work_root.is_absolute() {
            anyhow::bail!("RBE_DEPLOYMENT_WORK_ROOT must be absolute");
        }

        let worker_id = std::env::var("RBE_DEPLOYMENT_WORKER_ID")
            .ok()
            .filter(|value| valid_worker_id(value))
            .unwrap_or_else(|| format!("rbe-deployment-worker:{}", Uuid::new_v4()));
        if !valid_worker_id(&worker_id) {
            anyhow::bail!("RBE_DEPLOYMENT_WORKER_ID contains unsupported characters");
        }

        let poll_seconds = std::env::var("RBE_DEPLOYMENT_POLL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_POLL_SECONDS)
            .clamp(1, 60);

        Ok(Self {
            publisher_base,
            helper_token,
            runtime_registry_base,
            work_root,
            worker_id,
            poll_interval: Duration::from_secs(poll_seconds),
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    if value.trim().is_empty() {
        anyhow::bail!("{name} must not be empty");
    }
    Ok(value)
}

fn attempt_root(work_root: &Path, deployment_id: &str, attempt: u32) -> Result<PathBuf> {
    if !valid_deployment_id(deployment_id) || attempt == 0 {
        anyhow::bail!("invalid deployment attempt identity");
    }
    Ok(work_root
        .join("attempts")
        .join(deployment_id)
        .join(attempt.to_string()))
}

fn valid_worker_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn valid_deployment_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("dpl_") else {
        return false;
    };
    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}
