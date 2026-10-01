use crate::config::CloudNodeSettings;
use crate::provider_sync::{
    self, ProviderSyncAction, ProviderSyncRelation, ProviderSyncResult, ProviderSyncStatus,
};
use crate::store::CloudNodeStore;

const MAX_PROVIDER_STABILIZATION_PASSES: usize = 4;

/// Run the normal provider transaction and prove the live provider HEAD still
/// agrees with the activated local snapshot before reporting success.
///
/// Provider objects are immutable, but HEAD is intentionally mutable. A remote
/// writer can therefore publish another commit while a large snapshot is being
/// transferred. Re-checking status after each complete transaction prevents a
/// stale pull/push from being reported as the final synchronized state. Forward
/// remote movement is reconciled again within a small bounded number of passes.
/// A provider that disappears or moves backwards immediately after a pull is
/// failed closed instead of turning the freshly pulled snapshot into a push.
pub async fn synchronize_provider(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<ProviderSyncResult> {
    let mut first_before = None;
    let mut last_mutating_action = None;

    for pass in 1..=MAX_PROVIDER_STABILIZATION_PASSES {
        let mut result = provider_sync::synchronize_provider(settings, store)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node provider synchronization pass {pass} failed: {error:#}"
                )
            })?;

        if first_before.is_none() {
            first_before = Some(result.before.clone());
        }
        if result.action != ProviderSyncAction::None {
            last_mutating_action = Some(result.action);
        }

        let status = provider_sync::provider_status(settings, store)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node provider stabilization check after pass {pass} failed: {error:#}"
                )
            })?;

        if provider_sync_stable(&status) {
            result.before = first_before.take().unwrap_or(result.before);
            if let Some(action) = last_mutating_action {
                result.action = action;
            }
            result.final_root = status.local_root;
            result.final_head = status.local_head;
            return Ok(result);
        }

        if pull_regressed(last_mutating_action, status.relation) {
            anyhow::bail!(
                "Cloud Node provider moved backwards or disappeared after a pull (relation={:?}, localHead={}, remoteHead={}); refusing to republish the pulled snapshot. Retry after provider HEAD is stable",
                status.relation,
                status.local_head,
                status.remote_head.as_deref().unwrap_or("<empty>")
            );
        }

        if pass == MAX_PROVIDER_STABILIZATION_PASSES {
            anyhow::bail!(
                "Cloud Node provider did not stabilize after {MAX_PROVIDER_STABILIZATION_PASSES} synchronization passes: relation={:?} localHead={} remoteHead={}. Provider HEAD is changing too quickly; retry synchronization",
                status.relation,
                status.local_head,
                status.remote_head.as_deref().unwrap_or("<empty>")
            );
        }
    }

    unreachable!("bounded provider stabilization loop must return or fail")
}

fn provider_sync_stable(status: &ProviderSyncStatus) -> bool {
    status.relation == ProviderSyncRelation::InSync
}

fn pull_regressed(
    last_action: Option<ProviderSyncAction>,
    relation: ProviderSyncRelation,
) -> bool {
    matches!(
        last_action,
        Some(ProviderSyncAction::Pull | ProviderSyncAction::ForcedPull)
    ) && matches!(
        relation,
        ProviderSyncRelation::EmptyRemote | ProviderSyncRelation::LocalAhead
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(relation: ProviderSyncRelation) -> ProviderSyncStatus {
        ProviderSyncStatus {
            relation,
            local_head: "11".repeat(32),
            remote_head: Some("22".repeat(32)),
            local_root: "33".repeat(32),
            remote_root: Some("44".repeat(32)),
        }
    }

    #[test]
    fn stabilization_requires_exact_in_sync_relation() {
        assert!(provider_sync_stable(&status(ProviderSyncRelation::InSync)));
        for relation in [
            ProviderSyncRelation::EmptyRemote,
            ProviderSyncRelation::LocalAhead,
            ProviderSyncRelation::RemoteAhead,
            ProviderSyncRelation::Diverged,
        ] {
            assert!(!provider_sync_stable(&status(relation)));
        }
    }

    #[test]
    fn pull_never_turns_provider_regression_into_automatic_push() {
        for action in [ProviderSyncAction::Pull, ProviderSyncAction::ForcedPull] {
            assert!(pull_regressed(
                Some(action),
                ProviderSyncRelation::EmptyRemote
            ));
            assert!(pull_regressed(
                Some(action),
                ProviderSyncRelation::LocalAhead
            ));
            assert!(!pull_regressed(
                Some(action),
                ProviderSyncRelation::RemoteAhead
            ));
            assert!(!pull_regressed(
                Some(action),
                ProviderSyncRelation::Diverged
            ));
        }
        assert!(!pull_regressed(
            Some(ProviderSyncAction::Push),
            ProviderSyncRelation::LocalAhead
        ));
    }
}
