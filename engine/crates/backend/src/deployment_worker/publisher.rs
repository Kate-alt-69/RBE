use anyhow::{Context, Result};
use core_lib::call_public_http;
use rbe_install_runtime::GitSourceReceipt;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ClaimedDeployment {
    pub deployment_id: String,
    pub repository: String,
    pub git_ref: String,
    pub attempt: u32,
    pub lease_token: String,
}

pub struct WorkerUpdate<'a> {
    pub status: &'a str,
    pub stage: &'a str,
    pub level: &'a str,
    pub message: &'a str,
    pub blocked_reason: Option<&'a str>,
}

pub struct PublisherClient {
    base: String,
    helper_token: String,
}

impl PublisherClient {
    pub fn new(base: &str, helper_token: String) -> Result<Self> {
        let base = base.trim().trim_end_matches('/');
        if !base.starts_with("https://") || base.contains('?') || base.contains('#') {
            anyhow::bail!("KASTRICK_RPX_PUBLISHER_URL must be a clean public HTTPS base URL");
        }
        Ok(Self {
            base: base.to_owned(),
            helper_token,
        })
    }

    pub async fn claim_next(&self, worker_id: &str) -> Result<Option<ClaimedDeployment>> {
        let value = self
            .post(
                "v1/developer/deployment/source/claim-next",
                json!({ "workerId": worker_id }),
            )
            .await?;
        require_ok(&value)?;
        let Some(deployment) = value.get("deployment") else {
            anyhow::bail!("deployment claim response is missing deployment field");
        };
        if deployment.is_null() {
            return Ok(None);
        }
        let deployment_id = required_string(deployment, "deploymentId")?;
        if !valid_deployment_id(&deployment_id) {
            anyhow::bail!("publisher returned an invalid deployment ID");
        }
        let attempt = deployment
            .get("attempt")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .context("publisher returned an invalid deployment attempt")?;
        let lease_token = required_string(deployment, "leaseToken")?;
        if lease_token.len() != 64 || !lease_token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("publisher returned an invalid deployment lease token");
        }
        Ok(Some(ClaimedDeployment {
            deployment_id,
            repository: required_string(deployment, "repository")?,
            git_ref: required_string(deployment, "gitRef")?,
            attempt,
            lease_token,
        }))
    }

    pub async fn source_receipt_exists(&self, deployment: &ClaimedDeployment) -> Result<bool> {
        let value = self
            .post(
                "v1/developer/deployment/source/get",
                json!({
                    "deploymentId": deployment.deployment_id,
                    "leaseToken": deployment.lease_token
                }),
            )
            .await?;
        require_ok(&value)?;
        Ok(value
            .get("sourceReceipt")
            .is_some_and(|value| !value.is_null()))
    }

    pub async fn seal_source_receipt(
        &self,
        deployment: &ClaimedDeployment,
        receipt: &GitSourceReceipt,
    ) -> Result<()> {
        let value = self
            .post(
                "v1/developer/deployment/source/seal",
                json!({
                    "deploymentId": deployment.deployment_id,
                    "leaseToken": deployment.lease_token,
                    "resolvedCommit": receipt.resolved_commit,
                    "sourceTree": {
                        "algorithm": receipt.source_tree.algorithm,
                        "sha256": receipt.source_tree.sha256,
                        "fileCount": receipt.source_tree.file_count,
                        "totalBytes": receipt.source_tree.total_bytes
                    }
                }),
            )
            .await?;
        require_ok(&value)?;
        if value.get("sourceReceipt").is_none_or(Value::is_null) {
            anyhow::bail!("publisher did not return the sealed source receipt");
        }
        Ok(())
    }

    pub async fn update(
        &self,
        deployment: &ClaimedDeployment,
        update: WorkerUpdate<'_>,
    ) -> Result<()> {
        let value = self
            .post(
                "v1/developer/deployment/update",
                json!({
                    "deploymentId": deployment.deployment_id,
                    "leaseToken": deployment.lease_token,
                    "status": update.status,
                    "stage": update.stage,
                    "level": update.level,
                    "message": update.message,
                    "blockedReason": update.blocked_reason
                }),
            )
            .await?;
        require_ok(&value)?;
        Ok(())
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let response = call_public_http(
            "request",
            &[json!({
                "method": "POST",
                "url": url,
                "headers": {
                    "authorization": format!("Bearer {}", self.helper_token),
                    "content-type": "application/json"
                },
                "body": body,
                "timeoutMs": 10_000
            })],
        )
        .await
        .context("call trusted deployment publisher")?;
        let status = response
            .get("status")
            .and_then(Value::as_u64)
            .context("HTTP broker response is missing status")?;
        let http_ok = response.get("ok").and_then(Value::as_bool) == Some(true);
        let body = response
            .get("body")
            .and_then(Value::as_str)
            .context("HTTP broker response is missing body")?;
        let value: Value =
            serde_json::from_str(body).context("decode deployment publisher JSON")?;
        if !http_ok {
            anyhow::bail!(
                "deployment publisher returned HTTP {status}: {}",
                bounded_message(&value.to_string())
            );
        }
        Ok(value)
    }
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("deployment publisher response is missing {field}"))
}

fn require_ok(value: &Value) -> Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("publisher_operation_failed");
    anyhow::bail!("deployment publisher rejected operation: {error}")
}

pub fn bounded_message(value: &str) -> String {
    value.chars().take(1800).collect()
}

fn valid_deployment_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("dpl_") else {
        return false;
    };
    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}
