from pathlib import Path
import sys

mode = sys.argv[1] if len(sys.argv) > 1 else ""

if mode == "ipc-test":
    p = Path("engine/crates/service-runtime/src/mother.rs")
    text = p.read_text()
    old = '''        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, server_manager, server_token, shutdown_tx)
                .await
                .unwrap();
        });'''
    new = '''        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(stream, peer, server_manager, server_token, shutdown_tx)
                .await
                .unwrap();
        });'''
    if old not in text:
        raise SystemExit("Service Mother shutdown test helper anchor was not found")
    p.write_text(text.replace(old, new, 1))
elif mode == "cloud":
    p = Path("engine/crates/cloud-node/src/format.rs")
    text = p.read_text()
    old = "parent_content_sha256: has_parent.then_some(parent_raw),"
    new = "parent_content_sha256: (has_parent == 1).then_some(parent_raw),"
    if old not in text:
        raise SystemExit("Cloud Node parent flag fixup anchor was not found")
    p.write_text(text.replace(old, new, 1))

    p = Path("engine/crates/cloud-node/src/lib.rs")
    text = p.read_text()
    old = "pub use config::{CloudNodeSettings, NodeMode, ReplicationTarget, UpstreamSettings};"
    new = "pub use config::{CloudNodeSettings, NodeMode, NodeSettings, ReplicationSettings, ReplicationTarget, UpstreamSettings, SETTINGS_FILE_NAME};"
    if old not in text:
        raise SystemExit("Cloud Node config re-export fixup anchor was not found")
    p.write_text(text.replace(old, new, 1))

    p = Path("engine/crates/cloud-node/src/main.rs")
    text = p.read_text()
    old_import = '''use cloud_node::{
    load_signing_key_from_env, public_key_hex, CloudNodeSettings, CloudNodeStore,
};'''
    new_import = '''use cloud_node::{
    load_signing_key_from_env, public_key_hex, CloudNodeSettings, CloudNodeStore,
    SETTINGS_FILE_NAME,
};'''
    if old_import not in text:
        raise SystemExit("Cloud Node settings filename import anchor was not found")
    text = text.replace(old_import, new_import, 1)

    old_help = '''    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;'''
    new_help = '''    if matches!(command, "--help" | "-h") {
        print_help();
        return Ok(());
    }
    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;'''
    if old_help not in text:
        raise SystemExit("Cloud Node help/config fixup anchor was not found")
    text = text.replace(old_help, new_help, 1)

    old_default = '''                path.parent()
                    .map(|parent| parent.join("setting.node.cn.json"))
            })
        })
        .unwrap_or_else(|| PathBuf::from("setting.node.cn.json"))'''
    new_default = '''                path.parent()
                    .map(|parent| parent.join(SETTINGS_FILE_NAME))
            })
        })
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE_NAME))'''
    if old_default not in text:
        raise SystemExit("Cloud Node default settings path anchor was not found")
    p.write_text(text.replace(old_default, new_default, 1))

    p = Path("engine/crates/cloud-node/src/store.rs")
    text = p.read_text()
    old_now = '''fn now_ms() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))?)
}'''
    new_now = '''fn now_ms() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock predates Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| anyhow::anyhow!("system clock exceeds Cloud Node timestamp range"))
}'''
    if old_now not in text:
        raise SystemExit("Cloud Node now_ms clippy fixup anchor was not found")
    p.write_text(text.replace(old_now, new_now, 1))
else:
    raise SystemExit("usage: cloud-node-stage-fixups.py <ipc-test|cloud>")
