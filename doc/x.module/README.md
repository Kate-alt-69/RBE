# `*.module` — Module REL

Module REL is reusable in-process backend logic. Modules use the global REL grammar and are compiled/loaded as reusable programs owned by the RBE backend runtime.

## Purpose

Use `.module` for:

- reusable functions
- shared backend logic
- request-independent helpers
- controlled access to service calls
- shared ENV access
- future middleware stages
- heavier logic that should not be repeated inside routes

## Grammar

Module REL receives the same higher-level grammar as the other REL source types. It is not a weaker language than Server REL.

Modules may define arbitrary functions and exports:

```text
:import[json]

function normalize(value) {
    if (value == null) {
        return null;
    }
    return value;
}

export function find(value) {
    return normalize(value);
}
```

## Imports

Modules can import built-ins, other modules, and service interfaces where policy allows.

```text
:import[json, time]
:import[module&validation]
:import[service:mail as mail]
```

The active engine already discovers and parses `.module` files, validates dependency graphs, exports functions, and resolves module-to-module references. Documentation that still labels the entire `.module` system as unimplemented is legacy and should not be treated as authoritative.

## Module-to-module dependencies

A file-level dependency cycle is not automatically an error in the target RELC architecture.

```text
A.module -> B.module -> D.module -> C.module -> A.module
```

RELC must inspect symbol-level dependencies. If `C` calls `A.FUNC_B` while the original path began in `A.FUNC_A`, there may be no recursive execution at all.

Even a real recursive function graph is not automatically invalid; see [`../relc.md`](../relc.md).

## ENV

Module REL is a primary consumer of the shared Runtime ENV.

Target import/surface:

```text
:import[ENV]

export function appName() {
    return ENV.require("APP_NAME");
}
```

Runtime ENV is owned by the root RBE process and resolved from `settings.json` plus `server.server` defaults/policy. It should be typed rather than limited to strings.

## Service calls

Modules are the normal application-facing bridge to managed services.

```text
:import[service:mail as mail]

export async function sendWelcome(user) {
    return mail.sendWelcome(user);
}
```

Service calls are asynchronous and should route through the central Service Runtime/Fabric rather than granting raw process-to-process authority.

See [`../x.service/`](../x.service/).

## Future middleware role

A module may eventually expose middleware lifecycle hooks through a reserved middleware class/contract. Module middleware executes in-process and should receive only the request/response capabilities granted by ServerPolicy.

Target phases include concepts such as:

```text
beforeRoute
afterRoute
beforeResponse
onError
```

The final syntax should use global REL grammar while keeping middleware semantics explicit and deterministic.

## Embedded modules

An embedded module in `server.server` is still Module REL:

```text
[file-start:module.Auth]
:import[ENV]
export function verify(value) { ... }
[file-end:module]
```

It can import physical or embedded modules and is compiled through the same Module REL path. See [`../server.server/`](../server.server/).

## Runtime ownership

Modules execute in the backend process, unlike `.service` programs which execute in managed child processes. That distinction affects resource isolation, IPC, restart policy, and failure containment even though both file types share REL grammar.

## Capability direction

The exact capability list evolves with implementation, but the design rule is stable: a capability must be explicitly exposed to Module REL and validated by RELC. Shared grammar never becomes an ambient host escape hatch.
