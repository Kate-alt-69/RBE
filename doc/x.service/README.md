# `*.service` — Service REL

Service REL defines backend programs that execute as managed service processes. It shares global REL grammar with the other source types, but its runtime ownership, lifecycle, and IPC behavior are service-specific.

## Purpose

Use `.service` for work that benefits from process isolation or independent lifecycle management:

- long-lived or wake-on-demand workers
- email/background work
- isolated caches/search/indexing
- CPU/memory-contained jobs
- shared backend capabilities accessed through Service Fabric
- future service-backed middleware

## Declaration

Current service metadata is declared with `:service[...]`.

```text
:service[
  name = cache,
  mode = hybrid,
  restart = on-failure,
  memoryLimitMb = 256,
  startupTimeoutMs = 10000,
  idleTimeoutMs = 300000,
  instances = 1
]
```

The active runtime supports resident/on-demand/hybrid lifecycle concepts, restart policies, startup/idle limits, authenticated local IPC, exported function calls, lifecycle events, and process-local service memory. Multi-instance service execution remains future work on this branch.

## Global grammar

Service REL should have the same higher-level REL grammar as Route, Module, and Server REL. Service-specific restrictions apply to capabilities and lifecycle, not to ordinary language power.

## Lifecycle

A service may define:

```text
class Service {
    start(ctx) { }
    event(event) { }
    health(ctx) { }
    stop(ctx) { }
}
```

These hooks belong specifically to Service REL because only a service has this managed process lifecycle.

## Exports

Normal exported functions form the service's callable interface:

```text
export async function send(message) {
    ...
}
```

Callers should interact with that interface through the central Service Runtime/Fabric.

## ENV

Service REL should receive the same Mother-owned Runtime ENV snapshot as authorized Module REL code:

```text
:import[ENV]

export function region() {
    return ENV.require("REGION");
}
```

The service worker should receive a compiled/assigned environment snapshot from the root runtime rather than independently guessing configuration from source files.

## Service Fabric target

The target process topology is:

```text
RBE root / Grandmother
        |
central Service Manager / service runtime
        |
  +-----+------+-----+
  |            |     |
service.exe service.exe service.exe
```

The central manager owns service registry, assignment, supervision, RPC, deadlines, resource limits, restart policy, and dependency metadata.

Target call metadata includes:

```text
requestId
parentInvocationId
dependencyId
deadline
callDepth
```

This lets recursive/dependency tracking continue across service-process boundaries instead of ending at IPC.

## Service-to-service calls

The target architecture allows `.service -> .service` calls through the central Service Fabric. Raw peer-to-peer IPC is not the goal.

The active branch currently rejects direct service imports in `.service` parsing, so this page treats service-to-service REL calls as planned until the parser/runtime are upgraded.

## Recursion and deadlocks

A service call graph may be recursive without being invalid. What must be rejected is a runtime dependency chain that cannot make progress, for example service A waiting on B while B ultimately waits on the unresolved value currently being produced by A.

The central manager should propagate invocation/dependency IDs and participate in the Runtime Engine's cyclic-computation detection. See [`../relc.md`](../relc.md).

## Process-local memory

The current `memory` capability is RAM owned by the active service process. It is not durable state. A cold activation can create a fresh process and therefore a fresh memory store.

QuickDB-style in-memory membership structures are likewise accelerators, not the source of truth for durable application data.

## Resource and restart controls

Service policy includes per-service/default memory limits, startup timeouts, idle timeouts, monitor cadence, and restart backoff. Platform enforcement must remain part of engine correctness rather than relying on the service source to behave voluntarily.

## Future service middleware

A Service REL program may eventually expose middleware phases. Unlike Module REL middleware, service middleware crosses Service Fabric/IPC and must receive an explicit capability-limited request context rather than raw server internals.

## Embedded services

An embedded service inside `server.server` remains Service REL and receives exactly the same lifecycle/capability rules as a physical `.service` file.

See [`../server.server/`](../server.server/).


## Process identity

Service REL does not run inside `backend.exe`. RBE uses one canonical sibling runtime image named `service` (`service.exe` on Windows): one Mother process plus one separate process for each active service. The same executable file is reused; RBE does not manufacture per-service `rbe-service-*-parent-*` executable aliases.

```text
backend.exe
└─ service.exe              service - mother
   ├─ service.exe           service - Auth | service.exe
   └─ service.exe           service - Cache | service.exe
```

The canonical service image is byte-identical to the backend build artifact and is SHA-256 checked before Mother receives runtime authority. Per-service authentication, liveness, resource limits, and IPC remain independent even though the processes execute the same image file.


### Operator restart behavior

Service processes are intentionally supervisor-friendly. Ending one default (`on-failure`) worker causes Mother to replace only that worker. Ending Mother causes the backend supervisor to replace Mother and rebuild the entire Service process set after the old children lose their liveness pipe.

The same behavior can be requested portably:

```text
service.exe -restart-whole
service.exe -restart-auth.service
```

(`service` without `.exe` on Unix.) Explicit service restart requests retry with bounded exponential backoff and surface sustained failures as `crash_loop_backoff` instead of busy-looping.
