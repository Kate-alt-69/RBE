# `*.service` — Service REL

Service REL defines backend programs that execute through RBE's managed Service Runtime. It shares normal REL grammar with the other source roles, but its process lifecycle, Service Fabric authority, memory, and Service-only capabilities are deliberately separate.

## Purpose

Use `.service` for work that benefits from process isolation or an independent lifecycle, including:

- resident or wake-on-demand backend workers;
- email/background processing;
- isolated caches/search/indexing;
- CPU/memory-contained work;
- Service Fabric APIs consumed by Module REL or other services;
- Service-local probabilistic indexes such as `quickDB`.

## Declaration

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

Current modes are:

- `resident` — starts with the Service runtime and remains active until shutdown/failure;
- `on-demand` — stays dormant until a call/event wakes it;
- `hybrid` — starts resident, may sleep after its idle timeout, and wakes again when needed.

Restart policies are `always`, `on-failure`, and `never`. `instances` currently resolves to `1`; multi-instance Service REL is not implemented yet.

## Process model

Service REL does **not** execute inside `backend.exe`.

RBE packages a separately linked canonical Service executable at `./dep/service` (`./dep/service.exe` on Windows). The backend verifies that executable against the exact build-time SHA-256 and explicitly refuses the old/unsafe shape where the Service image is byte-identical to the backend executable.

Typical topology:

```text
backend.exe
└─ service.exe              Service Mother
   ├─ service.exe           Auth service worker
   ├─ service.exe           Cache service worker
   └─ service.exe           Mail service worker
```

The canonical Service binary contains the Mother/worker entrypoints, `.service` compiler/executor path, Service Fabric IPC, Runtime ENV bootstrap, CONTROL-ER recovery client, and restart controls. It does not expose the normal backend HTTP/Vault/Container/HostBootstrap/Error-Reporter-daemon boot path.

Each active service gets its own process even though the workers reuse the same canonical executable image.

## Lifecycle

A service may define:

```text
class Service {
    start(context) { }
    event(event) { }
    health(context) { }
    stop(context) { }
}
```

Current behavior:

- `start` runs after the worker has applied its resource boundary/opened IPC but before readiness is advertised;
- `event` is invoked through authenticated Service IPC and can wake on-demand/hybrid services;
- `health` probes an already-running service and does not wake an intentionally dormant service;
- `stop` runs for intentional shutdown/idle sleep when the process is healthy enough to execute it.

Lifecycle methods and exports share the same process-local Service capabilities while that worker remains alive.

## Exports and Service Fabric

Exported functions form the service's callable interface:

```text
export async function send(message) {
    ...
}
```

Module REL can call these through `service:*` imports, and Service REL can also import another Service through the authenticated Service Mother Fabric:

```text
:import[service:mail as mail]
```

The Fabric owns service discovery, activation, authenticated loopback transport, call routing, deadlines/recovery policy, and process ownership. Child workers do not receive raw peer-process authority.

### Service-to-Service cycle rule

Service-to-Service imports are implemented, but RELC rejects a synchronous dependency cycle because the current single-request worker model could deadlock:

```text
A -> B -> C -> A   // rejected for synchronous Service dependencies
```

This rule is separate from ordinary in-process REL function recursion.

## Runtime ENV

Service REL can import the same typed Runtime Image environment snapshot available to authorized Module REL:

```text
:import[ENV]

export function region() {
    return ENV.require("REGION");
}
```

The Service process does not recreate Runtime ENV from its OS environment. Backend resolves the snapshot during RELC linking and transports it through the supervised bootstrap path. Service processes start from a scrubbed process environment rather than inheriting arbitrary application/proxy/loader variables.

## Process-local memory

The `memory` capability is RAM owned by the active service process. Current operations include:

```text
memory.get(key)
memory.set(key, value)
memory.delete(key)
memory.clear()
memory.len()
memory.isEmpty()
```

A cold wake after a worker has exited creates fresh process memory. Treat it as a cache, not durable storage.

## `quickDB`

`quickDB` is an explicit Service-only capability for Bloom-family probabilistic membership indexes:

```text
:import[quickDB]
```

Its filters are also process-local accelerators and must be rebuilt/sealed after a cold process start. They never replace the authoritative database or uniqueness/transaction constraints.

See [`../quickdb.md`](../quickdb.md).

## Resource enforcement

Per-service policy includes memory limits, startup/idle timeouts, monitor cadence, and restart backoff.

Current hard memory enforcement uses:

- Unix: `RLIMIT_AS`;
- Windows: a private Job Object with `JOB_OBJECT_LIMIT_PROCESS_MEMORY`.

If a non-zero requested memory limit cannot be installed, startup fails instead of silently running unbounded.

## Crash supervision and CONTROL ER

Mother independently supervises each service. Unexpected exits are evaluated against restart policy and use bounded exponential backoff. A stable-running window resets accumulated crash attempts, and a scheduled replacement rechecks state before spawning so a concurrent wake/recovery cannot create a duplicate worker.

When CONTROL Error Reporter authority is available, an unexpected service exit can also be submitted as a bounded authenticated recovery report. ER may return `Restart`, `Stop`, or `Default` for that service, but it does not spawn the process itself and it cannot shorten the Service Manager's minimum backoff.

If CONTROL ER is unavailable, BASIC, stale, crashed, unauthenticated, or times out, Mother falls back to the local `.service` restart policy. Planned shutdowns and explicit operator restarts do not masquerade as unexpected crashes.

Service Mother itself is a critical child supervised by backend; an unexpected Mother exit rebuilds the Service process tree with bounded recovery.

## Operator restart controls

The same ownership model can be exercised explicitly:

```text
service.exe -restart-whole
service.exe -restart-auth.service
service.exe --restart-service auth.service
```

Use `service` without `.exe` on Unix.

Ending one default `restart = on-failure` worker causes Mother to replace that worker. Ending Mother causes backend to replace Mother; child liveness channels cause the old worker tree to terminate instead of becoming orphaned authority.

## Embedded Service REL

RELC can extract and compile a `[file-start:service.NAME]` block as Service REL with a virtual `SourceId`. It receives Service-role capability/semantic validation, not Server REL privileges.

Managed process activation still depends on the Service runtime/catalog contract, so embedded compilation should not be interpreted as permission for an arbitrary block to bypass Service catalog/fingerprint supervision.

## Still intentionally unfinished

The current Service runtime does not promise:

- `instances > 1` or Service load balancing;
- arbitrary shell/child-process execution from Service REL;
- durable persistence for process-local `memory`/`quickDB`;
- distributed Service placement;
- a general external event bus automatically feeding `Service.event()`;
- a completed Service-backed custom middleware lifecycle.
