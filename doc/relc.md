# RELC — Runtime Engine Language Compiler

RELC is the compiler/linker that turns the application's REL sources plus deployment settings into one validated immutable **Runtime Image**. It preserves each source role instead of flattening `.route`, `.module`, `.service`, and `server.server` into a generic script type.

## Implemented boot entrypoint

The current compiler entrypoint is conceptually:

```text
compile_runtime_image(
    server.server source,
    discovered physical REL sources,
    effective settings
) -> RuntimeImage
```

Before linking, RBE also validates the physical Service catalog and source fingerprints used for managed Service activation.

## Current compilation pipeline

The pipeline now performs the major RELC stages that older documents described as future work:

```text
PASS 1   extract embedded REL + register all source identities
PASS 2   lex/parse each source using its role-specific entrypoint
PASS 3   validate import targets and capability ownership
PASS 4   validate synchronous Service dependency cycles
PASS 5   build symbol/dependency graph + recursive groups
PASS 6   resolve typed Runtime ENV
         resolve ServerPolicy + FORCE precedence
         lower deterministic MiddlewarePlan
PASS 7   compile supported Route REL to native WASM
         record explicit fallback reason for unsupported routes
PASS 8   collect immutable executable REL program snapshots
PASS 9   derive SHA-256 source/image identity metadata
PASS 10  link RuntimeImage
```

The numbered passes are documentation groupings rather than a promise that every internal function is named exactly `pass_N`.

## Source discovery and identity

Physical discovery currently covers:

```text
server.server
api/**/*.route
module/**/*.module
validated service catalog entries (*.service)
```

RELC extracts `[file-start:*]` blocks from the Server REL root before compiling the remaining Server source. Every physical or embedded source receives a stable `SourceId`, source role, logical name, source origin, and content fingerprint.

Conceptually:

```text
RelSource {
    id
    kind
    logicalName
    origin
    source
}

SourceOrigin::Physical { path }
SourceOrigin::Embedded { container, blockIndex, startLine }
```

Physical and embedded sources use the same logical namespace. A physical and embedded source cannot both claim the same logical target and silently race for import resolution.

Embedded Server REL is rejected: `server.server` is the single root composition shape rather than a recursive container for another server root.

## Parsing and role preservation

Route, Module, Service, and Server sources share REL lexer/parser infrastructure, but they use role-specific parse/semantic entrypoints. The role determines declarations and capabilities, not whether basic REL expressions/functions are a separate language.

RELC validates important authority rules before activation, including:

- Route REL cannot directly import a Service.
- `ENV` is not exposed to Route REL by default.
- lowercase process-environment `env` is rejected in RELC-linked apps.
- `quickDB` is Service-only.
- imports must resolve to a valid logical target/export.

## Dependency analysis

RELC records symbol-level dependency information in `SymbolDependencyGraph` and computes recursive strongly connected groups. This lets the image distinguish a real symbol recursion group from a source file that merely imports another source.

Service imports receive an additional current-runtime rule: a synchronous Service-to-Service dependency cycle is rejected at compile time because it would deadlock the current Service worker/Fabric request model.

The runtime's `InvocationTracker` separately enforces maximum depth, repeated-symbol depth, operation budget, and cyclic waits on unresolved produced values.

## Runtime ENV resolution

Runtime ENV is fully linked during RELC compilation. Precedence is:

```text
built-ins
    < normal Server REL env defaults
    < settings.json runtimeEnv
    < forced Server REL env values
```

Types are preserved. The resulting `RuntimeEnv` snapshot is stored in the Runtime Image and can be reconstructed from the image snapshot for supervised Service processes.

See [`rel.md`](rel.md) and [`server.server/`](server.server/).

## ServerPolicy resolution

RELC resolves typed server policy before the image is activated. The current precedence is:

```text
RBE built-ins
    < normal Server REL values
    < supported settings.json overlays
    < Server REL FORCE values
```

Hard engine ceilings are validated after layering and cannot be overridden by FORCE.

Current hard ceilings include:

- request body: 1 GiB;
- recursion max depth: 1024;
- repeated-symbol depth: 512;
- invocation operation budget: 10,000,000.

## MiddlewarePlan lowering

Server REL `middleware { ... }` is lowered into an ordered native `MiddlewarePlan`. RELC rejects unknown middleware names, duplicate stages, and an `errorHandler` that is not last.

The plan recognizes the current native names documented in [`server.server/`](server.server/). Boot currently materializes plan-driven settings for JSON limits, timeout, CORS disabling, API rate-limit values, IP-ban values, CSP, and compression enablement. Other recognized stages may still be provided by the always-installed RBE security stack or await dedicated plan-controlled wiring.

## Native Route WASM

RELC now has a real Route REL -> WebAssembly lowering path. `ROUTE_WASM_ABI_VERSION` and compiler version are part of image identity.

The current native subset is deliberately strict:

- exactly one HTTP method;
- no route imports;
- no helper functions;
- exactly one `return` statement;
- the returned value must be statically encodable JSON (string/number/bool/null/array/object literals);
- static output is bounded.

A supported route produces deterministic WASM bytes plus a SHA-256 artifact identity. An unsupported route produces a stored `InterpreterFallback { reason }` instead of pretending to be native.

The current public HTTP route path still has the immutable REL evaluator path while native dispatch integration expands; the important compiler guarantee today is that native artifacts/fallback decisions are linked to the same immutable image as the source program.

## Runtime Image — implemented

The current `RuntimeImage` contains, in practical terms:

```text
RuntimeImage {
    imageId
    sourceHash
    serverPolicy
    environment
    routes
    modules
    services
    sources
    symbolTable
    dependencyGraph
    recursiveGroups
    middlewarePlan
    serviceAssignments
    capabilities
    routeWasmArtifacts
    routeWasmFallbacks
    executables
}
```

`executables` contains immutable parsed Route/Module/Service/Server program objects. Normal runtime construction therefore consumes the linked image instead of reopening Route/Module source files.

### Cryptographic source identity

`sourceHash` is now a full SHA-256 identity rather than the former small non-cryptographic hash. RELC sorts the complete source set by `SourceId`, domain-separates the hash with `RBE_SOURCE_SET_V1`, and length-delimits each identity/source input before hashing.

Conceptually:

```text
sourceHash = SHA-256(
    RBE_SOURCE_SET_V1
    + source-count
    + sorted(SourceId + exact source bytes)
)
```

The result is lowercase 64-character hexadecimal text.

### Cryptographic Runtime Image identity

`imageId` is also a lowercase 64-character SHA-256 identity. The current `RBE_RUNTIME_IMAGE_V3` identity binds:

- `sourceHash`;
- `ROUTE_WASM_ABI_VERSION`;
- `ROUTE_WASM_COMPILER_VERSION`;
- the effective settings JSON.

Settings hashing is deterministic: object keys are sorted, arrays remain ordered, JSON types are preserved, and encoded values are length-delimited. Equivalent settings objects with different object-key ordering therefore hash identically, while meaningful source/settings/compiler-ABI changes produce a different image identity.

The Container capability broker validates and binds grants to this exact Runtime Image SHA-256 identity. These hashes are content identities, **not digital signatures**; publisher authenticity belongs to the future signed RBI format.

See [`runtime-image.md`](runtime-image.md) for the full identity/activation contract.

## Activation and reload

`RuntimeImageSlot` stores an immutable active snapshot behind an atomic/RwLock-controlled replacement point:

```text
Image A active
   |
compile + validate Image B
   |
valid? -- no --> keep A
   |
  yes
   |
activate B atomically
```

The activation primitive exists. A complete automatic file watcher/reload coordinator is still separate work; do not read “transactional image slot” as “all hot reload UX is finished.”

## Diagnostics

RELC errors identify source identity and the failing stage (source registration, embedded extraction, parse, capability/import validation, policy/env/middleware lowering, dependency validation, or image linking). Existing role-specific diagnostic families such as `MOD1xxx`/`SVC1xxx` still exist in compatibility compiler paths; a single final numeric RELC error-book taxonomy is not yet the only diagnostic surface.

## Source integrity

A successfully linked image is the runtime authority. Native artifacts and executable program snapshots are pinned to it, while Service activation additionally verifies the parent-validated Service source digest. Raw source files remain deployment/restart inputs until a persistent signed source-less Runtime Image artifact is implemented.

See [`runtime-image.md`](runtime-image.md) and [`source-security.md`](source-security.md).
