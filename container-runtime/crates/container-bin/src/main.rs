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

//! **Reports issues into the SAME shared queue the main engine uses**
//! (`./data/admin`, via `error_client`) even though its VAULT
//! namespace is deliberately separate (see below) — these are
//! independent decisions for independent reasons. Vault isolation is
//! about limiting what secrets a compromised container could reach;
//! error reporting is about having ONE unified, centralized signed log
//! across every process in the system, which is the whole point of
//! `error_client`/the `--er` daemon existing as a shared, standalone
//! crate in the first place — see that crate's doc comment.

use std::path::PathBuf;
use std::sync::Arc;

use container_runtime_core::EnvironmentRegistry;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Shared with the main engine — where error-reports go, NOT where
    // this process's own vault secrets live (see `vault_data_dir`
    // below, and this file's doc comment for why those are two
    // different directories for two different reasons).
    let admin_dir = PathBuf::from("./data/admin");
    let io = atomic_io::AtomicIo::new();
    error_client::init(io.clone(), &admin_dir);
    error_client::install_panic_hook();

    let vault_data_dir = PathBuf::from("./data/container-admin");

    let vault = match vault::Vault::new(io.clone(), "backend-rs-container", &vault_data_dir) {
        Ok(v) => Arc::new(v),
        Err(e) => {
            error_client::report_issue(error_client::IssueInput {
                source: "container_bin_boot",
                level: Some(error_client::IssueLevel::Error),
                category: Some(error_client::IssueCategory::OperationFailure),
                message: &format!("container-bin: failed to init vault: {e}"),
                stack: None,
            });
            return Err(anyhow::anyhow!("container-bin: failed to init vault: {e}"));
        }
    };

    let registry = EnvironmentRegistry::new(io, vault, &vault_data_dir);

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
