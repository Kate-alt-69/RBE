# RBE `.service` Runtime

This document describes the `.service` implementation that exists in the RBE engine today. It deliberately separates implemented behavior from planned runtime work.

## Purpose

`.service` files are long-lived or wake-on-demand backend programs executed by RBE in isolated child OS processes. They use RBE's restricted module evaluator for executable bodies; they are not arbitrary JavaScript and they do not expose a generic process-spawn or shell API.

Services are discovered recursively below the configured service directory before normal backend startup completes. A compiler or executable-body parse error aborts backend boot and writes concrete diagnostics to the admin `service-compiler-error.txt` log.

## Declaration

A service starts with a `:service[...]` declaration. Current metadata includes:

```text
:service[
  name = cache,
  title = "Cache Service",
  mode = hybrid,
  restart = on-failure,
  memoryLimitMb = 256,
  startupTimeoutMs = 10000,
  idleTimeoutMs = 300000,
  instances = 1
]
```

Supported `mode` values are:

- `resident` — starts with the backend and remains resident until shutdown or failure.
- `on-demand` — starts dormant and is activated by the first service call/event.
- `hybrid` — starts with the backend, may become dormant after its idle timeout, and wakes on the next call/event.

The default mode is `resident`.

Supported restart policies are `always`, `on-failure`, and `never`. The default is `on-failure`. Restart policy controls automatic recovery from an unexpected process exit; it does not prevent an explicit call/event from waking an on-demand or hybrid service.

`instances` is currently required to resolve to `1`; multi-instance services are not implemented yet.

## Config-backed defaults

When a service omits a per-file override, values come from the typed `services` configuration. Current built-in defaults are:

- memory limit: 256 MiB
- startup timeout: 10,000 ms
- idle timeout: 300,000 ms
- monitor interval: 1,000 ms
- maximum restart backoff: 30,000 ms

The mother process and service child both load the same typed settings so omitted defaults do not silently change across the process boundary.

## Process model

Each active service is a separate OS process. RBE launches the same backend executable through a service-specific alias under `.runtime/process/` and starts it with internal `--service-host`, `--service-file`, and authenticated token arguments.

The child binds a loopback-only TCP IPC endpoint and prints one readiness record to stdout. The parent consumes that record, verifies the service identity, then continuously drains the remaining stdout stream into structured logging.

IPC uses a random 256-bit per-process token. Supported internal operations currently include health, exported function calls, lifecycle events, service memory operations, and shutdown.

On Unix, the configured memory limit is enforced with `RLIMIT_AS`. Equivalent Windows Job Object enforcement is still pending.

## Service execution

Exported functions execute in the same restricted async evaluator used by `.module` programs. Service-specific host capabilities are injected explicitly rather than becoming ambient powers of the evaluator.

The currently implemented service-local `memory` capability supports:

- `memory.get(key)`
- `memory.set(key, value)`
- `memory.delete(key)`
- `memory.clear()`
- `memory.len()`
- `memory.isEmpty()` / `memory.is_empty()`

This memory is process-local RAM. A cold activation after a dormant service has exited creates a new service process and therefore a new in-process memory store. Durable service state must be implemented explicitly through a persistent capability/data system; process-local memory should not be treated as durable storage.

Service-to-service imports are rejected in `.service` files. Service calls are intended to flow through the module/mother runtime rather than allowing a service mesh with ambient cross-process authority.

## Lifecycle class

A `.service` may define:

```text
class Service {
  start(context) {}
  event(event) {}
  health(context) {}
  stop(context) {}
}
```

Each lifecycle method accepts zero or one parameter. Methods are optional.

Implemented semantics:

- `start` executes after the child applies its resource limit and opens its loopback listener, but before readiness is advertised. A failing `start` prevents that process from becoming ready.
- `event` is invoked through authenticated service IPC. `ServiceManager::event` wakes an on-demand/hybrid service when necessary.
- `health` is invoked through authenticated IPC for an already-running service. Health probing does not wake dormant services.
- `stop` runs when RBE intentionally shuts the service down, including backend shutdown and hybrid/on-demand idle sleep. A hard crash cannot run `stop`.

Lifecycle methods and exported functions share the same service evaluator and process-local host capabilities, so `start`, `health`, `event`, `stop`, and exported calls can observe the same in-process `memory` state while that child remains alive.

## Hybrid and on-demand idling

The manager tracks active calls and last user activity. A wakeable service becomes eligible for intentional shutdown only when it has no active calls and its idle timeout has elapsed.

An internal health probe temporarily counts as an active operation so the idle monitor cannot shut the process down while it is being checked, but the probe does not refresh the service's idle clock. Repeated `/health` polling therefore does not keep a hybrid service alive indefinitely.

An intentional idle shutdown is not considered a crash and does not trigger restart policy handling. A later service call/event can activate the process again.

## Crash supervision

Each managed service has an independent monitor. Unexpected exits are evaluated against the restart policy. Restarts use exponential backoff starting at 250 ms and capped by the configured maximum backoff.

A process that stays alive for the stable window resets accumulated restart attempts. Before a delayed restart actually spawns a child, the monitor re-checks the service under the manager mutex. If another caller already revived the service during the backoff window, the scheduled restart is superseded instead of creating a duplicate child.

## Runtime health

Service snapshots expose runtime state and lifecycle health separately. States are `dormant`, `running`, `restarting`, `stopped`, and `unknown`.

Readiness rules are currently:

- dormant on-demand/hybrid service: ready without waking it
- running service: ready only when authenticated health IPC returns a response whose top-level `ok` is explicitly `true`
- restarting/stopped/unknown service: not ready

Snapshot entries include whether a health check was performed, the raw health response when available, and a health error when IPC/lifecycle health failed.

Backend `/health` aggregates these readiness values. Expected dormancy therefore does not make the backend unhealthy, while a running service whose `Service.health()` reports failure does.

Inside the service host, lifecycle `health()` return interpretation is:

- no `health()` method: healthy
- boolean: that boolean
- object containing `ok`: that boolean
- other values, or an object without `ok`: healthy

The host wraps that result into its top-level health response; the manager requires the resulting top-level `ok: true`.

## Compiler and startup failures

The metadata compiler emits typed `SVC1xxx` errors for problems such as missing declarations, duplicate service names, invalid modes/restart policies, invalid numeric fields, unsupported instance counts, or invalid idle configuration.

Executable-body validation also runs before service processes are launched. Backend startup persists the rendered diagnostics to the admin service compiler log, prints the concrete filename/location, and aborts startup. Interactive terminals retain the `Exit? : <enter>` pause; non-interactive CI/process environments do not hang on that prompt.

## Not implemented yet

The following should not be inferred from the current runtime:

- Windows Job Object memory/process limits
- `instances > 1` or service load balancing
- arbitrary child-process/shell execution from `.service`
- durable persistence for process-local `memory`
- direct `.service -> .service` imports
- a general external event bus feeding `Service.event()` automatically
- automatic distributed service placement

The Video Manager is a separate global runtime subsystem; see `docs/video-manager.md`.
