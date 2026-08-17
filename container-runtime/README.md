# container-runtime

**Standalone execution service.** This is a separate Cargo workspace and a separate `container` executable. The main `backend` process launches and controls it over authenticated IPC; the container runtime is not merged into the backend process. This separation is a deliberate security boundary.

## Runtime topology

```text
backend
  │ authenticated control IPC
  ▼
container
  │
  ├── global queue
  ├── scheduler
  ├── cache / execution profiles
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

A **Worker** is a reusable execution slot. Workers transition through `Idle → Running → Idle`; future sandbox failures must be able to destroy/recreate workers without giving the workload access to the trusted supervisor.

Global scheduling assigns work to an Environment. The Environment distributes it across Swamps, and Swamps rebalance backlog toward higher-throughput Swamps using bounded task batches.

## Cache and cost learning

The first scheduler slice contains an in-memory artifact/profile cache. Static `WorkCost` is a hint, not a security control. Actual execution duration is recorded into an execution profile so the scheduler can learn observed workload behavior instead of relying on source length or a fake millisecond cost.

The intended future cache structure is:

```text
artifact hash
  ├── WASM artifact
  ├── declared cost profile
  └── learned execution profile (p50/p95/p99, resource dimensions)
```

## Control IPC

`ipc-protocol` defines versioned length-prefixed JSON messages for the standalone container process. The current control service binds to an explicitly supplied address and requires `RBE_CONTAINER_TOKEN` for request authentication.

Supported protocol categories include:

- hello/authentication
- execute submission
- health
- inspection
- cancellation
- environment restart

The current branch wires authenticated execute submission into the live scheduler. Cancellation and environment restart remain lifecycle work to be completed before they can be advertised as fully operational.

## Security boundary

Each execution carries:

- deny-by-default network policy;
- restricted filesystem policy;
- no-extra-capability policy;
- isolated namespace policy;
- restricted syscall policy;
- CPU, memory, disk, network, process, file-descriptor, and wall-time limits.

These are currently **policy contracts**. They are not a claim that the OS already enforces every restriction. The kernel-level implementation belongs in `sandbox-primitives` and the execution backend.

The current Vault implementation uses a separate `backend-rs-container` namespace, but application ACLs are not an OS security boundary by themselves. A process running with the same OS identity can bypass the Vault API. Real host-secret isolation therefore still requires OS-level sandboxing.

## Payment environment

The payment environment remains a dedicated fixed environment with tighter abuse thresholds and its existing encrypted processing boundary. Its encryption/payment code is still subject to the warnings in `payment.rs`; the presence of the container scheduler does not make payment integration production-ready.

## Debug mode

The standalone binary exposes live scheduler state:

```powershell
cargo run -p container -- --debug --swamps-per-environment 2 --workers-per-swamp 2 --demo 100
```

The output shows Environment → Swamp → Worker topology, execution IDs, queue depth/cost, throughput, worker state, and cache profiles.

## What is still not implemented

The following remain required before calling this a production container sandbox:

- real WASM execution in `execution-engine`;
- OS-enforced namespaces / process isolation;
- cgroup or Windows Job Object resource enforcement;
- seccomp / platform syscall restrictions;
- capability dropping and dedicated OS identities;
- durable queue checkpoint/recovery;
- full worker cancellation and environment restart semantics;
- authenticated control IPC integrated with the main backend launcher;
- complete sandbox escape/stress-test suite;
- persistent cache invalidation and dependency-aware artifact reuse.

Do not describe the current scheduler as a secure OS sandbox yet. The current branch is the execution-control foundation that those enforcement layers plug into.
