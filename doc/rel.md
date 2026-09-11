# REL — Runtime Engine Language

REL is RBE's application language. It is **one language with multiple source roles**: ordinary grammar belongs to REL globally, while capabilities, declarations, lifecycle, and process ownership are scoped by the source type.

## Source roles

| Source | Runtime role |
|---|---|
| `*.route` | HTTP entrypoints and request/response logic |
| `*.module` | reusable in-process backend logic |
| `*.service` | isolated managed service programs |
| `server.server` | whole-server composition and policy |

A shared grammar does not turn the four roles into one generic file type.

## Global grammar rule

Common REL infrastructure supports the language building blocks used across Route, Module, Service, and Server helper code: literals, arrays/objects, expressions, conditionals, functions, async declarations/calls where the operation supports them, classes/methods where meaningful, member access, imports, and recursive function execution.

Role-specific syntax still exists where the role itself requires it: `class Route`, `:service[...]`, `class Service`, and the root `server NAME { ... }` declaration are examples.

## Imports and capability validation

Current import shapes include built-ins, direct built-in functions, modules, aliases, and Service references:

```text
:import[json, time]
:import[net.ping as ping]
:import[module&users]
:import["./module/users"]
:import[service:mail as mail]
```

RELC validates both existence and authority. Important current rules include:

- Route REL cannot directly import Service REL; route-to-service access goes through a module boundary.
- `ENV` is readable by Module, Service, and Server REL, not Route REL by default.
- lowercase process-environment `env` is rejected in RELC-linked applications.
- `quickDB` is Service-only.
- Video Manager (`vm` / `video-manager`) is a privileged Module REL capability.
- service-to-service imports are accepted through the Service Fabric, but synchronous Service dependency cycles are rejected because current single-request workers would deadlock.

## Typed Runtime ENV — implemented

`ENV` is RBE-owned Runtime Image configuration, not `std::env`/the operating-system process environment.

```text
:import[ENV]

ENV.has("APP_NAME")
ENV.get("APP_NAME")
ENV.require("APP_NAME")
ENV.string("APP_NAME")
ENV.number("SESSION_TTL")
ENV.bool("DEBUG")
ENV.object("MAIL")
ENV.array("FEATURE_FLAGS")
```

`get()` returns `null` for a missing key. `require()` and the typed accessors fail when the key is missing or has the wrong type.

Runtime ENV preserves JSON strings, numbers, booleans, arrays, objects, and nulls. Resolution precedence is:

```text
built-in Runtime ENV
    < normal server.server env values
    < settings.json runtimeEnv
    < forced server.server env values
```

Services do not reconstruct ENV from child-process state. The parent resolves the Runtime Image snapshot and transports that typed snapshot through the supervised Service bootstrap path.

Secrets do not belong in Runtime ENV; use Vault.

## Embedded REL sources — implemented

`server.server` can contain literal embedded REL blocks. RELC extracts them before normal Server REL parsing, registers virtual source identities, then compiles each block according to its own source role.

```text
[file-start:module.Auth]
:import[ENV]

export function appName() {
    return ENV.require("APP_NAME");
}
[file-end:module]
```

Embedded routes may also provide a `path` attribute. Physical and embedded sources share one logical namespace, so two sources cannot silently claim the same logical Route/Module/Service target.

An embedded module does not receive Server REL authority merely because its bytes live inside `server.server`.

## Runtime source integrity

RELC links exact source/program snapshots into the Runtime Image. Normal Route/Module execution uses those immutable objects rather than re-reading mutable source on each request. Native route-WASM artifacts and explicit interpreter-fallback reasons are also pinned to the image.

Service process activation additionally verifies the catalog/source fingerprint supplied by the parent before trusting a `.service` body. Service Mother and Service workers start from a scrubbed process environment and receive only RBE-owned bootstrap metadata/capabilities.

See [`source-security.md`](source-security.md).

## Recursion and dependency cycles

REL allows ordinary recursive execution. The Runtime Image contains symbol-level dependency edges and recursive strongly connected groups, while `InvocationTracker` enforces runtime limits for:

- maximum call depth;
- repeated-symbol depth;
- operation count;
- cyclic waits on a value still being produced by the same invocation chain.

A symbol cycle is therefore not automatically a runtime error.

There are two important current caveats:

1. **Service dependency cycles are rejected by RELC** because synchronous Service Fabric cycles would deadlock the current worker model.
2. The older ModuleProgram compatibility validator still performs source-level module-cycle detection when constructing executable module state. RELC's symbol graph is more precise, but cyclic module imports should not yet be treated as portable/current runtime behavior.

See [`relc.md`](relc.md).

## Current execution model

RELC's image can hold both immutable REL executable AST/program snapshots and native route-WASM artifacts. The native compiler currently handles a strict static route subset; dynamic request expressions, imports, helper functions, and other unsupported constructs remain explicit interpreter fallbacks while ABI lowering expands.

That split is intentional: RBE does not label interpreted execution as “compiled WASM” just to make the architecture diagram look cooler than reality. :)
