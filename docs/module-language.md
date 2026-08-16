# `.module`

> # 🚨 `.module` IS NOT IMPLEMENTED 🚨
>
> **Actual `.module` files are NOT implemented.**
>
> RBE currently does **not**:
> - parse `.module` files as executable source files;
> - discover `.module` files during boot;
> - compile `.module` files;
> - interpret or execute `.module` files;
> - generate `.module` artifacts;
> - load `.module` files as executable modules;
> - export functions from `.module` files;
> - import/export executable `.module` dependencies;
> - build or execute a `.module` dependency graph;
> - perform recursive `.module` loading or cycle detection.
>
> References to `.module` currently describe **planned/reserved language design, import/export syntax, capability boundaries, and future interoperability only**. An existing `.module` file must **not** be treated as a working executable module.
>
> **Do not confuse a parsed/planned module import reference with an implemented `.module` file system.**
>
---

# RBE Module Language Specification

**Status:** **PLANNED / RESERVED — NOT IMPLEMENTED**

This document defines the intended `.module` language and its relationship to `.route`. It deliberately separates the future language design from the current runtime.

## 1. Purpose

`.module` is intended to become RBE's reusable higher-power logic layer. It is designed for work that should not live directly inside an HTTP route: reusable functions, multiple parameters, privileged capabilities, persistent storage, advanced networking, and heavier computation.

`.route` remains the constrained HTTP entrypoint layer.

The intended relationship is:

```text
.route
  ↓ calls
.module
  ↓ may call
built-in capabilities / other modules
```

**This relationship is design-only today.** No executable `.module` call path currently exists end-to-end.

## 2. What makes `.module` different from `.route`

### `.route`

- HTTP entrypoint methods are restricted to route verbs (`get`, `post`, `put`, `delete`, `patch`, `head`, `options`).
- Request handling is the primary purpose.
- Privileged capabilities are deliberately restricted.
- The route capability surface is small and auditable.

### `.module`

Planned behavior:

- Function names are arbitrary identifiers.
- Functions may accept zero or more named parameters.
- Functions are reusable from routes and other modules.
- Module-only capabilities such as environment, Vault, storage, and cache belong here when implemented.
- Modules may import other modules.
- Recursive dependency loading requires cycle detection.

Example **planned** module syntax:

```js
:import[json]

function findUser(id) {
    // planned module implementation
}

function formatUser(user, includePrivate) {
    // planned module implementation
}
```

A route could **eventually** call:

```js
:import["./module/users"]

class Route {
    async get(req) {
        return users.findUser(req.params.id);
    }
}
```

That example is **not runnable today** because `.module` execution is not implemented.

## 3. Parameters

Unlike a route HTTP handler, a module function is intended to support multiple named parameters.

Planned syntax:

```js
function get(id) {
    // ...
}

function set(key, value) {
    // ...
}

function findByEmail(email, includePrivate) {
    // ...
}
```

Arguments are intended to be passed positionally:

```js
storage.get(id)
storage.set(key, value)
```

Arity and argument compatibility should eventually be validated during semantic analysis.

## 4. Imports and exports

The future `.module` system is intended to use the same import directive style as `.route`:

```text
:import[net]
:import[json]
:import["./other"]
:import[module&storage]
```

Quoted paths and `module&name` shorthand are **reserved/planned module references**. They do not currently cause a `.module` file to be loaded and executed.

When module execution is implemented, exported functions will form the callable surface of a module. The exact explicit `export` syntax is **reserved for the future implementation** and must not be assumed to work today.

## 5. Module binding names

The intended default binding behavior is:

```text
:import[module&storage]
```

binds the module as:

```text
storage
```

so a future caller can write:

```text
storage.get(id)
```

Explicit aliases are intended to follow the same import-grammar rules already established for `.route`.

Again: **binding syntax being documented does not mean a `.module` file is currently executable.**

## 6. Capability boundary

The intended module capability surface is:

| Capability | `.module` | Purpose |
|---|---:|---|
| `net` | Yes | Basic and advanced networking as policy allows |
| `json` | Yes | Structured data |
| `env` | Yes | Privileged environment access |
| `crypto` | Yes | Cryptographic primitives and heavier operations |
| `encoding` | Yes | Encoding/decoding utilities |
| `time` | Yes | Time and duration operations |
| `auth` | Yes | Authentication functionality when enabled |
| `vault` | Yes | Privileged secret access under Vault ACLs |
| `request` | Yes | Advanced request construction |
| `storage` | Yes | Persistent storage |
| `cache` | Yes | Cache control |
| `log` | Yes | Logging |
| `security` | Yes | Policy-controlled security operations |

`private` is intentionally **not** a general module escape hatch. The current `private.health()` surface is specifically a route health capability; future module access must be explicitly designed and policy-controlled rather than assumed.

## 7. Function calls

Once module execution is implemented, a caller is intended to use:

```text
moduleName.functionName(arg1, arg2)
```

The future compiler should reject:

- unknown module functions;
- incorrect argument counts;
- invalid argument forms;
- module values used as ordinary values when only a callable export exists;
- calls to capabilities unavailable under the current policy.

Unknown module members should produce a source-aware diagnostic rather than a silent runtime failure.

## 8. Module-to-module imports

Modules are intended to be composable:

```text
module A
  ↓ imports
module B
  ↓ imports
module C
```

The future loader must maintain a dependency graph and detect cycles:

```text
A → B → C → A
```

A cycle must become a deterministic compiler/load error. The loader must not recurse indefinitely or deadlock the backend.

## 9. Execution model

The planned first implementation should reuse the existing route interpreter infrastructure where practical. A module function would be parsed, semantically analyzed, and evaluated against a local parameter environment.

The intended call sequence is:

```text
.route request
    ↓
module lookup
    ↓
module content-hash lookup/cache
    ↓
parse if needed
    ↓
semantic analysis
    ↓
cycle/recursion checks
    ↓
module function lookup
    ↓
argument binding
    ↓
evaluate function body
    ↓
return value to route
```

**None of that execution pipeline is currently implemented end-to-end.**

## 10. Caching

Modules are intended to participate in the same content-hash caching model used by routes once module loading exists.

The eventual cache key must change when the module's own source changes. The dependency graph must also invalidate callers when an imported module changes, otherwise routes could execute stale dependency code.

## 11. Security

Module capabilities are privileged by design. In particular:

- `env` must not become ambient access for routes;
- `vault` must continue to respect Vault ACLs;
- `storage` and `cache` must be scoped by backend policy;
- filesystem/process access must not be exposed simply because a module is more powerful than a route;
- capability imports must remain explicit and auditable.

The existence of a `.module` file must never grant arbitrary Rust or operating-system access.

## 12. Diagnostics

When `.module` execution is eventually implemented, module diagnostics should use the same compiler diagnostic system as routes.

Planned examples:

```text
error[E3010]: vault.import does not exist as a import — please remove `vault.import` from module/uac/status.module
```

```text
error[E3011]: storage.missing() does not exist — please remove it from module/uac/status.module
```

The diagnostic should identify the exact module source file and, when available, its line and column.

Module errors must be collected without hiding unrelated route/module errors.

## 13. Error isolation

A broken module must not silently become a valid route artifact.

The intended future compiler behavior is:

```text
scan route/module files
    ↓
parse recoverable files
    ↓
resolve module graph
    ↓
collect syntax/import/semantic errors
    ↓
exclude invalid artifacts
    ↓
report all diagnostics
    ↓
abort boot if required errors remain
```

A module imported by a route should be validated before that route is registered as runnable.

## 14. Planned module filesystem

The intended project layout is:

```text
./backend(.exe)
./settings.json
./api/
    └── ... .route
./module/
    ├── storage.module
    ├── users.module
    └── uac/
        └── status.module
```

The `/module/` directory is intended to be a sibling of the compiled backend binary, not an embedded JavaScript package directory.

## 15. Implementation status

The following are **PLANNED / RESERVED**, not claims of current end-to-end support:

- actual `.module` file discovery;
- actual `.module` source parsing as a separate executable file type;
- arbitrary module function parameters;
- module function execution;
- module exports;
- module-to-module recursive loading;
- dependency graph construction;
- cycle detection;
- module content-hash dependency invalidation;
- module capability enforcement at runtime;
- module-specific tests;
- complete module diagnostics;
- module artifact generation.

Some route-engine infrastructure may already recognize or resolve module-like import references. **That does not mean the referenced `.module` file is parsed, loaded, exported, executed, or compiled.**

## 16. Relationship to the route specification

The route specification remains the authority for HTTP entrypoints and route-only restrictions. This document is the authority for the **planned** reusable module layer.

When `.module` execution is actually implemented, this document must be updated together with `docs/route-language.md`, `docs/route-error-book.md`, and `api/README.md` so the documented capability matrix and actual runtime behavior remain synchronized.
