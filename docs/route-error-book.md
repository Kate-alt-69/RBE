# RBE Route Compiler Error Book

This is the human-readable troubleshooting guide for `.route` compiler diagnostics.

The compiler should never only say that something is wrong. It should explain **what was expected, what was found, why the mistake matters, and how to fix it**.

The detailed error output is also written to `data/admin/compiler-error.txt` during boot.

---

# Diagnostic format

A route diagnostic is grouped by file:

```text
1 error, 0 warnings in file api/health.route

############################
|1| :import[net]
|2|
|3| const health = net.health(); <<<<<<<<<
############################
                         ^^^
error[E3011]: net.health() does not exist — please remove it from api/health.route
line 3, column 21
```

A compiler diagnostic should answer four questions:

1. **Where?** — exact source file, line, and column.
2. **What?** — the immediate problem.
3. **Why?** — the language rule that was violated.
4. **How do I fix it?** — an actionable removal or correction.

---

# Syntax errors

Syntax errors mean the source cannot be understood as a valid RBE route-language program yet.

## E1001 — unexpected token

```text
error[E1001]: unexpected token `}` while parsing an expression
```

The parser reached a token that cannot legally appear where it currently is. Check the highlighted token and the one or two tokens immediately before it.

## E1002 — missing `]`

```text
:import[net
```

```text
error[E1002]: expected `]` to close the import
```

Correct:

```text
:import[net]
```

## E1003 — expected `const`/`let`

```text
cnst LMAO = net
```

```text
error[E1003]: expected `const`
```

## E1004 — missing `)`

```text
function test(req {
```

Close the function parameter list before the body.

## E1005 — missing `}`

An opened class, method, block, or object must be closed before the enclosing construct ends.

## E1006 — missing comma between imports

```text
:import[response as resp net]
```

Correct:

```text
:import[response as resp, net]
```

## E1007 — trailing comma in imports

```text
:import[net, json,]
```

Correct:

```text
:import[net, json]
```

## E1008 — unknown route method

Only the route HTTP methods currently supported by the grammar should be used:

```text
get
post
put
delete
patch
head
options
```

---

# Semantic errors

## E2001 — undefined variable

```text
error[E2001]: `usr` is not defined
```

Declare it, use the correct name, or import the capability that provides it.

## E2002 — duplicate declaration/import binding

Two local or top-level bindings cannot occupy the same name in the same scope.

## E2003 — symbol shadows an imported/top-level symbol

A function or route parameter cannot silently shadow an imported or top-level name.

## E2004 — duplicate declaration in the same scope

```text
const value = 1;
const value = 2;
```

Rename one binding or remove the duplicate.

## E2005 — module used as a value

```text
:import[net]
return net;
```

`net` is a capability namespace, not a value. Use an exported function instead:

```text
return net.ping();
```

## E2006 — callable metadata used as a value

A function or direct imported function must be called rather than used as an ordinary value.

## E2007 — module is not imported

A member call or member access names a capability that is not present in the file's imports.

## E2008 — value is not callable

The compiler resolved the name, but it is a local value or module that cannot be called directly.

## E2009 — function is not defined

The called function name does not resolve to a local function or direct import.

---

# Capability/import errors

## E3000 — capability unavailable in `.route`

Example:

```text
:import[env]
```

Diagnostic:

```text
error[E3000]: capability `env` is not available to `.route` files
```

`env`, `vault`, `storage`, `cache`, and other privileged surfaces remain module-only.

## E3001 — duplicate import source

Example:

```text
:import[net as first, net as second]
```

The source capability `net` is requested twice. Aliasing does not make it a different capability.

```text
error[E3001]: duplicate import source `builtin:net`; a capability may only be imported once per file
```

---

# Unknown built-ins and imports

## E3010 — built-in does not exist as an import

This error is specifically for an attempted built-in function import whose exported function does not exist.

Example:

```text
:import[vault.import]
```

Diagnostic:

```text
error[E3010]: vault.import does not exist as a import — please remove `vault.import` from api/health.route
```

The important distinction is that this is an **import-time compiler error**. It is not a request-time failure.

The same rule applies to every known built-in namespace:

```text
:import[net.health]
```

```text
error[E3010]: net.health does not exist as a import — please remove `net.health` from api/health.route
```

## E3011 — built-in member call does not exist

Example:

```text
:import[net]

class Route {
    get(req) {
        return net.health();
    }
}
```

Diagnostic:

```text
error[E3011]: net.health() does not exist — please remove it from api/health.route
```

This is caught during semantic analysis so the backend does not boot with a route that is guaranteed to return HTTP 500 when invoked.

The same rule applies to `private`:

```text
private.foo()
```

becomes:

```text
error[E3011]: private.foo() does not exist — please remove it from api/health.route
```

---

# Internal runtime health capability

`.route` has a deliberately narrow `private` capability for server-owned health information:

```text
:import[private]
private.health()
```

Currently `private.health()` exposes:

```text
status          backend health state
uptime          backend runtime uptime in seconds
container       null until container health is exposed
vault           true while the backend's required Vault dependency is ready
errorReporter   null until reporter state is exposed
```

`private` is **not** a general process, filesystem, environment, or secret-access API.

---

# Runtime route failures

## E4000 — route evaluation failed at runtime

Runtime failures are still sent to the normal tracing/logging system, but they are also appended to:

```text
./data/admin/compiler-error.txt
```

Example:

```text
E4000: route evaluation failed at /api/example: ...
```

This is a runtime record, not a compiler diagnostic. It is kept in the same admin error file so an operator does not have to hunt through request logs to discover that a route failed.

---

# Warning messages

Warnings should teach without preventing a valid backend from booting.

## W0001 — unused variable

```text
warning[W0001]: local `debug` is never used
```

## W0002 — unused import/function symbol

```text
warning[W0002]: import `json` is never used
```

Remove unused imports or use the capability. Keeping imports honest makes the route's capability surface easier to audit.

---

# Why the compiler collects errors

A broken route should not hide unrelated broken routes.

The boot compiler is intended to:

```text
scan every `.route`
    ↓
parse every recoverable file
    ↓
collect syntax errors
    ↓
analyze valid ASTs
    ↓
collect semantic/capability errors
    ↓
show every file's diagnostics
    ↓
write errors to compiler-error.txt
    ↓
abort boot only after diagnostics are collected
```

Invalid routes are not turned into Rust artifacts.

---

# Error recovery rules

The parser should synchronize at safe grammar boundaries such as `;`, `}`, `class`, `function`, and `:import`.

Recovery must not invent fake valid AST nodes merely to make a diagnostic disappear. A unit that cannot be reconstructed safely is excluded from semantic analysis and Rust artifact generation.

---

# Fixing errors: practical workflow

1. Fix the first syntax error.
2. Re-run the compiler and inspect the full diagnostic list.
3. Fix semantic and capability errors next.
4. Treat warnings as cleanup unless they identify real logic mistakes.
5. Investigate generated Rust or runtime behavior only after the route compiles cleanly.
