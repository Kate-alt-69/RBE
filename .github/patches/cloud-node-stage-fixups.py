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
    new = "pub use config::{CloudNodeSettings, NodeMode, NodeSettings, ReplicationSettings, ReplicationTarget, UpstreamSettings};"
    if old not in text:
        raise SystemExit("Cloud Node config re-export fixup anchor was not found")
    p.write_text(text.replace(old, new, 1))

    p = Path("engine/crates/cloud-node/src/main.rs")
    text = p.read_text()
    old = '''    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;'''
    new = '''    if matches!(command, "--help" | "-h") {
        print_help();
        return Ok(());
    }
    if command == "public-key" {
        let signing = load_signing_key_from_env()?;
        println!("{}", public_key_hex(&signing));
        return Ok(());
    }

    let settings = CloudNodeSettings::load(&config_path)?;'''
    if old not in text:
        raise SystemExit("Cloud Node help/config fixup anchor was not found")
    p.write_text(text.replace(old, new, 1))
else:
    raise SystemExit("usage: cloud-node-stage-fixups.py <ipc-test|cloud>")
