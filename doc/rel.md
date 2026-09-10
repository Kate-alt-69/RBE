# REL — Runtime Engine Language

REL is the language used by RBE source files.

REL is **one language with multiple source roles**. Grammar belongs to REL globally; capabilities, declarations, lifecycles, and runtime ownership differ by source type.

## REL source types

| Source | Role |
|---|---|
| `*.route` | HTTP entrypoints and request/response logic |
| `*.module` | reusable in-process backend logic |
| `*.service` | isolated managed service programs |
| `server.server` | whole-server composition and policy |

The source types are intentionally individual. REL does not turn them into one generic file type.

## Global grammar rule

When REL gains a language feature, that feature should normally be available to all REL source types:

- variables and constants
- literals, arrays, and objects
- expressions and operators
- `if` / `else` and future control-flow constructs
- functions and helper functions
- async functions/calls where supported by the runtime operation
- classes and methods
- member access and calls
- imports
- recursion and mutually recursive function graphs
- future higher-level grammar

A source role may restrict *what a construct means* or *which capabilities it may call*, but should not receive an artificially weaker grammar merely because it is a `.route`, `.module`, or `.service` file.

## Functionality is scoped

Examples of semantic differences:

```text
.route   -> may declare route HTTP handlers
.module  -> may export reusable in-process functions
.service -> may declare service lifecycle and service exports
.server  -> may declare server policy, force settings, middleware and embedded files
```

A global grammar does not allow `.route` to use server-only configuration or `.module` to pretend it owns an isolated service process.

## Imports

REL imports remain capability-oriented and source-aware. Current forms include built-ins, direct built-in functions, module paths/shorthand, aliases, and service references where allowed.

Examples:

```text
:import[json, time]
:import[net.ping as ping]
:import[module&users]
:import["./module/users"]
:import[service:mail as mail]
```

The compiler validates whether the target exists and whether the current source type is allowed to import it.

## ENV

The target REL environment capability is `ENV` for shared runtime configuration available to authorized `.module` and `.service` code and Server REL itself.

Values from `settings.json` ENV are public to the backend runtime, while `server.server` may provide defaults when a value is not supplied by settings.

Target surface:

```text
ENV.get("APP_NAME")
ENV.has("APP_NAME")
ENV.require("APP_NAME")
ENV.string("APP_NAME")
ENV.number("SESSION_TTL")
ENV.bool("DEBUG")
ENV.object("MAIL")
```

ENV is typed; JSON numbers, booleans, arrays, and objects should not be flattened into strings merely because operating-system environment variables traditionally are.

See [`server.server/`](server.server/) for precedence and configuration ownership.

## Runtime source and environment security

A linked application executes the immutable AST/program snapshots stored in its
Runtime Image. Raw `.route`, `.module`, `.service`, and `server.server` files are
boot/deployment inputs; changing them after RELC validation does not change the
active Route/Module program. Service child activation independently verifies the
ServiceCatalog SHA-256 contract until Service execution is fully transported as
an image snapshot too.

Uppercase `ENV` is public typed Runtime Image configuration. It is not the
operating-system process environment and it must not carry credentials. Secrets
belong in Vault. The legacy lowercase `env` process-environment capability is
rejected by RELC-linked applications, and Service Mother/Service child processes
start from a scrubbed environment instead of inheriting arbitrary loader, proxy,
or application variables.

RBE does not destructively delete source files as its primary security boundary.
Deletion does not defend against a host administrator/process-memory compromise,
can break restart/relink workflows, and does not make HTTP endpoints secret. The
runtime contract is instead:

```text
source bytes -> RELC validate/link -> immutable Runtime Image -> execute snapshot
```

Supervised Service REL does not reconstruct Runtime ENV from child process state.
Backend sends the active image's typed ENV snapshot over the authenticated
parent-liveness bootstrap channel; Service Mother propagates that same snapshot
to resident, on-demand, and restarted service children.

A future sealed production bundle may omit raw REL sources entirely after RBE
has a persistent signed executable Runtime Image format. That is a deployment
hardening/obfuscation feature, not a replacement for authentication,
authorization, Vault isolation, rate limiting, or network policy.

## Embedded REL files

Server REL may contain literal embedded files:

```text
[file-start:module.Auth]

:import[ENV]

export function appName() {
    return ENV.get("APP_NAME");
}

[file-end:module]
```

After source extraction this is a normal Module REL source with a virtual `SourceId`. It may import other physical or embedded modules and is compiled by the Module REL compiler path. The same rule applies to embedded routes and services.

The embedded container does not grant extra privileges to its contents.

## Recursion

REL intentionally supports recursive and mutually recursive programs. Import cycles or symbol cycles alone are not errors.

RELC distinguishes source dependencies from symbol dependencies, and the runtime distinguishes legitimate recursion from a cyclic computation that waits for data still being produced by the same unresolved dependency chain.

See [`relc.md`](relc.md).

## Current implementation note

The active engine already shares lexer/parser/evaluator infrastructure across Route, Module, and Service REL concepts, but the architecture documented here is the target contract for making REL/RELC explicit and consistent. Features marked as targets must not be described as runtime-complete until the corresponding engine work lands.
