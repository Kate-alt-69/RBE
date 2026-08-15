# RBE Route Language Specification

Status: **living specification**

This document defines the planned and implemented language surface for RBE `.route` files. It is the source of truth for syntax, imports, capability boundaries, diagnostics, and the relationship between `.route` and `.module` files.

> **Important:** Features marked **Planned** describe the language design, not necessarily what the current compiler accepts. The implementation status in `api/README.md` remains authoritative for what is currently runnable.

---

## 1. Design goal

`.route` is intentionally a small, JavaScript-shaped API language that compiles/interprets into the Rust backend. It is **not JavaScript**, does not embed Node/Bun, and does not expose arbitrary host-language functionality.

The guiding rule is:

> A route should have enough power to express normal API work while dangerous, expensive, privileged, or complex operations belong in `.module` code.

The language is capability-oriented. Importing a capability is an explicit statement that the route is allowed to use it.

---

## 2. File types

### `.route`

A route file defines one HTTP endpoint group. It contains a `Route` class whose method names are HTTP verbs such as `get`, `post`, `put`, `delete`, `patch`, `head`, and `options`.

`.route` is deliberately restricted: expensive computation, privileged services, storage, Vault, environment access, and advanced networking belong outside the route language.

### `.module`

A module is the more powerful reusable logic layer. It is designed to support more general functions, more parameters, more control flow, more built-ins, and heavier work.

`.module` support is **planned**, not currently complete.

---

## 3. Variables

The preferred explicit variable spelling is:

```text
$[name]
```

The parser may also accept ordinary identifiers where the grammar calls for a name. `$[...]` is the deliberately recognizable route-language form and should remain reserved for variable references rather than making every punctuation character part of identifiers.

---

## 4. Imports

Imports are written as one directive and may contain multiple entries.

### Single built-in

```text
:import[net]
```

### Alias

```text
:import[response as resp]
```

### Multiple built-ins

```text
:import[response as resp, net]
```

The comma separator is deliberately written as `, ` — a comma followed by whitespace — to make multiple imports obvious to both humans and the parser.

### Built-in function import

```text
:import[net.ping as ping]
```

### Module path

External/module imports are represented by quoted strings:

```text
:import["module/network/uac-login"]
```

### Mixed import line

```text
:import[response as resp, net, "module/network/uac-login"]
```

The parser resolves each entry independently:

```text
response                 -> local name `resp`
net                      -> local name `net`
"module/network/uac-login" -> module capability
```

### Import uniqueness

A built-in or module should only be imported once in a file. Duplicate imports should produce a compiler diagnostic rather than silently replacing an earlier binding.

Aliases are local names; the compiler should detect duplicate **source capabilities**, not merely duplicate local aliases.

### Import resolution

Unquoted names refer to curated RBE built-ins. Quoted strings refer to module paths. `.module` resolution is a planned capability and must not be confused with JavaScript package resolution.

---

## 5. Capability matrix

The first planned capability boundary is:

| Capability | `.route` | `.module` | Boundary |
|---|---:|---:|---|
| `net` | Yes | Yes | Basic networking in routes; advanced networking in modules |
| `json` | Yes | Yes | Normal serialization/parsing |
| `env` | No | Yes | Environment access is privileged to modules |
| `crypto` | Yes | Yes | Basic primitives in routes; heavy/advanced crypto in modules |
| `encoding` | No | Yes | Module-only utility surface |
| `time` | Yes | Yes | Same basic time functionality |
| `http` | Yes | No | Route-facing request/response helpers |
| `auth` | No | Yes | Requires the authentication feature to be enabled and built |
| `vault` | No | Yes | Privileged secret access |
| `request` | Yes | Yes | Simple HTTP operations in routes; advanced request construction in modules |
| `storage` | No | Yes | Module-only persistent storage |
| `cache` | No | Yes | Module-only cache control |
| `log` | Yes | Yes | Same usage in both file types |
| `security` | Yes | Yes | Capability is configurable and policy-controlled |
| `response` | Via `net` | Via `net` | Planned namespace such as `net:response` |
| `private` | Yes | N/A | Read-only backend runtime health information |

This table is a **design contract**, not permission to expose all listed functions immediately.

---

## 6. Planned built-ins

### `net`

`.route` should provide safe, common operations such as:

```text
net.ping()
net.get(url)
net.post(url, body)
net.put(url, body)
net.patch(url, body)
net.delete(url)
```

Advanced networking belongs in `.module`, including more complex request construction, special transports, lower-level networking, and operations that carry substantially more risk or compute cost.

The exact API should be kept narrow and capability-checked rather than exposing arbitrary sockets or host networking.

### `json`

Available to both file types for ordinary structured data work.

### `env`

Module-only. Routes must not be able to enumerate process secrets or ambient environment state.

### `crypto`

Both layers receive normal primitives. Heavy or advanced computation belongs in `.module`.

### `encoding`

Module-only.

### `time`

Both layers. Normal timestamp and duration operations should not require a module.

### `http`

Route-only helpers for HTTP-facing behavior.

### `auth`

Module-only. Importing it is not sufficient: the corresponding authentication feature must be enabled and built before the capability can be used.

### `vault`

Module-only and privileged. A Vault import must not grant unrestricted access to secrets; normal Vault ACLs still apply.

### `request`

Both layers. `.route` gets common API request operations; advanced request construction and specialized methods belong in `.module`.

### `storage`

Module-only.

### `cache`

Module-only.

### `log`

Available in both layers with the same usage model.

`log.warn` and `log.info` are intended to be available by default and therefore do not need to be imported as explicit built-in capabilities.

### `security`

Available in both layers. The exact capability set should be configurable by security policy and settings rather than hard-coded forever into the language.

### `net:response`

The response capability is conceptually part of the network/HTTP boundary and may be imported with:

```text
:import[net:response as resp]
```

This provides an explicit namespace for response construction without making `response` an ambient global.

### `private`

`private` is a backend-owned, read-only health capability. It is intentionally **not** a general host/runtime escape hatch.

The currently exposed operation is:

```text
private.health()
```

It returns:

```text
status          -> backend health state
uptime          -> backend runtime uptime in seconds
container       -> null until container health is exposed here
vault           -> true when the backend's required Vault process is ready
errorReporter   -> null until reporter state is exposed here
```

Routes cannot use `private` to read environment variables, secrets, arbitrary processes, or filesystem state.

---

## 7. Route structure

A normal route looks like:

```js
:import[net]

class Route {
    async get(req) {
        const engine = net.ping();

        return {
            ok: true,
            engine: engine,
            path: req.path
        };
    }
}
```

A health route can use the internal runtime capability:

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

Only declared HTTP methods are registered.

The class is an HTTP entrypoint surface; helper functions should eventually remain outside the class so reusable logic is clearly separated from transport entrypoints.

---

## 8. Functions

Top-level functions are intended to be allowed in `.route` files for small reusable logic:

```js
function validate(req) {
    return req.method == "GET";
}

class Route {
    get(req) {
        return validate(req);
    }
}
```

The compiler should perform reachability analysis so unused functions can be excluded from generated Rust artifacts.

Example warning:

```text
warning[W0002]: function `legacyHelper` is never called
          this function will not be included in the generated Rust artifact
```

---

## 9. Compiler pipeline

The intended pipeline is:

```text
.route source
    ↓
lexing
    ↓
parsing + error recovery
    ↓
AST
    ↓
name resolution
    ↓
import/capability validation
    ↓
unused/dead symbol analysis
    ↓
diagnostics
    ↓
Rust artifact generation
    ↓
route registration
```

A syntax or semantic error in one file should not prevent the compiler from inspecting unrelated files. The compiler should collect as many diagnostics as possible before deciding whether boot can continue.

Invalid routes must not be turned into Rust artifacts.

---

## 10. Diagnostics

Diagnostics are divided into:

- syntax errors
- semantic/name-resolution errors
- import/capability errors
- artifact-generation errors
- runtime route evaluation errors
- warnings

Warnings do not abort backend boot. Errors do.

The compiler writes detailed **errors** to:

```text
./data/admin/compiler-error.txt
```

That file is cleared at backend boot so stale diagnostics cannot survive into a new run.

### Diagnostic codes

```text
E3000  capability unavailable in `.route`
E3001  duplicate import source
E3010  built-in import/function name does not exist as an import
E3011  built-in member call does not exist
E4000  route evaluation failed at runtime
```

Unknown built-in imports are compiler errors, not request-time errors:

```text
error[E3010]: vault.import does not exist as a import — please remove `vault.import` from api/health.route
```

Unknown built-in calls are also compiler errors:

```text
error[E3011]: net.health() does not exist — please remove it from api/health.route
```

---

## 11. Source diagnostics

Compiler messages should be displayed per file:

```text
2 errors, 1 warning in file /api/uac/login.route

############################
|1| :import[net]
|2|
|3| cnst LMAO = net <<<<<<<<<
|4| funciton(LMAO)
############################
    ^^^^
expected `const`
line 3, column 2
```

The diagnostic frame is intentionally multi-line rather than a one-line blob. The source lines are rendered individually so terminal coloring can distinguish:

- frame borders
- source lines
- the failing source line
- the pointer
- the explanation

The `<` continuation marker on the error line should extend toward the right border so the frame remains visually aligned at different terminal widths.

The terminal renderer must size the frame dynamically for the active terminal.

---

## 12. Boot-time terminal ownership

During route compilation the compiler temporarily owns the visible terminal UI.

Conceptually:

```text
normal RBE boot logs
        ↓
acquire compiler terminal session
        ↓
scan /api
        ↓
parse / semantic analysis / artifact generation
        ↓
release compiler terminal session
        ↓
normal RBE logs continue
        ↓
backend ready
```

This is a presentation layer, not a second logging system. Structured logs remain collected normally.

The UI must work on ANSI-capable Linux terminals, Windows CMD, Windows PowerShell, PowerShell 7, and non-interactive output. A plain-output fallback is required when terminal control is unavailable.

---

## 13. Work accounting

Before progress rendering begins, the compiler discovers the route files and calculates the work units.

The current design uses three logical units per `.route` file:

```text
1. Parsing
2. Semantic analysis
3. Rust artifact generation
```

The progress counter is shown separately from the bar:

```text
Parsing 18/59
[████████████████░░░░░░░░]
```

The bar starts empty and fills left-to-right. Its width is calculated from the active terminal width.

No artificial sleep is required to make progress visible; the renderer represents real compiler work.

---

## 14. Status labels in this document

Every feature documented here should be treated as one of:

- **Implemented** — currently available in the RBE codebase and tested enough to describe as working.
- **Planned** — agreed language design that still needs implementation.
- **Reserved** — syntax/name is intentionally held for future use and should not be implemented opportunistically.

The specification should be updated whenever the language changes so source syntax and compiler behavior do not drift apart.
