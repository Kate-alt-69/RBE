# RBE

RBE is a Rust backend engine designed to run backend applications through a controlled runtime rather than by exposing an unrestricted general-purpose JavaScript/TypeScript process.

RBE is built around four REL source types:

- `*.route` — HTTP entrypoints.
- `*.module` — reusable in-process backend logic.
- `*.service` — isolated service programs managed by the Service Runtime/Fabric.
- `server.server` — root server composition, policy, middleware, ENV defaults, forced settings, and embedded REL files.

The language used by these files is **REL — Runtime Engine Language**. The compiler is **RELC — Runtime Engine Language Compiler**.

## What RBE is trying to become

RBE's target architecture is a backend runtime where source is discovered and compiled at boot into a validated **Runtime Image**. The running process then executes that image instead of repeatedly depending on source files being present on disk.

```text
settings.json
server.server
api/**/*.route
module/**/*.module
service/**/*.service
        |
        v
       RELC
        |
        v
   Runtime Image
        |
        v
     RBE Engine
```

This gives RBE a place to enforce capability boundaries, middleware policy, service isolation, resource limits, dependency analysis, deterministic configuration, and runtime safety before requests are accepted.

## Why the source types stay separate

The source types are intentionally individual because their jobs are different.

- Route REL owns HTTP request/response entrypoints.
- Module REL owns reusable code that runs in the backend process.
- Service REL owns code that executes through managed service processes.
- Server REL owns whole-server composition and policy.

They share REL grammar, but they do **not** share every capability or lifecycle.

## Global grammar, scoped functionality

A higher-level grammar feature belongs to REL globally. If REL gains richer functions, conditions, classes, structured expressions, pattern matching, recursion syntax, or another language construct, all source types should be able to use that grammar where the construct makes semantic sense.

What differs is functionality. For example:

- `.route` can define HTTP handlers but cannot manage the server listener.
- `.module` can expose reusable functions but does not own service process lifecycle.
- `.service` can define `class Service` lifecycle behavior but does not own server CORS policy.
- `server.server` can force server policy and embed other REL files, but an embedded module is still governed by Module REL capabilities.

See [`compatibility.md`](compatibility.md).

## Runtime ownership

The root RBE process (the Mother/Grandmother runtime) owns the authoritative application image and shared public runtime configuration. Public ENV values from `settings.json` and fallback/default values from `server.server` are resolved into a runtime-owned environment snapshot that authorized REL sources can read.

"Public" here means shared within the REL backend runtime. It does not mean automatically exposed over HTTP.

## Is RBE worth using?

RBE is useful when the project benefits from a backend runtime with strong control over what application code can access and how the server is assembled. Its value comes from integration: compiler diagnostics, explicit capabilities, service isolation, native middleware, runtime images, resource control, and a backend-specific language/runtime designed together.

RBE is still evolving. Some parts are implemented today while others in these docs are the target contract being built. Every reference page marks planned features instead of pretending parser support equals finished runtime behavior.
