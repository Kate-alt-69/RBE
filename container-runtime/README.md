# container-runtime

**Standalone execution service.** This is a separate Cargo workspace and a separate `container` executable. The main `backend` process is expected to launch and control it over authenticated IPC; the container runtime is not merged into the backend process. This separation is a deliberate security boundary.

## Runtime topology

```text
backend
  │ authenticated control IPC
  ▼
container
  │
  ├── global queue + durable execution journal
  ├── scheduler
  ├── persistent WASM artifact cache + learned profiles
  └── environments
       ├── general-1 → Swamps → Workers
       ├── general-2 → Swamps → Workers
       ├── general-3 → Swamps → Workers
       ├── general-4 → Swamps → Workers
       ├── general-5 → Swamps → Workers
       └── payment   → Swamps → Workers
```

Every execution receives a unique `ExecutionId` and carries an explicit resource policy and deny-by-default sandbox policy.

## Swamps and workers

A **Swamp** is an Environment-local execution workshop. It owns a local queue, reusable workers, queue-cost accounting, throughput measurements, and local rebalancing.

A **Worker** is a reusable execution slot. Workers transition through `Idle → Running → Idle`. Real WASM artifacts are executed in a separate disposable `container --worker` child process so the scheduler/control plane does not become the workload boundary.

Global scheduling assigns work to an Environment. The Environment distributes it across Swamps, and Swamps rebalance backlog toward higher-capacity Swamps using bounded queue chunks.

## Cache and cost learning

`ArtifactCache` stores approved WASM artifacts and execution profiles. Static `WorkCost` is only a scheduling hint, never a security control. Actual execution duration is recorded so workload behavior can be learned from reality instead of source length or a fake millisecond score.

Artifacts are persisted under the container-owned runtime data directory and lazily restored after a process restart.

## Control IPC

`ipc-protocol` defines versioned length-prefixed JSON messages for the standalone container process. The control service binds only when explicitly requested and requires `RBE_CONTAINER_TOKEN` for request authentication.

The current control surface includes:

- hello/authentication
- execute submission
- health
- inspection
- cancellation
- environment restart

The backend-side launcher/client is still a separate integration task; the container server itself is operational on the branch.

## Security boundary

Each execution carries:

- deny-by-default network policy;
- restricted filesystem policy;
- no-extra-capability policy;
- isolated namespace policy;
- restricted syscall policy;
- CPU, memory, disk, network, process, file-descriptor, and wall-time limits.

On **Linux**, real artifact execution is refused unless the execution can be placed in:

- PID/mount/IPC/UTS/network namespaces;
- `PR_SET_NO_NEW_PRIVS`;
- an x86_64 seccomp deny-list for host-control/privilege syscalls;
- cgroup-v2 CPU, memory and PID limits;
- a hard wall-clock timeout.

The actual WASM code runs in the disposable worker process with Wasmtime fuel and memory policy in addition to the OS boundary.

On **Windows/non-Linux**, the policy contracts and portable scheduler remain buildable, but the runtime deliberately refuses to claim a secure OS sandbox until a native enforcement backend (for example Job Objects/AppContainer or an equivalent hardened design) exists.

The current Vault implementation uses a separate `backend-rs-container` namespace, but application ACLs are not an OS security boundary by themselves. Real host-secret isolation comes from the OS sandbox and process identity boundary.

## Restart, cancellation and recovery

The runtime keeps an append-only execution journal. Unresolved `queued` executions are replayed after a container-process restart using their original execution IDs. Approved WASM artifacts are persistent in the artifact cache, so recovery does not require the backend to resend the artifact.

Environment restart rebuilds its Swamp/Worker pool and requeues work that has not started. Cancellation removes queued work and marks running work for cooperative termination; wall-time expiry can forcibly kill the isolated worker process.

The current queue model is **at-least-once** across crashes: a crash between execution and the final `done` journal event can cause a replay. Exactly-once external side effects are intentionally not promised by the runtime.

## Debug mode

The standalone binary exposes live scheduler state:

```powershell
cargo run -p container -- --debug --swamps-per-environment 2 --workers-per-swamp 2 --demo 100
```

The output shows Environment → Swamp → Worker topology, execution IDs, generation, queue depth/cost, throughput, worker state, failures, and cache/artifact counts.

## Security test status

The branch contains automated unit/smoke coverage for sandbox policy validation, resource-limit checks, real Wasm executor rejection of invalid artifacts, IPC framing, cache persistence logic, and Linux security-layer jobs in CI.

A **full hostile-workload escape suite is still required** before production security sign-off. That suite should exercise filesystem exposure, process visibility, namespace creation, ptrace, mount, BPF, key management, device access, network escape, fork/resource exhaustion, and runtime-control-socket access from inside the worker boundary.

## Remaining production work

The major remaining pieces are:

- backend-side launcher/client integration for the standalone `container` process;
- native Windows OS-enforcement backend;
- stronger filesystem-root isolation (the current Linux namespace layer does not yet build a dedicated chroot/rootfs image);
- a complete hostile-workload escape/stress suite;
- dependency-aware cache invalidation and richer multi-dimensional p50/p95/p99 profiles;
- immediate cancellation signalling to an already-running worker rather than only cooperative cancellation/timeout;
- production-grade sandbox identity/capability dropping and dedicated OS user management;
- full execution-state persistence beyond the queue-recovery journal.
