# RELC — Runtime Engine Language Compiler

RELC is the compiler/orchestrator for REL sources. It does not flatten `.route`, `.module`, `.service`, and `.server` into one generic source type. Instead it coordinates the purpose-built compile/validation paths for each source role while sharing REL grammar infrastructure.

## Compiler responsibilities

RELC owns:

1. source discovery
2. embedded-file extraction
3. lexing/parsing
4. declaration collection
5. import and symbol resolution
6. capability validation
7. dependency analysis
8. recursion analysis
9. diagnostics
10. runtime-image linking
11. future IR optimization/AOT stages

## Source discovery

RELC discovers both physical and embedded sources before it attempts to build the final application image.

```text
api/**/*.route
module/**/*.module
service/**/*.service
server.server
        +
embedded [file-start:*] blocks
```

Each source receives a stable diagnostic identity such as:

```text
route:/account/login
module:Auth
service:Mail
server:Main
server:Main#module:Auth
```

An embedded source keeps its own REL role. `server:Main#module:Auth` is Module REL after extraction; the `server:Main#` prefix records where it came from for diagnostics and reload metadata.

Physical and embedded sources are equivalent compilation inputs after discovery. They are also in the same logical namespace: a physical `module:Auth` and embedded `server:Main#module:Auth` may not both claim the logical Module REL target `Auth`.

### Source Registry — implemented foundation

The first concrete RELC compiler primitive is now implemented in the route-engine crate as `RelSourceRegistry`.

It currently provides:

- `RelSourceKind` for Route, Module, Service and Server REL roles without using the role as a grammar gate.
- stable `SourceId` values for physical and embedded sources.
- `SourceOrigin::Physical` and `SourceOrigin::Embedded` metadata.
- embedded block index and source start-line metadata for later diagnostic remapping into `server.server`.
- deterministic source iteration through an ordered registry.
- duplicate `SourceId` rejection.
- duplicate physical-path rejection.
- duplicate logical-target rejection across physical and embedded sources.
- a logical lookup index for later import/symbol resolution.
- enforcement that embedded sources belong to a registered Server REL container.
- rejection of embedded Server REL; `server.server` is the single root composition source shape, not a nesting mechanism for more servers.

The current registry is source identity and compiler-state infrastructure. It does **not** yet parse `server.server`, extract embedded blocks from it, replace the existing route/module/service boot loaders, or produce a Runtime Image. Those are subsequent RELC stages.

Current conceptual representation:

```text
RelSource {
    id
    kind
    logicalName
    origin
    source
}

SourceOrigin::Physical {
    path
}

SourceOrigin::Embedded {
    container
    blockIndex
    startLine
}
```

The distinction is intentional:

```text
SourceId                  = exact source/diagnostic identity
(kind, logicalName)       = logical compiler/import identity
```

That prevents two different source origins from silently defining the same import target.

## Multi-pass compilation

RELC must not read file A and immediately try to fully compile it before discovering the rest of the application.

Target pipeline:

```text
PASS 1  discover sources
PASS 2  parse REL
PASS 3  collect declarations/exports
PASS 4  resolve imports
PASS 5  build symbol graph
PASS 6  semantic/capability analysis
PASS 7  dependency/recursion analysis
PASS 8  lower to executable representation / IR
PASS 9  optimize
PASS 10 link Runtime Image
```

The Source Registry is the foundation of PASS 1. Physical discovery and `server.server` embedded extraction will feed that registry before later passes consume it.

This lets files refer to symbols whose implementations appear in another source without inventing a fake "compile this file first" rule.

## File graph versus symbol graph

A source-level cycle is not automatically an execution cycle.

```text
A.module -> B.module -> D.module -> C.module -> A.module
```

may resolve to:

```text
A.FUNC_A -> B.lookup -> D.resolve -> C.inspect -> A.FUNC_B
```

which contains no recursive call to `A.FUNC_A`.

RELC therefore records symbol-level edges, not merely file imports.

## Recursive groups

A symbol graph such as:

```text
A.FUNC_A -> B.lookup -> C.inspect -> A.FUNC_A
```

is marked as a recursive group, not rejected merely because the graph is cyclic.

Recursion is valid backend behavior. Examples include hierarchical permissions, nested resources, trees, graphs, schemas, dependency resolution, and parent/child data structures.

The compiler's job is to determine whether references are valid and to preserve recursion metadata for runtime safety.

## Runtime cyclic-computation protection

The runtime must distinguish legitimate recursion from a dependency that cannot make progress.

Valid example:

```text
walk(5) -> walk(4) -> walk(3) -> ... -> base case
```

Broken example:

```text
A.FUNC_A is producing VALUE-X
  -> B.resolve
  -> C.load
  -> A.FUNC_A asks for VALUE-X
```

If VALUE-X cannot exist until the original invocation completes, the operation is a cyclic computation dependency.

Target runtime state:

```text
Uninitialized
Computing(ownerInvocation)
Ready(value)
Failed(error)
```

Target invocation metadata:

```text
Invocation {
    id
    parentId
    symbol
    argumentsFingerprint
    depth
    operationCount
    waitingFor
    producing
}
```

The runtime should abort the impossible invocation chain, not crash the whole RBE process.

## Runtime recursion policy

Server REL should eventually configure normal limits such as maximum depth, repeated-function depth, and operation budget. Hard engine ceilings remain non-disableable safety invariants.

A future `recursive` function modifier may acknowledge intentional recursion and suppress diagnostics, but recursion must not require that keyword to work.

## Runtime Image

The target RELC output is a validated linked Runtime Image:

```text
RuntimeImage {
    imageId
    sourceHash
    serverPolicy
    environment
    routes
    modules
    services
    symbolTable
    dependencyGraph
    middlewarePlan
    serviceAssignments
    capabilities
}
```

After the image becomes active, normal request execution should not reread application source files from disk.

## Reload model

Future reloads should be transactional:

```text
Image A running
   |
source changes
   |
compile Image B
   |
validate all of B
   |
valid? -- no --> keep A
   |
  yes
   |
atomic swap to B
```

## RELC diagnostics

Diagnostics should identify both source kind and source identity. Embedded-file diagnostics should point back into the containing `server.server` block while retaining the virtual source name.

Planned families:

```text
REL1xxx  grammar/parser
REL2xxx  symbol/import
REL3xxx  capability
REL4xxx  runtime-image/linker
SRV1xxx  Server REL policy
RTE1xxx  Route REL
MOD1xxx  Module REL
SVC1xxx  Service REL
REL8xxx  recursion/dataflow
```

The exact numeric catalog should be kept in one error-book source once the RELC refactor lands.


## Runtime source integrity

RELC's successful output is an immutable Runtime Image, and the runtime must execute the exact parsed/lowered objects associated with that image rather than reopening mutable source files. Service processes additionally verify their parent-validated source digest before activation. See [`source-security.md`](source-security.md) for the complete boundary and the planned source-less sealed deployment artifact.
