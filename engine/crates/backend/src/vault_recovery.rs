use std::collections::BTreeMap;
use std::time::Duration;

use service_runtime::ServiceRestartDirective;

pub struct VaultErRecoveryAuthority {
    client: crate::er_recovery::ErRecoveryClient,
    runtime: tokio::runtime::Handle,
}

impl VaultErRecoveryAuthority {
    pub fn new(key: crate::host_bootstrap::ErControlKey) -> Self {
        Self {
            client: crate::er_recovery::ErRecoveryClient::new(key),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl vault_process::VaultRecoveryAuthority for VaultErRecoveryAuthority {
    fn decide(
        &self,
        report: vault_process::VaultProcessExitReport,
    ) -> anyhow::Result<vault_process::VaultRecoveryAdvice> {
        let mut context = BTreeMap::new();
        context.insert("supervision_scope".into(), "credential-runtime".into());
        context.insert("transport".into(), "stdio-json".into());
        context.insert("recovery_owner".into(), "vault-process-io".into());

        let report = crate::er_recovery::ProcessExitReport {
            component: "vault-runtime".into(),
            process_image: if cfg!(windows) {
                "backend.exe".into()
            } else {
                "backend".into()
            },
            pid: report.pid,
            exit_success: report.exit_success,
            exit_code: report.exit_code,
            exit_signal: report.exit_signal,
            previous_restart_attempts: report.previous_restart_attempts,
            uptime_ms: report.uptime_ms,
            expected: false,
            phase: report.phase,
            last_operation: report.last_operation,
            observation_error: report.observation_error,
            context,
        };

        let decision = self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(700),
                self.client.decide_process(report),
            )
            .await
        });
        match decision {
            Ok(Ok(ServiceRestartDirective::Restart {
                minimum_backoff_ms,
                reason,
            })) => Ok(vault_process::VaultRecoveryAdvice {
                minimum_backoff: Duration::from_millis(minimum_backoff_ms),
                reason: Some(reason),
            }),
            Ok(Ok(ServiceRestartDirective::Default)) => {
                Ok(vault_process::VaultRecoveryAdvice::default())
            }
            Ok(Ok(ServiceRestartDirective::Stop { reason })) => {
                tracing::error!(
                    authority_reason = %reason,
                    "CONTROL ER requested Stop for unexpected critical Vault exit; ignoring unsafe stop directive"
                );
                Ok(vault_process::VaultRecoveryAdvice {
                    minimum_backoff: Duration::ZERO,
                    reason: Some(format!("unsafe CONTROL ER stop ignored: {reason}")),
                })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => anyhow::bail!("CONTROL ER Vault recovery decision timed out"),
        }
    }
}
