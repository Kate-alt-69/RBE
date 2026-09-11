# `*.module` — Module REL

Module REL is reusable **in-process** backend logic. Module programs are linked into the Runtime Image and execute inside the backend rather than receiving their own Service process.

## Purpose

Use `.module` for reusable functions, validation/transformation logic, request-independent helpers, controlled Service calls, typed Runtime ENV access, and privileged module-scoped facilities such as Video Manager.

```text
:import[json, ENV]

function normalize(value) {
    if (value == null) {
        return null;
    }
    return value;
}

export function find(value) {
    return {
        app: ENV.get("APP_NAME"),
        value: normalize(value)
    };
}
```

## Imports

Modules can import built-ins, other modules, and Service interfaces:

```text
:import[json, time]
:import[module&validation]
:import[service:mail as mail]
:import[service:search.find as lookup]
```

RELC validates logical targets/capabilities, and the executable ModuleProgram validates requested exports/interfaces before the application becomes runnable.

## Module -> Service

Module REL is the normal application-facing bridge to Service REL:

```text
:import[service:mail as mail]

export async function sendWelcome(user) {
    return mail.sendWelcome(user);
}
```

Service calls are routed through the central Service Runtime/Fabric rather than giving the module raw Service process addresses/tokens.

See [`../x.service/`](../x.service/).

## Runtime ENV — implemented

Module REL can explicitly import the typed Runtime Image ENV snapshot:

```text
:import[ENV]

export function region() {
    return ENV.require("REGION");
}
```

Available accessors are `has`, `get`, `require`, `string`, `number`, `bool`, `object`, and `array`. Types are preserved from linked configuration.

`ENV` is not a secret store and is not the process environment. Use Vault-backed capabilities for credentials.

## Video Manager — Module-only privileged capability

Video Manager is exposed to Module REL through explicit imports:

```text
:import[vm]
:import[video-manager as media]
:import[video-manager.status as videoStatus]
```

The legacy name `video` is intentionally rejected. Module ownership is used to scope asset/job access; language code does not receive raw media filesystem paths, arbitrary FFmpeg arguments, or raw network/process execution.

See [`../video-manager.md`](../video-manager.md).

## Module dependency graph

RELC builds symbol-level dependency metadata and recursive groups. That model intentionally distinguishes a source import cycle from a true function recursion cycle.

However, the current executable `ModuleProgram` compatibility validator still performs source-level module cycle detection while constructing runtime module state. Therefore:

- ordinary function recursion is supported within runtime budgets;
- symbol recursion metadata exists in the Runtime Image;
- **cyclic module-import graphs should still be avoided today** because the compatibility validator can reject them during boot/runtime construction.

This is one of the remaining areas where the RELC graph is ahead of the older execution compatibility layer.

## Embedded modules

RELC can extract a Module REL block from `server.server`, register it with a virtual `SourceId`, parse it with Module REL rules, and include it in the Runtime Image:

```text
[file-start:module.Auth]
:import[ENV]

export function appName() {
    return ENV.require("APP_NAME");
}
[file-end:module]
```

Physical and embedded modules share the same logical namespace. Duplicate logical targets are rejected.

## Runtime ownership

Module executable programs are stored in the immutable Runtime Image and ModuleProgram can be constructed from those linked snapshots. Normal request-time module resolution therefore does not need to reopen the module source file.

This is a different isolation model from `.service`: a module failure remains in the backend execution domain, while a Service worker has its own process lifecycle, resource boundary, restart policy, and IPC.

## Middleware status

Reusable application logic can live in Module REL, but a general user-defined Module middleware lifecycle (`beforeRoute`, `afterRoute`, etc.) is not a completed runtime contract yet. Native server middleware is configured through `server.server`/`MiddlewarePlan` today.

## Capability rule

The exact built-in operation set evolves, but the rule does not: a host capability must be explicitly importable by Module REL and enforced by the compiler/runtime. Common REL grammar never grants ambient host authority.
