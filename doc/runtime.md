# RBE Runtime Architecture

This page describes the current process/runtime boundaries on `main`. The important rule is that RBE does not make `backend` the ambient owner of every subsystem.

## Process topology

A deployed RBE instance is conceptually:

```text
backend
  |
  +-- Service Mother (service/service.exe)
  |     +-- Service REL worker
  |     +-- Service REL worker
  |     `-- ...
  |
  +-- Container Controller (container/container.exe)
  |     +-- Environment child: general-1
  |     +-- Environment child: general-2
  |     +-- Environment child: general-3
  |     +-- Environment child: general-4
  |     +-- Environment child: general-5
  |     `-- Environment child: payment
  |             `-- disposable WASM worker per execution
  |
  +-- supervised Vault child
  `-- Error Reporter / CONTROL ER

separate loopback listener:
  RBE Control Room -> 127.0.0.1:5799
```

The exact number of active Service workers depends on service mode/lifecycle. The six Container Environment identities are the current configured runtime set.

## Backend ownership

`backend` owns the active immutable Runtime Image, public HTTP listener, application shared state, native request/security stack, top-level critical-child supervision, and the handoff of narrowly scoped authority to child runtimes.

The backend builds Route/Module execution from the Runtime Image snapshot rather than treating raw source files as request-time authority.

The active image now has a full lowercase SHA-256 `imageId` derived from the canonical source hash, Route-WASM ABI/compiler versions, and effective settings. See [`runtime-image.md`](runtime-image.md) for the exact identity contract.

## Service Runtime

Service REL runs under a separately linked canonical `service` executable, not inside `backend` and not through a copy of `backend.exe`.

One Service Mother owns worker creation, authenticated Service Fabric routing, lifecycle/restart handling, and worker supervision. Every active service is a separate OS process of the canonical Service executable.

Service processes receive bounded bootstrap state—including the linked Runtime ENV snapshot—through supervised parent channels. They do not derive application authority from arbitrary inherited environment variables.

See [`x.service/`](x.service/).

## Container Runtime

The Container runtime is a standalone execution service controlled by authenticated IPC. The Controller owns global scheduling, durable execution journal/artifact metadata, Environment process ownership, and capability-manifest enforcement.

### Environment processes

Each configured Environment has a persistent child `container` process. Real artifact execution crosses that OS process boundary before the Environment child launches a disposable `container --worker` process running Wasmtime.

```text
backend
  -> authenticated Container control IPC
Container Controller
  -> session-authenticated Environment channel
Environment child
  -> disposable WASM worker
```

The Environment session capability is separate from the external Controller token, delivered through inherited bootstrap input, and bound to that child generation.

### Environment storage — authoritative in child

After `BUG-CTR-003`, Controller-side scheduling no longer pretends to own transactional Environment storage when execution is delegated to an Environment process. The Environment child owns its ephemeral storage manager/lifecycle for the external-runner path.

The current default Environment storage budget remains 100 MiB. It is an Environment lifecycle/resource boundary; stronger filesystem-root isolation is still separate hardening work.

### Cancellation lifecycle — hard and linearized

Running external executions have an Environment-owned cancellation path. The Controller tracks the owning Environment/generation and routes cancellation to the child, which can terminate the disposable worker rather than merely marking a running job cooperatively cancelled in Controller state.

`BUG-CTR-004` also linearizes cancellation against completion through one execution-lifecycle state. Submitted/recovered executions stay live until an outcome is published. A cancel that wins the lifecycle transition is guaranteed to be represented as cancelled; a completion that removes the execution from the live set first makes a later cancel correctly return too-late/false instead of racing a worker snapshot.

Queued cancellation removes the task and publishes a cancelled outcome. Once dispatch has left a queue, cancellation is forwarded into the Environment supervisor, whose pending-cancel/active-execution state closes the dispatch-to-worker ownership race.

Cancellation requests remain generation-bound so stale authority cannot be silently applied to a replacement Environment generation.

### Swamps and workers

Swamps remain Environment-local scheduling workshops. By default RBE sizes them from physical CPU-core count and uses one Worker thread per Swamp. These scheduler objects are not one OS process each; the persistent Environment child and disposable artifact worker are the important process boundaries.

### Artifact/runtime safety

Approved WASM artifacts are cached by hash and executed with Wasmtime policy. On Linux, secure execution requires the configured namespace/no-new-privileges/seccomp/cgroup/wall-time controls before RBE claims a secure sandbox. Windows/non-Linux builds deliberately do not claim equivalent OS sandbox enforcement until a native backend exists.

## Capability manifests

Sandbox-originated host authority is deny-by-default. Container capability grants are registered against an exact identity tuple:

```text
RuntimeImage SHA-256
+ SourceId
+ Environment
+ Environment generation
```

The Runtime Image value must be the lowercase 64-character image identity produced by RELC. A grant identifies a capability kind, logical target, allowed operation(s), and request/response byte limits. Wildcards are rejected. Debug/host-file capabilities are refused by a non-debug Controller.

Replacing either the active image identity or an Environment generation requires authority to be registered for the new exact identity; old Environment-generation grants can be revoked when the generation is replaced.

## Execution recovery

The Container runtime keeps an append-only execution journal and persistent artifact cache. Queued unresolved work can be replayed after Controller restart using its existing execution identity/resource limits. The crash model is at-least-once: exactly-once external side effects are not promised across a crash between execution and final durable completion recording.

## HostBootstrap and credential authority

On Linux, normal backend boot establishes the HostBootstrap prerequisite before normal credential authority is created. RBE verifies/provisions a usable Secret Service path and fails closed when the required credential boundary cannot be established.

Vault remains the secrets authority. Runtime ENV, Service bootstrap metadata, Container grants, and Error Reporter metadata are not secret-storage substitutes.

## Error Reporter / CONTROL ER

Error Reporter supports BASIC diagnostic authority and CONTROL recovery-decision authority. CONTROL material is issued only from an already-authorized parent and transported through inherited one-shot bootstrap channels rather than command-line flags, environment variables, or key files.

CONTROL ER can make bounded component-local recovery decisions for unexpected failures, but the component's owning supervisor remains the process creator:

- Service Mother owns Service worker replacement;
- backend owns Service Mother replacement;
- backend/Container supervision owns Container replacement;
- Vault supervision owns Vault-child recovery;
- backend owns Error Reporter replacement when ER itself dies.

If CONTROL ER is unavailable or fails authentication/timing checks, local bounded recovery policy remains the fallback. ER therefore augments supervision instead of becoming a single process-spawn god object. :)

## Control Room

When dashboards are enabled, the backend-owned Control Room is served on a separate loopback listener at:

```text
127.0.0.1:5799
```

The configured public API-side admin path is only a redirect to that isolated listener; the dashboard itself is not served directly from the public API socket.

The Control Room exposes authenticated runtime/backend/container/security/settings views and control endpoints. Builds without an admin password leave the protected Control Room unavailable until rebuilt/configured appropriately.

## Server state gating

The active Runtime Image's ServerPolicy gates normal requests:

- `online` — normal traffic;
- `readonly` — only GET/HEAD/OPTIONS;
- `maintenance`, `draining`, `offline` — normal traffic rejected as unavailable.

Health/admin/maintenance control-plane routes remain reachable so status does not lock operators out of recovery.

## Runtime Image activation

`RuntimeImageSlot` provides immutable snapshots plus atomic replacement. This is the activation primitive for transactional relinking:

```text
compile candidate -> validate candidate -> atomically activate
                         |
                       failure
                         v
                    keep current image
```

The image identity is deterministic, but it is not a digital signature and does not by itself authenticate the publisher. See [`runtime-image.md`](runtime-image.md) and [`source-security.md`](source-security.md).

A complete automatic source watcher/hot-reload coordinator remains separate work.

## Important remaining boundaries

Current architecture should not be read as claiming these are finished:

- a native Windows hostile-workload sandbox equivalent to the Linux enforcement path;
- dedicated rootfs/chroot/tmpfs-style filesystem isolation for Container workloads;
- exactly-once external side effects across Container crashes;
- full native Route-WASM HTTP dispatch coverage;
- multi-instance/distributed Service REL scheduling;
- persistent signed source-less Runtime Image (RBI) loading.
