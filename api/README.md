# `/api` — `.route` and `.module` files

This document covers **only** the `.route`/`.module` file system: the
syntax, how imports resolve, what's actually implemented today, and
what's designed-but-not-built-yet. For everything else about the
backend (architecture, phases, other crates), see the root
`README.md` and `rust-migration-plan.md`.

Implementation lives in the `route-engine` crate — this doc is the
user-facing reference; that crate's source has the engineering-facing
detail (parser, interpreter, caching).

---

## Status at a glance

| Piece | Status |
|---|---|
| `.route` files (imports, HTTP-verb methods, statements/expressions below) | **Implemented** |
| Built-in module imports (`net`, `env`) | **Implemented**, deliberately tiny (see below) |
| Content-hash caching (unchanged files aren't re-parsed) | **Implemented** |
| `.module` files (reusable code, imported by path or `module&name` shorthand) | **Not implemented.** Both `:import["./path"]` and `:import[module&name]` parse fine — the import itself never fails — but calling anything on the resulting module errors clearly at request time, and the error message shows exactly what path it would have resolved to. See [Planned: `.module` files](#planned-module-files) for the intended design. |
| Dynamic path segments (`[id].route`) | Not implemented — `req.params` always exists but is always empty |
| Real Rust codegen (parse → generate `.rs` → compile) instead of interpretation | Not implemented — v1 is a tree-walking interpreter, see [Execution model](#execution-model) |

---

## File → URL mapping

`.route` files under `/api/` are discovered at boot. File path maps to
URL path, same convention as Next.js and similar file-based routers:

```
api/example/ping.route        ->  /api/example/ping
api/account/index.route       ->  /api/account
```

`index.route` in a directory maps to the directory itself. There's no
manual registration step — dropping a file there is the registration.

## A `.route` file, in full

```js
:import[net]
:import["./module/storage"]

class Route {
    async get(req) {
        const pong = net.ping();
        return { ok: true, pong: pong, path: req.path };
    }

    async post(req) {
        return { ok: true, created: true };
    }
}
```

- **Imports** go at the top, before the class. Order doesn't matter
  between them, but they must all come before `class`.
- **The class name doesn't matter** to routing — `class Route { ... }`
  is convention, not a requirement enforced beyond "there must be
  exactly one class."
- **Method names must be HTTP verbs** — `get`, `post`, `put`, `delete`,
  `patch`, `head`, `options` (case-insensitive). Anything else fails
  to parse; there's no way to accidentally define a route method that
  silently doesn't get registered.
- **Only methods you write get registered.** A file with just `get`
  never registers `post` for that path — a request with any other
  method gets Axum's normal 404/405 handling. This is also *why*
  "block GET by default" from the Node backend's Permission Manager
  needs no separate implementation here: an undeclared verb is simply
  never wired up.
- **`async` is accepted and ignored.** The whole request pipeline is
  already async; the keyword doesn't change evaluation. It's in the
  grammar purely so real-looking class syntax parses without special-
  casing.

## Imports

Three forms:

```js
:import[net]                    // bare identifier -> built-in Rust capability
:import[module&storage]         // shorthand -> the default /module/ folder
:import["./module/storage"]     // explicit string path -> anywhere else
```

- **Bare identifier, no `&`, no quotes:** resolves against a fixed,
  curated set of built-in capabilities — real Rust functions, not
  "whatever Node's module of the same name exposes." Only `net` and
  `env` exist right now (see below). This is deliberate: the
  capability surface a `.route` file can reach is exactly what's been
  explicitly implemented, nothing more — the same reasoning that makes
  the container runtime's sandboxing meaningful applies here too.
- **`module&name` shorthand:** the common case — a `.module` file
  living in the default `/module/` folder, a sibling of the compiled
  binary. `:import[module&storage]` is exactly equivalent to
  `:import["./module/storage"]`; it's just shorter to write for the
  normal case where you're not reaching into a non-default location.
  Desugars to the same thing internally, so everything below about
  string-path imports applies to it too.
- **Explicit string path:** resolves a `.module` file anywhere else.
  `./` always means the directory the *compiled backend binary* runs
  from — **never** the current working directory, and never the
  `.route` file's own location. This is deliberate: the binary is
  meant to be launched from anywhere (a shortcut, a service manager, a
  different shell's CWD) and still find its sibling folders
  correctly, the same way `@` reliably means "project root" in a
  TypeScript path-alias setup regardless of which file uses it.
- **Binding name:** what the import is called inside the file's body.
  A builtin binds to its own name (`net` → `net`). A `module&name` or
  string-path import binds to its file's stem — `module&storage` and
  `"./module/storage"` both bind to `storage`. `.module` extension
  included or not in a string path, doesn't matter which you write —
  it's added automatically if missing.

## Directory layout

```
./backend(.exe)     <- the compiled binary
./settings.json       <- engine config (separate — see the root README)
./api/                 <- .route files, discovered here
│   └── example/ping.route
./module/               <- .module files, the default location `module&name` resolves against
    └── storage.module    (planned — not implemented yet)
```

`/api/` and `/module/` are **siblings of the binary, never compiled
into it.** Drop or edit files there without rebuilding anything — the
content-hash cache (see [Execution model](#execution-model)) means an
unchanged file isn't even re-parsed, only ones you actually touch.

## Built-in modules

### `net`

| Function | Signature | Notes |
|---|---|---|
| `ping()` | `() -> { ok: bool }` | Placeholder proving the call-into-Rust wiring works end to end. Real networking (outbound HTTP, DNS) is a capability-surface decision to make deliberately, not something to wire in as a side effect of the parser existing. |

### `env`

| Function | Signature | Notes |
|---|---|---|
| `get(key)` | `(string) -> string \| null` | Reads a process environment variable. Returns `null` if unset. |

Calling anything not listed here on `net`/`env` is a clear parse-time-
adjacent error ("`net.foo` is not implemented"), not a silent no-op.

## Statement and expression grammar

This is the whole v1 grammar — deliberately small:

**Statements:**
```js
const name = expr;      // binds a local name
return expr;             // ends the method, produces the response body
expr;                    // bare expression statement (e.g. a call for its side effect)
```

**Expressions:**
```js
"a string"                              // string literal
123, 1.5                                // number literal
true, false, null
{ key: expr, key2: expr }               // object literal
[expr, expr]                            // array literal
identifier                              // a const, req, or an imported module name
identifier.field                        // member access
module.function(arg1, arg2)             // call into an imported module
```

**Not in v1, on purpose:** `if`/`else`, loops, `try`/`catch`,
arithmetic/comparison operators, template-literal interpolation,
arrow functions, anything beyond calling *into* a module (no
user-defined functions inside a `.route` file itself). Real control
flow is where `.module` files are expected to matter most — see
below — which is exactly why they're the harder, deferred half of
this system rather than an afterthought.

## The request object

Bound to whatever parameter name a method declares (conventionally
`req`):

```js
async get(req) {
    // req.method -> "GET"
    // req.path   -> "/api/example/ping"
    // req.params -> {} (always empty in v1 — no dynamic segments yet)
    // req.query  -> {} (always empty in v1 — query-string parsing not wired yet)
}
```

## What a method returns

Whatever its `return` expression evaluates to gets JSON-serialized as
the response body (`200 OK`, `application/json`). A method that falls
off the end without hitting `return` produces `null` — same as a JS
function implicitly returning `undefined`, just represented as `null`
since this value model has no `undefined`.

An evaluation error (calling an unimported module, accessing a field
that doesn't behave the way v1 expects, etc.) produces a `500` with a
JSON `{ "error": "..." }` body describing what went wrong — errors are
never silent.

## Execution model

**v1 is a tree-walking interpreter, not a compiler.** Parsed `.route`
files are cached by content hash (a file's bytes are hashed; if
unchanged, the cached parse is reused — the real, buildable version of
"smart caching," at file granularity rather than sub-file diffing,
which isn't something hand-rollable reliably). At request time, the
cached AST is evaluated directly.

The eventual goal discussed for this system is real compilation:
parse → generate genuine Rust source implementing the equivalent
`axum` handler → let `rustc`/Cargo build and cache that, keyed by the
same content-hash dependency graph. That's a substantial next step,
intentionally not what v1 does — proving the grammar out against real
routes first, then upgrading the execution strategy once it's clear
the grammar is right, is the lower-risk order to do this in.

---

## Planned: `.module` files

Not implemented. This section describes the intended design so the
shape is documented before it's built, not a description of what
exists today.

A `.module` file is expected to look like a `.route` file with two
differences:

1. **No verb restriction on method names.** A `.route` file's methods
   must be named `get`/`post`/etc.; a `.module` file's methods are
   named whatever the function should be called
   (`storage.get(id)` implies a method literally named `get`, but
   `storage.findByEmail(email)` should be just as valid — any
   identifier, not a fixed verb set).
2. **Multiple parameters.** A `.route` method takes one implicit
   parameter (`req`); a `.module` function should take however many
   named parameters its callers pass, positionally — `get(id)`,
   `set(key, value)`, etc.

Everything else — the import syntax, the statement/expression
grammar, `./` meaning root — is intended to be identical, so a
`.module` file can itself `:import[net]` or `:import["./other"]`
another module.

**Resolution**, as designed — and this part IS already built
(`route_engine::paths`), even though loading the file's contents isn't
yet: when a `.route` (or `.module`) file imports a custom path or a
`module&name` shorthand, the engine resolves it against the compiled
binary's own directory (never the CWD), defaulting to the `/module/`
folder for the shorthand form or the given relative path otherwise,
adding a `.module` extension if the path doesn't already have one.
Once loading itself is implemented: parse it (same content-hash
caching as `.route` files), and make its functions callable as
`boundName.functionName(args)`. Calling into a module recursively
evaluates that module's function body against the given arguments, in
the same interpreter — with a cycle guard for `a` importing `b`
importing `a`, since that's a real risk once modules can import each
other and recursion isn't otherwise bounded.

**Why this is deferred rather than half-built:** doing it properly
touches the parser (generalizing method parameters from "zero-or-one,
named `req`" to "zero-or-more, arbitrary names"), the module registry
(recursive resolution instead of a flat builtin table), and needs the
cycle guard to be correct before it ships — not a small addition on
top of what exists. Better to land it as one deliberate piece than
partially.
