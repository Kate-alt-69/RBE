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

Each active service is a separate OS process. RBE now uses one canonical dependency executable at `./dep/service` (`./dep/service.exe` on Windows). The Service Mother and every active Service REL worker are separate processes of that same canonical image; per-service executable aliases under `.runtime/process/` are no longer created.

Typical Windows process layout:

```text
backend.exe
└─ service.exe              service - mother
   ├─ service.exe           service - Auth | service.exe
   ├─ service.exe           service - Cache | service.exe
   └─ service.exe           service - Mail | service.exe
```

The packaged `service.exe` is now a separately linked executable target. It contains the Service Mother/worker entrypoints, `.service` compiler/executor path, Service Fabric IPC, Runtime ENV bootstrap, CONTROL-ER recovery client, and restart controls, but it does not expose the normal backend API/Vault/container/HostBootstrap/ER-daemon boot path. The release builder compiles `service` first, passes its exact artifact to the backend build, and backend embeds that Service SHA-256. At runtime backend requires `./dep/service.exe`/`./dep/service` to match that build-time digest and explicitly refuses a Service image whose bytes are identical to backend. Backend never repairs, copies, or hard-links itself into the Service path.

The child binds a loopback-only TCP IPC endpoint and prints one readiness record to stdout. The parent consumes that record, verifies the service identity, then continuously drains the remaining stdout stream into structured logging.

IPC uses a random 256-bit per-process token. Supported internal operations currently include health, exported function calls, lifecycle events, service memory operations, and shutdown.

On Unix, the configured memory limit is enforced with `RLIMIT_AS`. On Windows, each service host creates a private Job Object, applies `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, and assigns itself before advertising readiness. A non-zero `memoryLimitMb` therefore fails service startup if the platform cannot install the requested hard limit instead of silently running unbounded.

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

Service-to-service imports are supported through the authenticated loopback Service Mother Fabric. Child services receive the Mother address as non-secret process metadata and receive the Mother authentication token only through the inherited stdin bootstrap pipe. Direct synchronous service dependency cycles are rejected because they would deadlock single-request service workers; normal in-process REL recursion remains a separate concept.

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

- additional Windows Job Object limits beyond per-process memory, such as CPU or process-count limits
- `instances > 1` or service load balancing
- arbitrary child-process/shell execution from `.service`
- durable persistence for process-local `memory`
- direct `.service -> .service` imports
- a general external event bus feeding `Service.event()` automatically
- automatic distributed service placement

The Video Manager is a separate global runtime subsystem; see `docs/video-manager.md`.


## Process restart controls

The Service runtime intentionally uses OS-process supervision. With the default `restart = on-failure`, terminating one worker through Task Manager, `kill`, `htop`, or another process manager is treated as a failed worker and Mother restarts only that service. Terminating Mother closes every child parent-liveness pipe; the backend supervisor recreates Mother and therefore recreates the complete Service process tree.

Operators can request the same lifecycle explicitly without exposing Mother authentication:

```text
./service.exe -restart-whole
./service.exe -restart-auth.service
./service.exe --restart-service auth.service
```

On Unix the executable is `./service` instead of `./service.exe`. Restart commands write a short-lived atomic request under the binary-relative RBE admin directory. Mother consumes the request and remains the only component allowed to create/replace Service workers.

An explicit per-service restart overrides that service's `restart = never` crash policy for the requested restart. If replacement startup keeps failing, RBE retries with bounded exponential backoff rather than a hot loop; after repeated failures the snapshot state becomes `crash_loop_backoff`, while `restartAttempts` shows continued recovery attempts.


## HostBootstrap and Error Reporter authority

On Linux, normal RBE startup has a Phase 0 `HostBootstrap` boundary before any
runtime credential is created, read, migrated, rotated, modified, or persisted.
The bootstrap scripts are normal `.sh` source files for debugging, embedded in
the backend at compile time, and piped to `/bin/sh` through stdin; RBE does not
extract temporary script files. The bootstrap first reuses a working
`org.freedesktop.secrets` provider, otherwise starts/provisions D-Bus plus a
Secret Service provider through a supported package manager, then verifies a
disposable write/read/delete credential. Failure aborts normal boot before
Vault, container signing material, Communication keys, or user Services start.
Production output is intentionally generic; a debug build launched with
`-debug` outside `RBE_ENV=production` exposes bounded script diagnostics.

The Error Reporter has two authority levels. `BASIC` keeps diagnostics and
report signing but cannot authorize managed restarts. `CONTROL` is issued only
by a parent that already owns `HostBootstrapReady`; each ER generation receives
fresh signing/control material over an inherited one-shot stdin pipe. ER COM
keys are never stored in a file, argv, or environment variable. CONTROL reports
also carry structured diagnostic context (`why`, `how`, activity source,
process identity, and bounded stack/message metadata) for later recovery-policy
decisions. Actual process execution remains owned by the relevant supervisor;
CONTROL is authority to decide/authorize recovery, not permission to spawn an
arbitrary executable.


### CONTROL ER recovery decisions

A verified HostBootstrap now issues one per-backend-generation ER CONTROL key in
RAM. The backend sends that same capability independently to the Error Reporter
and Service Mother through inherited one-shot bootstrap pipes. It never appears
in a command line, environment variable, or key file. Refreshing the ER rotates
its ephemeral report-signing key while retaining the backend-generation recovery
capability, so Mother does not need to be restarted merely because ER refreshes.

For an unexpected `.service` exit, Mother sends CONTROL ER a bounded,
authenticated `ServiceExitReport` containing only process/runtime metadata:
service identity, `.service` filename, PID, exit code or Unix signal, restart
policy, mode, uptime, prior restart attempts, active-call count, idle duration,
and a non-sensitive last-operation label such as `call:lookup_user`. Arguments,
request bodies, event payloads, credentials, Runtime ENV values, and database
rows are never included. CONTROL ER returns a service-local Restart/Stop/Default
directive and records a signed detailed decision report including `why`, `how`,
and `what_was_doing`.

The Service Manager waits only a bounded time for CONTROL ER. Missing, BASIC,
crashed, unauthenticated, stale, or timed-out ER responses fall back to the
existing local `.service` restart policy. ER can request a longer delay but
cannot shorten the manager's exponential backoff or exceed its configured cap.
This path can only decide recovery for the service that exited; it cannot turn
one service crash into a whole-RBE restart. Explicit operator restarts and
planned shutdowns stay outside the unexpected-crash decision path.

### CONTROL ER critical-process supervision

CONTROL ER recovery protocol v2 also accepts a bounded `ProcessExitReport` for
critical runtime supervisors. Service Mother is the first critical process wired
to this path. On an unexpected Mother exit the backend records the stable process
identity, PID, portable exit code or Unix signal, supervisor observation error
when one exists, uptime, previous replacement attempts, runtime phase, a fixed
non-sensitive activity label, the service-catalog fingerprint, Runtime ENV key
count, and supervision scope. Runtime ENV values, service call arguments,
credentials, request bodies, database rows, and arbitrary child memory are not
included.

The report is authenticated with the same per-backend-generation CONTROL key as
`.service` recovery requests. ER records `why`, `how`, `what_was_doing`, the full
bounded metadata report, and its signed Restart/Stop/Default decision in
`er-process-decisions.log`; `error-reporter-status.json` exposes a cumulative
`recoveryDecisionCount` for the current ER process generation.

Service Mother remains a critical root. An unexpected Mother exit is locally
restartable even if it returned exit code 0. CONTROL ER may raise the minimum
replacement backoff (for example during a crash loop), but its delay is capped by
the backend's Mother maximum. Missing/BASIC/dead/timed-out ER falls back to the
local Mother supervisor. A `Stop` response for an unexpected critical Mother is
treated as unsafe and ignored, so an ER bug cannot permanently remove the
`.service` tree. Planned backend shutdown bypasses this crash-decision path.

### CONTROL ER Vault child recovery

The separate Vault child is also connected to the critical-process recovery
boundary without making `vault-process` depend on backend/ER. `vault-process`
defines a neutral bounded recovery-authority interface; backend adapts that
interface to the same per-backend-generation CONTROL ER capability from a
separate `vault-process-io` thread.

When the Vault child exits unexpectedly, the report contains only its PID,
portable exit code or Unix signal, uptime, prior recovery attempts, observation
phase, and a fixed non-sensitive operation class such as `credential-get` or
`credential-set`. Backend adds only fixed runtime context (`credential-runtime`,
`stdio-json`, and `vault-process-io`). Credential names, callers, credential
values, session tokens, Secret Service data, Vault ACL contents, Runtime ENV
values, and protocol request/response bodies are never sent to ER.

Vault keeps final recovery ownership. Local recovery uses exponential backoff
from 200 ms to a 30-second cap. CONTROL ER may raise the minimum delay but cannot
exceed that cap or permanently stop the critical Vault child; an unavailable,
BASIC, failed, or timed-out ER decision falls back to the local Vault recovery
path. A scheduled `refresh_process()` remains an intentional restart and resets
unexpected-crash attempts rather than being classified as a crash.

### Error Reporter self-crash postmortems

ER cannot safely ask itself whether its own dead process should restart. The
backend therefore remains the final recovery owner for the Error Reporter daemon
and uses a bounded local exponential replacement delay (500 ms through 30 s).
A process that survives the 60-second stable window resets the crash streak.
Scheduled ER refreshes remain planned maintenance and do not create crash
postmortems or inflate the crash counter.

For every unexpected ER exit, wait failure, spawn failure, or CONTROL bootstrap
failure, backend writes one normal `error-client` issue into the existing bounded
queue. The replacement ER later consumes and signs that record. The structured
postmortem says `why`, `how`, `whatWasDoing`, child PID when available, authority
mode, exit code or Unix signal, uptime, failure streak, and the local recovery
backoff. It explicitly records that no synchronous CONTROL-ER decision was
possible because the decision engine was the failed process itself.

The self-postmortem contains no CONTROL key, report-signing key, queue contents,
issue bodies, Runtime ENV values, credentials, request data, or arbitrary process
memory. It goes through `error-client`'s normal redaction, de-duplication, and
bounded queue path before the replacement ER signs it. This avoids an ER
self-dependency while still preserving enough failure chronology for later
diagnostics.

