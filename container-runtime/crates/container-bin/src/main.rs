//! **This is a wiring demonstration, not the real container runtime
//! yet.** It proves `environments` + `vault` + `atomic-io` actually
//! connect and run — constructs the six-environment registry, prints
//! a health snapshot, exits. The real long-running process (an IPC
//! listener accepting execution requests from the main engine, per
//! migration-plan §5) needs `ipc-protocol` and `execution-engine`,
//! neither of which exist yet — still Phase 2 proper, not part of
//! this change. See `container-runtime-core`'s doc comment.
//!
//! **Deliberately uses its own vault namespace**, separate from the
//! engine's (`backend-rs-container` service name, `data/container-admin`
//! directory, not `backend-rs`/`data/admin`) — worth being honest
//! about why this matters and what it doesn't solve on its own: the
//! ACL in `vault` is enforced by *our own code*, not the OS credential
//! store itself. Any process running as the same OS user could bypass
//! our `Vault` API entirely and read OS-keyring entries directly if it
//! wanted to — the ACL is a well-behaved-caller boundary and an audit
//! trail, not a hard security guarantee against a fully malicious
//! process. Real enforcement that the container genuinely can't reach
//! the engine's secrets needs OS-level sandboxing (restricted process
//! permissions) once `sandbox-primitives` exists. A separate namespace
//! now is real, cheap hygiene in the meantime — not a claim that this
//! alone makes the boundary airtight.

use std::path::PathBuf;
use std::sync::Arc;

use container_runtime_core::EnvironmentRegistry;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let data_dir = PathBuf::from("./data/container-admin");
    let io = atomic_io::AtomicIo::new();

    let vault = Arc::new(
        vault::Vault::new(io.clone(), "backend-rs-container", &data_dir)
            .map_err(|e| anyhow::anyhow!("container-bin: failed to init vault: {e}"))?,
    );

    let registry = EnvironmentRegistry::new(io, vault, &data_dir);

    println!("container-bin: environment registry constructed. Health snapshot:");
    for (id, status) in registry.health_snapshot() {
        println!("  {id:<10} {status:?}");
    }

    println!(
        "\ncontainer-bin: this is a wiring demonstration only — no IPC listener, no real \
         sandboxed execution yet. See this file's doc comment."
    );

    Ok(())
}
