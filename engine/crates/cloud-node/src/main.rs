use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cloud_node::{
    ingest_registry_export, load_local_sync_settings, load_signing_key_from_env, negotiate_sync,
    probe_upstream, provider_status, public_key_hex, synchronize_provider,
    synchronize_registry_export, synchronize_upstream, CloudNodeSettings, CloudNodeStore,
    LocalSyncSettings, ProviderClient, ProviderConflictPolicy, ProviderSyncAction,
    ProviderSyncRelation, ProviderSyncResult, SETTINGS_FILE_NAME,
};

struct LocalSyncRuntime {
    settings: LocalSyncSettings,
    directory: PathBuf,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("cloud_node: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let config_path = take_config_arg(&mut args)?.unwrap_or_else(default_config_path);
    let command = args.first().map(String::as_str).unwrap_or("evaluate");

    if matches!(command, "--help" | "-h") {
        print_help();
        return Ok(());
    }
    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;
    let store = CloudNodeStore::open(&settings)?;
    let project_root = cloud_node_project_root(&config_path)?;
    let local_sync = load_local_sync_settings(&config_path)?
        .map(|local_settings| {
            let directory = local_settings.resolve_directory(&config_path)?;
            Ok::<_, anyhow::Error>(LocalSyncRuntime {
                settings: local_settings,
                directory,
            })
        })
        .transpose()?;

    match command {
        "evaluate" => {
            let summary = store.summary();
            let plan = store.sync_plan()?;
            println!("Cloud Node {} ready", settings.node.id);
            println!("storage={}", summary.storage.display());
            println!("backup={}", summary.backup.display());
            println!("syncRoot={}", plan.root_hex());
            println!("syncObjects={}", plan.object_count());
            if let Some(local) = &local_sync {
                let status = store
                    .local_directory_status(&local.directory, &local.settings.logical_prefix)?;
                println!("localSyncDirectory={}", local.directory.display());
                println!("localSyncPrefix={}", local.settings.logical_prefix);
                println!("localSyncDirty={}", status.dirty);
                println!("localSyncFiles={}", status.scanned_files);
                println!("localSyncObjects={}", status.managed_objects);
                println!("localSyncMissing={}", status.missing_managed_objects);
                println!("localSyncChanged={}", status.changed_managed_objects);
                println!("localSyncUntracked={}", status.untracked_files);
            }
            if let Some(upstream) = &settings.upstream {
                println!("transport=peer");
                println!("upstream={}", upstream.url);
                println!("upstreamNode={}", upstream.node_id);
                println!("upstreamPollIntervalMs={}", upstream.poll_interval_ms);
                println!(
                    "upstreamMaxReconnectDelayMs={}",
                    upstream.max_reconnect_delay_ms
                );
            }
            if let Some(provider) = &settings.provider {
                let client = ProviderClient::new(provider)?;
                println!("transport=provider");
                println!("provider={:?}", provider.kind);
                println!("providerTarget={}", client.target_description());
                println!("providerConflictPolicy={:?}", provider.conflict_policy);
                println!(
                    "providerIntegrityAuditIntervalMs={}",
                    provider.integrity_audit_interval_ms
                );
            }
        }
        "probe-upstream" => {
            let peer = probe_upstream(&settings).await?;
            println!("authenticated={}", peer.node_id);
            println!("session={}", hex::encode(peer.session));
        }
        "negotiate-sync" => {
            let peer = probe_upstream(&settings).await?;
            let negotiation = negotiate_sync(&settings, &store, &peer).await?;
            println!("authenticated={}", peer.node_id);
            println!("localRoot={}", hex::encode(negotiation.local.root_sha256));
            println!("remoteRoot={}", hex::encode(negotiation.remote.root_sha256));
            println!("rootsMatch={}", negotiation.roots_match());
        }
        "sync-upstream" => {
            sync_local_inputs(&store, &project_root, local_sync.as_ref())?;
            let peer = probe_upstream(&settings).await?;
            let negotiation = synchronize_upstream(&settings, &store, &peer).await?;
            println!("authenticated={}", peer.node_id);
            println!("localRoot={}", hex::encode(negotiation.local.root_sha256));
            println!("remoteRoot={}", hex::encode(negotiation.remote.root_sha256));
            println!("rootsMatch={}", negotiation.roots_match());
        }
        "probe-provider" => {
            let provider = settings
                .provider
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
            let client = ProviderClient::new(provider)?;
            client.probe().await?;
            println!("provider={:?}", provider.kind);
            println!("target={}", client.target_description());
            println!("reachable=true");
        }
        "provider-status" => {
            let status = provider_status(&settings, &store).await?;
            println!("relation={:?}", status.relation);
            println!("localHead={}", status.local_head);
            println!("localRoot={}", status.local_root);
            println!(
                "remoteHead={}",
                status.remote_head.as_deref().unwrap_or("<empty>")
            );
            println!(
                "remoteRoot={}",
                status.remote_root.as_deref().unwrap_or("<empty>")
            );
        }
        "sync-provider" => {
            let result = synchronize_provider(&settings, &store).await?;
            print_provider_result(&result);
        }
        "sync" => {
            let bootstrap_recovery = sync_bootstrap_requested(&args)?;
            if settings.provider.is_some() {
                let result = sync_provider_cycle(
                    &settings,
                    &store,
                    &project_root,
                    local_sync.as_ref(),
                    bootstrap_recovery,
                    true,
                )
                .await?;
                print_provider_result(&result);
            } else if settings.upstream.is_some() {
                if bootstrap_recovery {
                    anyhow::bail!(
                        "cloud_node sync --bootstrap is only supported for provider-backed synchronization"
                    );
                }
                sync_local_inputs(&store, &project_root, local_sync.as_ref())?;
                let peer = probe_upstream(&settings).await?;
                let negotiation = synchronize_upstream(&settings, &store, &peer).await?;
                println!("authenticated={}", peer.node_id);
                println!("localRoot={}", hex::encode(negotiation.local.root_sha256));
                println!("remoteRoot={}", hex::encode(negotiation.remote.root_sha256));
                println!("rootsMatch={}", negotiation.roots_match());
            } else {
                anyhow::bail!("Cloud Node sync requires an upstream or provider configuration");
            }
        }
        "ingest-registry" => {
            if args.len() != 2 {
                anyhow::bail!("ingest-registry requires <export-root>");
            }
            let export_root = Path::new(&args[1]);
            let result = ingest_registry_export(&store, export_root)?;
            println!("files={}", result.files);
            println!("metadataFiles={}", result.metadata_files);
            println!("artifactFiles={}", result.artifact_files);
            println!("removedFiles={}", result.removed_files);
            for stored in result.stored {
                println!(
                    "{}\t{}\t{}",
                    stored.logical_path, stored.object_key, stored.content_sha256
                );
            }
        }
        "sync-registry" => {
            if args.len() != 2 {
                anyhow::bail!("sync-registry requires <export-root>");
            }
            let export_root = Path::new(&args[1]);
            let result = synchronize_registry_export(&settings, &store, export_root).await?;
            println!("ingestFiles={}", result.ingest.files);
            println!("ingestMetadataFiles={}", result.ingest.metadata_files);
            println!("ingestArtifactFiles={}", result.ingest.artifact_files);
            println!("ingestRemovedFiles={}", result.ingest.removed_files);
            println!("before={:?}", result.provider.before.relation);
            println!("action={:?}", result.provider.action);
            println!("head={}", result.provider.final_head);
            println!("root={}", result.provider.final_root);
        }
        "run" => run_daemon(&settings, &store, &project_root, local_sync.as_ref()).await?,
        "sync-plan" => {
            let plan = store.sync_plan()?;
            println!("root={}", plan.root_hex());
            println!("folders={}", plan.folders.len());
            println!("videos={}", plan.videos.len());
            println!("files={}", plan.files.len());
            for object in plan.ordered() {
                println!(
                    "{:?}\t{}\t{}\t{}",
                    object.kind,
                    hex::encode(object.object_key),
                    hex::encode(object.content_sha256),
                    object.logical_path
                );
            }
        }
        "store-file" | "store-video" | "snapshot-folder" => {
            if args.len() != 3 {
                anyhow::bail!("{command} requires <source> <logical-path>");
            }
            let source = Path::new(&args[1]);
            let logical = &args[2];
            let stored = match command {
                "store-file" => store.store_file(source, logical)?,
                "store-video" => store.store_video(source, logical)?,
                "snapshot-folder" => store.snapshot_folder(source, logical)?,
                _ => unreachable!(),
            };
            println!("object={}", stored.object_key);
            println!("content={}", stored.content_sha256);
            println!("manifest={}", stored.manifest.display());
        }
        "verify" => {
            println!("verified={}", store.verify()?);
        }
        other => anyhow::bail!("unknown cloud_node command {other:?}; use --help"),
    }
    Ok(())
}

fn ingest_project_writes(store: &CloudNodeStore, project_root: &Path) -> anyhow::Result<()> {
    let ingested = store.ingest_storage_journal(project_root)?;
    if ingested > 0 {
        eprintln!(
            "cloud_node: ingested {ingested} project-root Storage write(s) from {}",
            project_root.display()
        );
    }
    Ok(())
}

fn sync_local_inputs(
    store: &CloudNodeStore,
    project_root: &Path,
    local_sync: Option<&LocalSyncRuntime>,
) -> anyhow::Result<()> {
    ingest_project_writes(store, project_root)?;
    if let Some(local) = local_sync {
        let result =
            store.sync_local_directory(&local.directory, &local.settings.logical_prefix)?;
        if result.stored > 0 || result.removed > 0 {
            eprintln!(
                "cloud_node: localSync scanned={} stored={} removed={} unchanged={} directory={}",
                result.scanned_files,
                result.stored,
                result.removed,
                result.unchanged,
                local.directory.display()
            );
        }
    }
    Ok(())
}

fn restore_local_after_pull(
    store: &CloudNodeStore,
    local_sync: Option<&LocalSyncRuntime>,
    action: ProviderSyncAction,
) -> anyhow::Result<()> {
    if !matches!(
        action,
        ProviderSyncAction::Pull | ProviderSyncAction::ForcedPull
    ) {
        return Ok(());
    }
    let Some(local) = local_sync else {
        return Ok(());
    };
    let result = store.restore_local_directory(&local.directory, &local.settings.logical_prefix)?;
    eprintln!(
        "cloud_node: provider pull restored localSync restored={} removed={} unchanged={} directory={}",
        result.restored,
        result.removed,
        result.unchanged,
        local.directory.display()
    );
    Ok(())
}

fn restore_missing_bootstrap_checkout(
    store: &CloudNodeStore,
    local_sync: Option<&LocalSyncRuntime>,
    should_restore: bool,
) -> anyhow::Result<()> {
    if !should_restore {
        return Ok(());
    }
    let Some(local) = local_sync else {
        return Ok(());
    };
    let restored =
        store.restore_missing_local_directory(&local.directory, &local.settings.logical_prefix)?;
    if restored > 0 {
        eprintln!(
            "cloud_node: bootstrap repaired partial localSync checkout restored={} directory={}",
            restored,
            local.directory.display()
        );
    }
    Ok(())
}

async fn sync_provider_cycle(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
    local_sync: Option<&LocalSyncRuntime>,
    bootstrap_recovery: bool,
    deep_in_sync_audit: bool,
) -> anyhow::Result<ProviderSyncResult> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let remote = provider_status(settings, store).await?;
    let (local_dirty, restore_missing_checkout) = local_sync
        .map(|local| {
            let status =
                store.local_directory_status(&local.directory, &local.settings.logical_prefix)?;
            let material_local_changes =
                status.changed_managed_objects > 0 || status.untracked_files > 0;
            let local_dirty = if bootstrap_recovery {
                material_local_changes
            } else {
                status.dirty
            };
            let restore_missing = bootstrap_recovery && status.missing_managed_objects > 0;
            Ok::<_, anyhow::Error>((local_dirty, restore_missing))
        })
        .transpose()?
        .unwrap_or((false, false));

    if local_dirty
        && matches!(
            remote.relation,
            ProviderSyncRelation::RemoteAhead | ProviderSyncRelation::Diverged
        )
        && provider.conflict_policy == ProviderConflictPolicy::Fail
    {
        anyhow::bail!(
            "Cloud Node provider changed remotely while localSync has uncommitted directory changes; refusing to overwrite either side. Commit/sync one side first or explicitly choose conflictPolicy prefer-local/prefer-remote"
        );
    }

    let prefer_local_dirty = local_dirty
        && provider.conflict_policy == ProviderConflictPolicy::PreferLocal
        && matches!(
            remote.relation,
            ProviderSyncRelation::RemoteAhead | ProviderSyncRelation::Diverged
        );

    if matches!(
        remote.relation,
        ProviderSyncRelation::RemoteAhead | ProviderSyncRelation::Diverged
    ) && !prefer_local_dirty
    {
        // Git-like recovery rule: reconcile provider history before any
        // working-tree/project mutation. An empty local store therefore adopts
        // the provider head instead of manufacturing a conflicting root.
        let recovered = synchronize_provider(settings, store).await?;
        restore_local_after_pull(store, local_sync, recovered.action)?;
    }

    // Backend bootstrap uses `sync --bootstrap`. Missing managed files are
    // treated as an incomplete checkout and filled from the verified CAS before
    // the normal scanner can interpret them as deletions. Existing changed and
    // untracked files are deliberately preserved as real local changes.
    restore_missing_bootstrap_checkout(store, local_sync, restore_missing_checkout)?;

    sync_local_inputs(store, project_root, local_sync)?;

    if !deep_in_sync_audit {
        let status = provider_status(settings, store).await?;
        if status.relation == ProviderSyncRelation::InSync {
            return Ok(ProviderSyncResult {
                action: ProviderSyncAction::None,
                final_root: status.local_root.clone(),
                final_head: status.local_head.clone(),
                before: status,
            });
        }
    }

    let result = synchronize_provider(settings, store).await?;
    restore_local_after_pull(store, local_sync, result.action)?;
    Ok(result)
}

async fn run_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
    local_sync: Option<&LocalSyncRuntime>,
) -> anyhow::Result<()> {
    if settings.provider.is_some() {
        return run_provider_daemon(settings, store, project_root, local_sync).await;
    }
    run_peer_daemon(settings, store, project_root, local_sync).await
}

async fn run_peer_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
    local_sync: Option<&LocalSyncRuntime>,
) -> anyhow::Result<()> {
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream or provider"))?;
    let mut retry_delay_ms = upstream.reconnect_delay_ms;
    loop {
        let result: anyhow::Result<()> = async {
            sync_local_inputs(store, project_root, local_sync)?;
            let peer = probe_upstream(settings).await?;
            println!(
                "Cloud Node authenticated upstream {} session={}",
                peer.node_id,
                hex::encode(peer.session)
            );
            if upstream.sync_on_connect {
                let negotiation = synchronize_upstream(settings, store, &peer).await?;
                println!(
                    "Cloud Node sync roots local={} remote={} match={}",
                    hex::encode(negotiation.local.root_sha256),
                    hex::encode(negotiation.remote.root_sha256),
                    negotiation.roots_match()
                );
            }
            Ok(())
        }
        .await;

        let delay_ms = match result {
            Err(error) => {
                eprintln!("Cloud Node upstream cycle failed: {error}");
                if !upstream.auto_reconnect {
                    return Err(error);
                }
                let delay = retry_delay_ms;
                retry_delay_ms = retry_delay_ms
                    .saturating_mul(2)
                    .min(upstream.max_reconnect_delay_ms);
                delay
            }
            Ok(()) => {
                retry_delay_ms = upstream.reconnect_delay_ms;
                if !upstream.auto_reconnect {
                    return Ok(());
                }
                upstream.poll_interval_ms
            }
        };
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

async fn run_provider_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
    local_sync: Option<&LocalSyncRuntime>,
) -> anyhow::Result<()> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let client = ProviderClient::new(provider)?;
    let mut retry_delay_ms = provider.reconnect_delay_ms;
    let mut write_probe_verified = false;
    let mut last_integrity_audit = None::<Instant>;
    loop {
        let result = if provider.sync_on_connect {
            let integrity_audit_due = provider_integrity_audit_due(
                last_integrity_audit.map(|last| last.elapsed()),
                provider.integrity_audit_interval_ms,
            );
            sync_provider_cycle(
                settings,
                store,
                project_root,
                local_sync,
                false,
                integrity_audit_due,
            )
            .await
            .map(|sync| {
                if integrity_audit_due || sync.action != ProviderSyncAction::None {
                    last_integrity_audit = Some(Instant::now());
                }
                println!(
                    "Cloud Node provider sync target={} before={:?} action={:?} head={} root={} integrityAudit={}",
                    client.target_description(),
                    sync.before.relation,
                    sync.action,
                    sync.final_head,
                    sync.final_root,
                    integrity_audit_due
                );
            })
        } else {
            sync_local_inputs(store, project_root, local_sync)?;
            let probe = if write_probe_verified {
                client.probe_read_only().await
            } else {
                client.probe().await
            };
            // A failed read-only health check must fall back to a full write+read
            // capability probe on the next retry so a deleted probe object can
            // self-heal instead of leaving the daemon permanently read-only.
            write_probe_verified = probe.is_ok();
            probe.map(|()| {
                println!(
                    "Cloud Node provider reachable target={}",
                    client.target_description()
                );
            })
        };

        let delay_ms = match result {
            Err(error) => {
                eprintln!("Cloud Node provider synchronization failed: {error}");
                if !provider.auto_reconnect {
                    return Err(error);
                }
                let delay = retry_delay_ms;
                retry_delay_ms = retry_delay_ms
                    .saturating_mul(2)
                    .min(provider.max_reconnect_delay_ms);
                delay
            }
            Ok(()) => {
                retry_delay_ms = provider.reconnect_delay_ms;
                if !provider.auto_reconnect {
                    return Ok(());
                }
                provider.poll_interval_ms
            }
        };
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

fn provider_integrity_audit_due(elapsed: Option<Duration>, interval_ms: u64) -> bool {
    elapsed.is_none_or(|elapsed| elapsed >= Duration::from_millis(interval_ms))
}

fn print_provider_result(result: &ProviderSyncResult) {
    println!("before={:?}", result.before.relation);
    println!("action={:?}", result.action);
    println!("head={}", result.final_head);
    println!("root={}", result.final_root);
}

fn sync_bootstrap_requested(args: &[String]) -> anyhow::Result<bool> {
    match args.get(1).map(String::as_str) {
        None => Ok(false),
        Some("--bootstrap") if args.len() == 2 => Ok(true),
        Some("--bootstrap") => {
            anyhow::bail!("cloud_node sync --bootstrap does not accept additional arguments")
        }
        Some(other) => anyhow::bail!(
            "cloud_node sync does not accept argument {other:?}; only the optional --bootstrap flag is supported"
        ),
    }
}

fn take_config_arg(args: &mut Vec<String>) -> anyhow::Result<Option<PathBuf>> {
    let mut found = None;
    let mut index = 0usize;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--config=") {
            if value.is_empty() || found.is_some() {
                anyhow::bail!("--config must be supplied at most once with a non-empty path");
            }
            found = Some(PathBuf::from(value));
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(found)
}

fn cloud_node_project_root(config_path: &Path) -> anyhow::Result<PathBuf> {
    let candidate = std::env::var_os("RBE_PROJECT_ROOT")
        .map(PathBuf::from)
        .or_else(|| config_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let root = candidate.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "Cloud Node ProjectRoot {} could not be canonicalized: {error}",
            candidate.display()
        )
    })?;
    if !root.is_dir() {
        anyhow::bail!(
            "Cloud Node ProjectRoot is not a directory: {}",
            root.display()
        );
    }
    Ok(root)
}

fn default_config_path() -> PathBuf {
    std::env::var_os("RBE_CN_SETTINGS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.join(SETTINGS_FILE_NAME)))
        })
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE_NAME))
}

fn print_help() {
    println!(
        "cloud_node [--config=<setting.node.cn.json>] [evaluate|probe-upstream|negotiate-sync|sync-upstream|probe-provider|provider-status|sync-provider|sync [--bootstrap]|ingest-registry <export-root>|sync-registry <export-root>|run|sync-plan|verify|public-key|store-file <source> <logical>|store-video <source> <logical>|snapshot-folder <source> <logical>]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_bootstrap_flag_is_explicit_and_bounded() {
        assert!(!sync_bootstrap_requested(&["sync".into()]).unwrap());
        assert!(sync_bootstrap_requested(&["sync".into(), "--bootstrap".into()]).unwrap());
        assert!(
            sync_bootstrap_requested(&["sync".into(), "--bootstrap".into(), "oops".into()])
                .is_err()
        );
        assert!(sync_bootstrap_requested(&["sync".into(), "unexpected".into()]).is_err());
    }

    #[test]
    fn provider_integrity_audit_runs_first_and_then_on_interval() {
        assert!(provider_integrity_audit_due(None, 900_000));
        assert!(!provider_integrity_audit_due(
            Some(Duration::from_millis(899_999)),
            900_000
        ));
        assert!(provider_integrity_audit_due(
            Some(Duration::from_millis(900_000)),
            900_000
        ));
    }
}
