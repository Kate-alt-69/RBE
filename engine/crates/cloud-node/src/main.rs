use std::path::{Path, PathBuf};
use std::time::Duration;

use cloud_node::{
    load_signing_key_from_env, negotiate_sync, probe_upstream, public_key_hex, CloudNodeSettings,
    CloudNodeStore, SETTINGS_FILE_NAME,
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
                println!("upstream={}", upstream.url);
                println!("upstreamNode={}", upstream.node_id);
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
    let upstream = settings
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream"))?;
    loop {
        match probe_upstream(settings).await {
            Ok(peer) => {
                println!(
                    "Cloud Node authenticated upstream {} session={}",
                    peer.node_id,
                    hex::encode(peer.session)
                );
                if upstream.sync_on_connect {
                    match negotiate_sync(settings, store, &peer).await {
                        Ok(negotiation) => {
                            println!(
                                "Cloud Node sync roots local={} remote={} match={}",
                                hex::encode(negotiation.local.root_sha256),
                                hex::encode(negotiation.remote.root_sha256),
                                negotiation.roots_match()
                            );
                        }
                        Err(error) => {
                            eprintln!("Cloud Node sync negotiation failed: {error}");
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
        "cloud_node [--config=<setting.node.cn.json>] [evaluate|probe-upstream|negotiate-sync|run|sync-plan|verify|public-key|store-file <source> <logical>|store-video <source> <logical>|snapshot-folder <source> <logical>]"
    );
}
