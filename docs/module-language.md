# RBE Module Language Specification

Status: **planned / design specification**

This document defines the intended `.module` language and its relationship to `.route`. It is deliberately explicit about what is designed versus what is currently runnable.

> **Important:** `.module` execution is not currently implemented end-to-end. Syntax/path resolution may exist in the route engine, but a module import must not be described as a working executable module until loading, interpretation, capability enforcement, recursion protection, and tests are complete.

## 1. Purpose

`.module` is RBE's reusable logic layer. It exists for work that should not live directly in an HTTP route: reusable functions, multiple parameters, privileged capabilities, persistent storage, advanced networking, and heavier computation.

`.route` remains the constrained HTTP entrypoint layer.

The intended relationship is:

```text
.route
  ↓ calls
.module
  ↓ may call
built-in capabilities / other modules
```

## 2. What makes `.module` different from `.route`

### `.route`

- HTTP entrypoint methods are restricted to route verbs (`get`, `post`, `put`, `delete`, `patch`, `head`, `options`).
- Request handling is the primary purpose.
- Privileged capabilities are deliberately restricted.
- The route capability surface is small and auditable.

### `.module`

- Function names are arbitrary identifiers.
- Functions may accept zero or more named parameters.
- Functions are reusable from routes and other modules.
- Module-only capabilities such as environment, Vault, storage, and cache belong here when implemented.
- Modules may import other modules.
- Recursive dependency loading requires cycle detection.

Example intended module:

```js
:import[json]

function findUser(id) {
    // module implementation
}

function formatUser(user, includePrivate) {
    // module implementation
}
```

A route could eventually call:

```js
:import["./module/users"]

class Route {
    async get(req) {
        return users.findUser(req.params.id);
    }
}
```

## 3. Parameters

Unlike a route HTTP handler, a module function is not restricted to one request parameter.

The intended syntax is ordinary function parameters:

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

Arguments are passed positionally by the caller:

```js
storage.get(id)
storage.set(key, value)
```

Arity and argument compatibility should be validated during semantic analysis once module execution exists.

## 4. Imports

Modules use the same import directive as routes:

```text
:import[net]
:import[json]
:import["./other"]
:import[module&storage]
```

### Built-in imports

Unquoted names refer to curated RBE capabilities. The module capability matrix is broader than the route matrix, but importing a name alone must not bypass the actual capability implementation or security policy.

### Module-path imports

Quoted paths identify another `.module` file. The `module&name` shorthand identifies a module under the default `/module/` directory.

The intended resolution rules are:

```text
module&storage
    ↓
/module/storage.module

"./module/storage"
    ↓
/module/storage.module
```

The backend binary's directory is the root for module resolution; resolution must not depend on the process current working directory.

## 5. Module binding names

A module import binds to the module's file stem unless an explicit alias is added by the import grammar.

For example:

```text
:import[module&storage]
```

binds the module as:

```text
storage
```

and a caller uses:

```text
storage.get(id)
```

The same resolved module can eventually be imported through a quoted path.

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

Once module execution is implemented, a caller will use:

```text
moduleName.functionName(arg1, arg2)
```

The compiler should reject:

- unknown module functions
- incorrect argument counts
- invalid argument forms
- module values used as ordinary values when only a callable export exists
- calls to capabilities unavailable under the current policy

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

The loader must maintain a dependency graph and detect cycles:

```text
A → B → C → A
```

A cycle must become a deterministic compiler/load error. The loader must not recurse indefinitely or deadlock the backend.

## 9. Execution model

The first module implementation should use the same interpreter infrastructure as `.route` where practical. The module function is parsed, semantically analyzed, and evaluated against a local parameter environment.

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

Modules must not become an uncontrolled escape hatch into the Rust host process.

## 10. Caching

Modules should participate in the same content-hash caching model used by routes once loading is implemented.

The cache key must change when the module's own source changes. A module dependency graph must also invalidate callers when an imported module changes, otherwise routes could execute stale dependency code.

## 11. Security

Module capabilities are privileged by design. In particular:

- `env` must not become ambient access for routes.
- `vault` must continue to respect Vault ACLs.
- `storage` and `cache` must be scoped by backend policy.
- filesystem/process access must not be exposed simply because a module is more powerful than a route.
- capability imports must remain explicit and auditable.

The existence of a `.module` file must never grant it arbitrary Rust or operating-system access.

## 12. Diagnostics

Module diagnostics use the same compiler diagnostic system as routes.

Examples:

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

The intended compiler behavior is:

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

The `/module/` directory is a sibling of the compiled backend binary, not an embedded JavaScript package directory.

## 15. Implementation status

The following are **planned**, not claims of current end-to-end support:

- full `.module` parsing as a separate executable file type
- arbitrary module function parameters
- module function execution
- module-to-module recursive loading
- dependency graph construction
- cycle detection
- module content-hash dependency invalidation
- module capability enforcement at runtime
- module-specific tests
- complete module diagnostics

Path resolution/desugaring infrastructure may already exist in the route engine. That does **not** mean module execution is implemented.

## 16. Relationship to the route specification

The route specification remains the authority for HTTP entrypoints and route-only restrictions. This document is the authority for the planned reusable module layer.

When module execution is actually implemented, both documents must be updated together, along with `api/README.md` and `route-error-book.md`, so the documented capability matrix and real runtime behavior remain synchronized.
