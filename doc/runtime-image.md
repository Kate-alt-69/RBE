# RBE Runtime Image

The **Runtime Image** is the immutable linked application snapshot produced by RELC and activated by the running backend. It is the boundary between mutable deployment inputs and the program/configuration RBE actually executes.

This page documents the current Runtime Image contract on `main`.

## What the image contains

The current Rust representation contains, in practical terms:

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

`executables` stores immutable parsed Route/Module/Service/Server program snapshots. Route-native WASM artifacts and explicit interpreter-fallback reasons are stored beside those program snapshots rather than being rediscovered from mutable source at request time.

## Two cryptographic identities

RBE currently derives two deterministic SHA-256 identities:

```text
sourceHash = SHA-256(canonical REL source set)
imageId    = SHA-256(sourceHash + Route-WASM ABI/compiler versions + canonical settings)
```

Both values are lowercase 64-character hexadecimal strings.

These are **content identities**, not digital signatures. They detect/bind exact inputs; they do not prove who produced those inputs. Persistent signed RBI packaging remains separate work.

## `sourceHash`

`sourceHash` is domain-separated with:

```text
RBE_SOURCE_SET_V1
```

Before hashing, RELC canonicalizes the source set by sorting entries by `SourceId`. The hash input binds:

- the number of sources;
- every exact `SourceId`;
- every exact source byte sequence.

Fields are length-delimited before being fed into SHA-256, so the identity does not rely on ambiguous string concatenation.

A source edit, source addition/removal, or a changed source identity therefore changes `sourceHash`.

## `imageId`

`imageId` is domain-separated with:

```text
RBE_RUNTIME_IMAGE_V3
```

The current image identity binds:

1. the hexadecimal `sourceHash`;
2. `ROUTE_WASM_ABI_VERSION`;
3. `ROUTE_WASM_COMPILER_VERSION`;
4. the effective settings JSON used by RELC.

Settings JSON is hashed deterministically: object keys are sorted, arrays retain order, values retain their JSON type, and each encoded component is length-delimited.

That means equivalent JSON objects with different object-key ordering produce the same image identity, while a meaningful setting/value change produces a different `imageId`.

## Why compiler/ABI versions are included

A source file can remain byte-for-byte identical while the native Route-WASM compiler or ABI changes. Binding those versions into `imageId` prevents RBE from treating native artifacts produced under a different compiler/ABI contract as the same linked application image.

This matters even while only a strict Route REL subset lowers to native WASM: the image records both native artifacts and interpreter fallback decisions under one identity.

## Capability binding

The Container Controller treats Runtime Image identity as authority metadata, not decoration.

Capability manifests are registered against the exact tuple:

```text
Runtime Image ID
+ SourceId
+ Environment
+ Environment generation
```

The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image.

Native WASM artifact registration is also bound to the exact `Runtime Image ID + SourceId + capability ABI`. Artifact bytes remain deduplicated by SHA-256 in the Container cache, but cache presence alone is never execution authority: Execute must present an artifact hash explicitly registered for that Runtime Image source.

Replacing an Environment generation also invalidates grants tied to the previous generation, so image identity and process-generation identity participate together in the capability boundary.

See [`runtime.md`](runtime.md) and [`source-security.md`](source-security.md).

## Runtime source authority

Once an image is active, the image—not the mutable REL source files—is the normal runtime authority.

Current behavior includes:

- Route execution from linked Route program snapshots;
- Module resolution from linked Module program snapshots;
- resolved typed Runtime ENV stored in the image;
- resolved ServerPolicy stored in the image;
- deterministic MiddlewarePlan stored in the image;
- symbol/dependency/recursion metadata stored in the image;
- native Route-WASM artifacts/fallbacks pinned to the image;
- Service assignments/capability metadata pinned to the image.

Service child activation still has its own source/catalog fingerprint contract until Service execution is transported entirely as a persistent image artifact.

## Atomic activation

`RuntimeImageSlot` owns the currently active `Arc<RuntimeImage>` behind a controlled replacement point.

Readers clone the active immutable snapshot. Candidate image activation follows the model:

```text
Image A active
      |
compile + validate Image B
      |
      +-- failure --> keep A
      |
      `-- success --> atomically replace active snapshot with B
```

A reader that already holds Image A is not mutated underneath itself when Image B becomes active.

The activation primitive is implemented. A complete automatic file-watcher/reload coordinator is still separate work.

## Native Route-WASM artifacts

For routes inside the current native compiler subset, the image stores exact deterministic WASM bytes and artifact identity. Those bytes are the eligible payload for Container artifact registration.

Routes outside the subset store an explicit fallback reason instead of being mislabeled as native.

The public HTTP route dispatcher now executes a linked native Route-WASM artifact through the standalone Container runtime when that exact Runtime Image + SourceId has a native artifact. Admission registers the immutable artifact and an exact capability manifest before execution; the current native subset has no imports, so its manifest is intentionally empty.

Routes outside the native compiler subset continue through the linked REL evaluator using their explicit fallback reason. Once a route is native, Container admission/execution failure is fail-closed and does **not** silently fall back to the in-process evaluator.

Native routes request the logical `general` Environment profile during manifest admission. Container Controller resolves that profile round-robin across the configured `general-N` Environments and returns one exact Environment + generation binding. The Execute request then uses that exact Environment; `general` is never accepted as wildcard execution authority, and the dedicated Payment Environment is never selected by the general profile.

## What `imageId` does not guarantee

The cryptographic identity does **not** by itself provide:

- publisher authenticity;
- secret storage;
- sandbox isolation;
- authorization for a source that has no capability grant;
- exactly-once execution semantics;
- persistent source-less deployment;
- resistance to a fully compromised host/kernel.

Those concerns belong to separate RBE boundaries: Vault, HostBootstrap, capability manifests, Service/Container process isolation, CONTROL ER, Linux sandbox enforcement, and the future signed RBI format.

## Persistent RBI status

The current Runtime Image is an in-memory linked runtime object reconstructed during normal boot from deployment sources/settings.

The planned RBI path will persist a verified source-less image artifact so production packaging can omit raw REL source after a trusted build/deploy step. RBI signing/loading is **not complete yet**, so deleting REL source after normal boot is not a supported deployment model today.
