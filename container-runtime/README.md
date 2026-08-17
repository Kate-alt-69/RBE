# container-runtime

**Standalone execution service.** This is a separate Cargo workspace and a separate `container` executable. The main `backend` process launches and controls it over authenticated IPC; the container runtime is not merged into the backend process. This separation is a deliberate security boundary.

## Runtime topology

```text
backend
  │ authenticated control IPC
  ▼
container
  │
  ├── monitor process
  ├── global queue + durable execution journal
  ├── scheduler
  ├── persistent WASM artifact cache + learned profiles
  └── environments
       ├── general-1 → physical-core Swamps → 1 Worker thread/Swamp
       ├── general-2 → physical-core Swamps → 1 Worker thread/Swamp
       ├── general-3 → physical-core Swamps → 1 Worker thread/Swamp
       ├── general-4 → physical-core Swamps → 1 Worker thread/Swamp
       ├── general-5 → physical-core Swamps → 1 Worker thread/Swamp
       └── payment   → physical-core Swamps → 1 Worker thread/Swamp
```

By default the runtime creates **one lightweight Swamp per physical CPU core per Environment** and **one Worker thread per Swamp**. A 128-physical-core host therefore gets 128 Swamps + 128 Worker threads in each Environment: 640 Swamps and 640 Worker threads across the six configured Environment identities. Swamps are scheduler objects/threads, not 640 separate OS processes; only the disposable untrusted execution worker is a separate sandbox process.

Every execution receives a unique `ExecutionId` and carries an explicit resource policy and deny-by-default sandbox policy.

## Ephemeral Environment storage

Each Environment owns an ephemeral workspace under:

```text
./data/container-runtime/environments/<environment-id>/
```

The default quota metadata is **100 MiB per Environment**, and the workspace is wiped/recreated when that Environment is restarted. The current WASM ABI does not expose arbitrary host filesystem access yet, so the 100 MiB value is currently the Environment storage budget and lifecycle boundary rather than a filesystem-level quota primitive. Production Linux filesystem enforcement still belongs in the hardened rootfs/tmpfs layer.

## Swamps and workers

A **Swamp** is an Environment-local execution workshop. It owns a local queue, reusable workers, queue-cost accounting, throughput measurements, and local rebalancing.

A **Worker** is a reusable execution slot. Workers transition through `Idle → Running → Idle`. Real WASM artifacts are executed in a separate disposable `container --worker` child process so the scheduler/control plane does not become the workload boundary.

Global scheduling assigns work to an Environment. The Environment distributes it across Swamps, and Swamps rebalance backlog toward higher-capacity Swamps using bounded queue chunks.

## Monitor process

The container launches a small sibling monitor process from the same executable using `--monitor`. The monitor is independent of the scheduler and watches:

- container PID liveness;
- appended container event records;
- unexpected container process exit.

It writes its own audit trail to:

```text
./data/container-runtime/container-monitor.log
```

The monitor is intentionally narrow: it does not own execution policy and cannot submit jobs. Its role is observation and crash/event visibility.

## Cache and cost learning

`ArtifactCache` stores approved WASM artifacts and execution profiles. Static `WorkCost` is only a scheduling hint, never a security control. Actual execution duration is recorded so workload behavior can be learned from reality instead of source length or a fake millisecond score.

Artifacts are persisted under the container-owned runtime data directory and restored for crash recovery.

## Control IPC

`ipc-protocol` defines versioned length-prefixed JSON messages for the standalone container process. The control service binds only when explicitly requested and requires `RBE_CONTAINER_TOKEN` for request authentication.

The current control surface includes:

- hello/authentication
- execute submission
- health
- inspection
- cancellation
- environment restart

## Security boundary

Each execution carries:

- deny-by-default network policy;
- restricted filesystem policy;
- no-extra-capability policy;
- isolated PID/mount/IPC/UTS/network/**cgroup** namespaces;
- restricted syscall policy;
- CPU, memory, disk, network, process, file-descriptor, and wall-time limits.

On **Linux**, real artifact execution is refused unless the execution can be placed in:

- PID/mount/IPC/UTS/network/cgroup namespaces;
- `PR_SET_NO_NEW_PRIVS`;
- an x86_64 seccomp deny-list for host-control/privilege syscalls;
- cgroup-v2 CPU, memory and PID limits;
- a hard wall-clock timeout.

The actual WASM code runs in the disposable worker process with Wasmtime fuel and memory policy in addition to the OS boundary.

### Wasmtime security policy

The runtime was upgraded from the older 36.x line to **Wasmtime 45.0.0**. This is a deliberate security update: Wasmtime published multiple 2026 advisories affecting older releases, including a high-severity WASI `path_open(TRUNCATE)` permissions bypass and later WASI filesystem issues. Patched releases include 45.0.0 for the former advisory. The branch also adds `cargo audit` CI coverage so future dependency advisories are caught automatically.

On **Windows/non-Linux**, the policy contracts and portable scheduler remain buildable, but the runtime deliberately refuses to claim a secure OS sandbox until a native enforcement backend (for example Job Objects/AppContainer or an equivalent hardened design) exists.

The current Vault implementation uses a separate `backend-rs-container` namespace, but application ACLs are not an OS security boundary by themselves. Real host-secret isolation comes from the OS sandbox and process identity boundary.

## Restart, cancellation and recovery

The runtime keeps an append-only execution journal. Unresolved `queued` executions are replayed after a container-process restart using their original execution IDs. Approved WASM artifacts are persistent in the artifact cache, so recovery does not require the backend to resend the artifact.

Environment restart rebuilds its Swamp/Worker pool, wipes the Environment's ephemeral workspace, and requeues work that has not started. Cancellation removes queued work and marks running work for cooperative termination; wall-time expiry can forcibly kill the isolated worker process.

The current queue model is **at-least-once** across crashes: a crash between execution and the final `done` journal event can cause a replay. Exactly-once external side effects are intentionally not promised by the runtime.

## Debug mode

The standalone binary exposes live scheduler state:

```powershell
cargo run -p container -- --debug --swamps-per-environment 2 --workers-per-swamp 1 --demo 100
```

For a default hardware-sized runtime, omit `--swamps-per-environment`; the runtime will use physical CPU-core count where the host exposes it.

The output shows Environment → Swamp → Worker topology, execution IDs, generation, queue depth/cost, throughput, worker state, failures, storage budgets, and cache/artifact counts.

## Security test status

The branch contains automated unit/smoke coverage for sandbox policy validation, resource-limit checks, real Wasm executor rejection of invalid artifacts, IPC framing, cache persistence logic, and Linux security-layer jobs in CI.

The hostile Linux test path is manually triggerable through `workflow_dispatch`; regular CI remains unprivileged and portable.

## Remaining production work

The major remaining pieces are:

- native Windows OS-enforcement backend;
- stronger filesystem-root isolation (the current Linux namespace layer does not yet build a dedicated chroot/rootfs/tmpfs image, so the 100 MiB storage budget is currently an Environment lifecycle budget rather than a hard filesystem quota);
- a complete hostile-workload escape/stress suite covering devices, network escape, resource exhaustion, and runtime-control access;
- dependency-aware cache invalidation and richer multi-dimensional p50/p95/p99 profiles;
- immediate cancellation signalling to an already-running worker rather than only cooperative cancellation/timeout;
- production-grade sandbox identity/capability dropping and dedicated OS user management;
- full execution-state persistence beyond the queue-recovery journal.
