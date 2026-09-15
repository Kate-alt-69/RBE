use std::path::{Path, PathBuf};
use std::time::Duration;

use cloud_node::{
    load_signing_key_from_env, negotiate_sync, probe_upstream, provider_status, public_key_hex,
    synchronize_provider, synchronize_upstream, CloudNodeSettings, CloudNodeStore, ProviderClient,
    SETTINGS_FILE_NAME,
};

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
    match command {
        "evaluate" => {
            let summary = store.summary();
            let plan = store.sync_plan()?;
            println!("Cloud Node {} ready", settings.node.id);
            println!("storage={}", summary.storage.display());
            println!("backup={}", summary.backup.display());
            println!("syncRoot={}", plan.root_hex());
            println!("syncObjects={}", plan.object_count());
            if let Some(upstream) = &settings.upstream {
                println!("transport=peer");
                println!("upstream={}", upstream.url);
                println!("upstreamNode={}", upstream.node_id);
            }
            if let Some(provider) = &settings.provider {
                let client = ProviderClient::new(provider)?;
                println!("transport=provider");
                println!("provider={:?}", provider.kind);
                println!("providerTarget={}", client.target_description());
                println!("providerConflictPolicy={:?}", provider.conflict_policy);
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
            println!("before={:?}", result.before.relation);
            println!("action={:?}", result.action);
            println!("head={}", result.final_head);
            println!("root={}", result.final_root);
        }
        "run" => run_daemon(&settings, &store).await?,
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

async fn run_daemon(settings: &CloudNodeSettings, store: &CloudNodeStore) -> anyhow::Result<()> {
    if settings.provider.is_some() {
        return run_provider_daemon(settings, store).await;
    }
    run_peer_daemon(settings, store).await
}

async fn run_peer_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<()> {
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream or provider"))?;
    loop {
        match probe_upstream(settings).await {
            Ok(peer) => {
                println!(
                    "Cloud Node authenticated upstream {} session={}",
                    peer.node_id,
                    hex::encode(peer.session)
                );
                if upstream.sync_on_connect {
                    match synchronize_upstream(settings, store, &peer).await {
                        Ok(negotiation) => {
                            println!(
                                "Cloud Node sync roots local={} remote={} match={}",
                                hex::encode(negotiation.local.root_sha256),
                                hex::encode(negotiation.remote.root_sha256),
                                negotiation.roots_match()
                            );
                        }
                        Err(error) => {
                            eprintln!("Cloud Node synchronization failed: {error}");
                            if !upstream.auto_reconnect {
                                return Err(error);
                            }
                            tokio::time::sleep(Duration::from_millis(upstream.reconnect_delay_ms))
                                .await;
                            continue;
                        }
                    }
                }
                if !upstream.auto_reconnect {
                    return Ok(());
                }
            }
            Err(error) => {
                eprintln!("Cloud Node upstream unavailable: {error}");
                if !upstream.auto_reconnect {
                    return Err(error);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(upstream.reconnect_delay_ms)).await;
    }
}

async fn run_provider_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<()> {
    let provider = settings
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider mode is not configured"))?;
    let client = ProviderClient::new(provider)?;
    loop {
        let result = if provider.sync_on_connect {
            synchronize_provider(settings, store).await.map(|sync| {
                println!(
                    "Cloud Node provider sync target={} before={:?} action={:?} head={} root={}",
                    client.target_description(),
                    sync.before.relation,
                    sync.action,
                    sync.final_head,
                    sync.final_root
                );
            })
        } else {
            client.probe().await.map(|()| {
                println!(
                    "Cloud Node provider reachable target={}",
                    client.target_description()
                );
            })
        };

        if let Err(error) = result {
            eprintln!("Cloud Node provider synchronization failed: {error}");
            if !provider.auto_reconnect {
                return Err(error);
            }
        } else if !provider.auto_reconnect {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(provider.reconnect_delay_ms)).await;
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
        "cloud_node [--config=<setting.node.cn.json>] [evaluate|probe-upstream|negotiate-sync|sync-upstream|probe-provider|provider-status|sync-provider|run|sync-plan|verify|public-key|store-file <source> <logical>|store-video <source> <logical>|snapshot-folder <source> <logical>]"
    );
}
