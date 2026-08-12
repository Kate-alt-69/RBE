# container-runtime

Separate Cargo workspace, separate binary — never merged into the main
engine process. See `engine/README.md`'s "non-negotiable exception."

## The six environments

Five general-purpose (`general-1`..`general-5`) plus one dedicated
payment environment. Fixed, closed set — not a dynamic pool — because
the payment environment specifically needs to be a known, deliberately
configured thing, not "whichever environment happened to be free."

Every environment, general or payment, gets:

- **Health monitoring** (`environments::HealthMonitor`) — heartbeat
  staleness + consecutive-failure tracking, three states (Healthy /
  Degraded / Unresponsive).
- **Abuse detection** (`environments::AbuseDetector`) — four
  independent windowed counters per caller: request rate, CPU time,
  network bytes, disk bytes. Crossing any one threshold is a block —
  it's an OR across dimensions, not requiring all four maxed out at
  once. Payment gets tighter default thresholds than general
  environments (see `AbuseThresholds::payment_defaults`).

The payment environment additionally gets:

- **Encryption boundary** (`environments::PaymentEnvironment`) —
  incoming payloads are decrypted, processed, and re-encrypted before
  leaving; the plaintext buffer is zeroized immediately after use, so
  it exists in memory only for the duration of the actual work, never
  touches disk unencrypted. The encryption key is read from the shared
  `vault` crate, ACL-gated under the caller identity
  `payment_environment`.
- **An explicitly stubbed `send_details`** — proves the encryption +
  audit-logging wiring end to end. Does **not** perform a real network
  call; there is no payment gateway integration in this codebase.

## What this crate does NOT do (yet)

This is the monitoring/policy layer, not OS-level sandboxing. It
decides whether a caller's *reported* resource usage is abusive; it
does not itself measure CPU/network/disk consumption, and it does not
enforce a `Blocked` verdict by actually killing or refusing to run
anything. That's `sandbox-primitives`/`resource-limits`/
`execution-engine`'s job — still stubs, not touched by this change.
Once those exist, they're the natural caller of
`EnvironmentRegistry::record_general_execution`/
`record_payment_execution` with real measured numbers, and the thing
that actually acts on a `Blocked` verdict.

There's also no IPC listener yet — `container-bin` currently just
constructs the registry, prints a health snapshot, and exits, to prove
the wiring works. The real long-running process that accepts execution
requests from the main engine needs `ipc-protocol` +
`execution-engine`, both still empty stubs.

## Shared dependencies

`atomic-io` and `vault` are standalone crates at the project root
(siblings of `engine/`, `api/`, `module/`, `container-runtime/`), not
part of either Cargo workspace — both the engine and this container
runtime depend on them via relative path. One implementation of
"encrypt/store a secret" and "write a file safely" to get right, not
two copies in two processes to keep in sync. See each crate's own doc
comment for what it actually guarantees.

**Worth being honest about:** this container runtime uses its own
vault namespace (`backend-rs-container` service name, separate from
the engine's `backend-rs`) as cheap hygiene, but the ACL that gates
vault access is enforced by *our own code*, not the OS. A fully
malicious process running as the same OS user could still bypass the
`Vault` API and read OS-keyring entries directly. Real enforcement
that this process genuinely can't reach the engine's secrets needs
OS-level sandboxing (restricted process permissions), which doesn't
exist yet either.

## Testing this

Same pattern as `engine/README.md`'s testing section:

```
cd container-runtime
cargo test --workspace
cargo run -p container-bin
```

Expect the unit tests (health thresholds, abuse-detection windowing,
payment encryption round-trip/tamper-detection — the highest-risk file
here, flagged in `payment.rs`'s doc comment) to pass, and
`container-bin` to print a health snapshot for all six environments
before exiting.
