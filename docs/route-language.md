# RBE Route Language Specification

Status: **living specification**

This document is the source of truth for the implemented and planned `.route` language: syntax, imports, capability boundaries, diagnostics, execution, and the relationship between `.route` and `.module` files. **Implemented** means the current RBE codebase exposes and tests the feature; **Planned** means the design is documented but not yet complete.

## 1. Design goal

`.route` is a small, JavaScript-shaped API language interpreted by the Rust backend. It is not JavaScript and does not embed Node/Bun. Routes are capability-oriented: importing a capability explicitly grants the route access to that curated surface.

Privileged, expensive, persistent, or complex work belongs in `.module` code.

## 2. File types

### `.route`

A `.route` file defines an HTTP endpoint group. It contains one route class whose methods are HTTP verbs such as `get`, `post`, `put`, `delete`, `patch`, `head`, and `options`.

Only declared HTTP methods are registered.

### `.module`

`.module` is the reusable logic layer. It is designed for arbitrary function names, multiple parameters, privileged capabilities, and heavier work. **Full `.module` execution is planned and remains separate from the currently implemented `.route` interpreter.**

## 3. Variables, statements, and expressions

The route language supports the explicit `$[name]` variable form as well as ordinary identifiers where the grammar requires a name.

The implemented route surface includes constants, return statements, expression statements, objects, arrays, literals, member access, calls, conditional/control-flow constructs, and top-level helper functions. The compiler performs semantic analysis before a route is accepted.

Unused helpers/imports can be reported as warnings; invalid routes are not turned into runnable artifacts.

## 4. Imports

Imports use one directive:

```text
:import[net]
:import[response as resp, net]
:import[net.ping as ping]
:import["module/network/uac-login"]
```

A comma separates entries. Aliases create a local binding without changing the underlying capability. Duplicate source capabilities are rejected.

Import resolution distinguishes curated built-ins from quoted module paths. `.module` loading is a planned capability even though path resolution/desugaring exists.

### Import diagnostics

An unknown built-in import is a compiler error:

```text
error[E3010]: net.health does not exist as a import — please remove `net.health` from api/health.route
```

The compiler should report the source file, line, and column whenever the diagnostic span is available.

## 5. Capability boundary

The intended boundary is:

| Capability | `.route` | `.module` | Status |
|---|---:|---:|---|
| `net` | Yes | Yes | Implemented in a deliberately small route surface |
| `json` | Yes | Yes | Basic route support implemented |
| `env` | No | Yes | Privileged/module-only design |
| `crypto` | Yes | Yes | Basic surface exists; cryptographic correctness remains a release concern |
| `encoding` | No | Yes | Planned |
| `time` | Yes | Yes | Basic support implemented |
| `http` | Yes | No | Planned surface |
| `auth` | No | Yes | Planned/feature-gated |
| `vault` | No | Yes | Planned privileged module surface |
| `request` | Yes | Yes | Planned beyond the current minimal surface |
| `storage` | No | Yes | Planned |
| `cache` | No | Yes | Planned |
| `log` | Yes | Yes | `info`/`warn` support implemented |
| `security` | Yes | Yes | Policy-controlled surface; broader implementation planned |
| `net:response` | Yes | Yes | Planned namespace |
| `private` | Yes | N/A | Implemented as read-only backend health information |

This table is a design contract and must not be read as claiming that every listed operation is implemented.

## 6. Implemented built-ins

### `net`

```text
net.ping()
```

`net.ping()` proves the route-to-Rust capability wiring and returns the engine ping result. Additional outbound networking operations are planned and must remain explicitly capability-checked.

### `json`

Basic JSON parsing/stringifying support is available to routes.

### `time`

Basic current-time functionality is available.

### `log`

`log.info(...)` and `log.warn(...)` are available for route logging.

### `private`

`private` is a backend-owned, read-only health capability. It is intentionally not a general host/runtime escape hatch.

```text
:import[private]
private.health()
```

The current health object exposes:

```text
status          backend health state
uptime          backend runtime uptime in seconds
container       null until container health is exposed here
vault           true when the backend's required Vault dependency is ready
errorReporter   null until reporter state is exposed
```

Routes cannot use `private` to read environment variables, secrets, arbitrary processes, or filesystem state.

### Unknown members

Calling an unavailable built-in member is a compiler error rather than a guaranteed request-time 500:

```text
error[E3011]: net.health() does not exist — please remove it from api/health.route
```

## 7. Route structure

```js
:import[private, net]

class Route {
    async get(req) {
        const health = private.health();
        const engine = net.ping();

        return {
            ok: true,
            status: health.status,
            uptime: health.uptime,
            container: health.container,
            vault: health.vault,
            errorReporter: health.errorReporter,
            engine: engine
        };
    }
}
```

The class is an HTTP entrypoint surface. Only supported HTTP verb methods are registered.

## 8. Compiler pipeline

```text
.route source
    ↓
lexing
    ↓
parsing + recovery
    ↓
AST
    ↓
name resolution
    ↓
import/capability validation
    ↓
semantic analysis / warnings
    ↓
diagnostics
    ↓
route registration
```

The current RBE route engine is a tree-walking interpreter rather than a complete Rust source generator. Invalid routes must not become runnable route registrations.

## 9. Diagnostics

Diagnostic classes include syntax, semantic/name-resolution, import/capability, artifact/runtime, and warnings.

Current documented codes include:

```text
E3000  capability unavailable in `.route`
E3001  duplicate import source
E3010  built-in import/function name does not exist as an import
E3011  built-in member call does not exist
E4000  route evaluation failed at runtime
```

Unknown imports use `E3010` and unknown built-in calls use `E3011`. Both diagnostics should identify the file so the fix is actionable.

Detailed errors are written to:

```text
./data/admin/compiler-error.txt
```

The file is cleared at backend boot so stale diagnostics do not survive a new run.

## 10. Boot and progress UI

Route compilation temporarily owns the visible compiler UI. The intended work accounting is three logical units per route:

```text
Parsing
Semantic analysis
Rust artifact generation
```

The UI must represent real work, adapt to terminal width, support ANSI-capable terminals and non-interactive fallback, and never require artificial sleeps merely to make progress visible.

## 11. `.module` relationship

`.module` files are not JavaScript packages. Their imports use the same capability-oriented syntax and are resolved against the backend's module root/path rules.

The intended module model is documented separately in [`module-language.md`](module-language.md). Full module loading/execution, recursive module graphs, arbitrary module function parameters, and cycle guards remain planned until the module runtime is implemented end-to-end.

## 12. Status discipline

Documentation must not call a feature **Implemented** merely because a parser accepts its syntax or a capability name exists in a registry. A feature is implemented only when the corresponding runtime behavior exists and is tested enough to be described as working.

When the compiler/runtime changes, this specification and the error book must be updated together so documentation does not drift from actual behavior.
