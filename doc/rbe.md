# RBE

RBE is a Rust backend engine built around a controlled application runtime instead of an unrestricted general-purpose JavaScript/TypeScript host. Application code is written in **REL — Runtime Engine Language** and linked by **RELC — Runtime Engine Language Compiler** into an immutable **Runtime Image** during boot.

RBE currently recognizes four REL source roles:

- `*.route` — HTTP entrypoints and request/response logic.
- `*.module` — reusable in-process backend logic.
- `*.service` — isolated programs managed by the Service Runtime/Fabric.
- `server.server` — root server composition, policy, Runtime ENV defaults/FORCE values, middleware selection, and embedded REL sources.

## What RBE does today

Normal boot builds one application image from the server root, discovered physical REL files, embedded REL blocks, the validated Service catalog, and typed settings.

```text
settings.json
server.server
api/**/*.route
module/**/*.module
service/**/*.service
        |
        v
       RELC
        |
        +-- source/import/capability validation
        +-- symbol + dependency graph
        +-- Runtime ENV resolution
        +-- ServerPolicy resolution
        +-- MiddlewarePlan lowering
        +-- route WASM compilation where supported
        |
        v
   Runtime Image
        |
        +-- HTTP/Route runtime
        +-- Module runtime
        +-- Service Runtime/Fabric
        +-- Container execution runtime
```

The backend then builds Route/Module execution from the image snapshot rather than reopening mutable `.route`/`.module` source on every request. Service activation also rechecks the parent-validated Service source fingerprint before a child is trusted.

## Why the source roles remain separate

REL grammar is shared, but runtime authority is not:

- Route REL owns HTTP handlers but cannot control the listener or directly call Service REL.
- Module REL owns reusable in-process exports and is the normal bridge to managed services and privileged module-only facilities such as Video Manager.
- Service REL owns process lifecycle, process-local memory, Service Fabric calls, and Service-only facilities such as `quickDB`.
- Server REL owns whole-server policy, Runtime ENV defaults/FORCE values, native middleware configuration, server status, and embedded source composition.

See [`compatibility.md`](compatibility.md) for the current matrix.

## Runtime Image ownership

The Runtime Image is the authoritative linked application snapshot. It currently includes:

```text
imageId / sourceHash
ServerPolicy
typed RuntimeEnv
route/module/service source identities
immutable executable REL program snapshots
symbol table + dependency graph + recursive groups
MiddlewarePlan
service assignments
capability metadata
native route-WASM artifacts
explicit route interpreter-fallback reasons
```

`RuntimeImageSlot` supports immutable snapshots and atomic image replacement. Full automatic source watching/hot-reload orchestration is still separate work; the slot itself is already transactional.

## Process isolation

RBE deliberately splits authority across processes rather than putting every subsystem into `backend.exe`:

- `backend` owns HTTP, the active Runtime Image, shared state, and top-level supervision.
- `service`/`service.exe` is a separately linked canonical Service executable. One Mother process supervises separate Service REL worker processes.
- `container`/`container.exe` is a standalone execution service. Its Controller launches persistent per-Environment child processes, which in turn launch disposable WASM workers for untrusted execution.
- Vault uses a supervised child process boundary.
- Error Reporter/CONTROL ER participates in bounded crash-recovery decisions without taking arbitrary spawn authority away from the owning supervisors.

See [`runtime.md`](runtime.md).

## Configuration and policy

`settings.json` remains the operator/deployment configuration input, but it is no longer the only policy layer. Server REL is resolved with explicit precedence:

```text
built-in defaults
    < normal server.server values
    < settings.json overlays
    < server.server FORCE values
```

Hard engine safety ceilings remain above all of those and cannot be forced away.

Typed Runtime ENV uses the same basic precedence model and remains JSON typed. It is not the operating-system process environment and is not a secret store. Credentials belong in Vault.

## Native route compilation

RELC now contains a real `.route -> WebAssembly` compiler for a deliberately small native subset. At present, a route can compile natively when it has exactly one HTTP method, no imports/helper functions, and returns one static JSON-literal value. Unsupported/dynamic routes are recorded as **explicit interpreter fallbacks** rather than being mislabeled as WASM.

The artifact bytes, SHA-256, ABI/compiler versions, and fallback reason are pinned into the Runtime Image. The current HTTP route execution path still retains the immutable REL evaluator path while native dispatch coverage is expanded.

## Reliability and operations

RBE now includes bounded supervision/recovery for critical children, a separate Container runtime with durable artifact/execution metadata, service restart policies, server status gating, and an authenticated loopback Control Room. The Control Room is intentionally served on a separate loopback listener rather than exposed directly on the public API listener.

## Is RBE worth using?

RBE is useful when a project benefits from explicit backend authority boundaries instead of giving application code ambient host access. Its main value is the integration of language/compiler rules, policy resolution, process isolation, native middleware, service supervision, sandbox execution, media infrastructure, and security controls into one runtime contract.

It is still evolving. The docs in this directory call out partial areas—especially full route-to-WASM coverage, complete plan-controlled middleware behavior, source-less sealed Runtime Images, multi-instance Service REL, and stronger platform-specific Container isolation—without pretending they are finished.
