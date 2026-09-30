# Library Worker Capability Evidence

Status: implemented on `main` for the verified external-library worker path.

This page documents how RBE treats capability declarations from published library packages. It complements [`library-host-web-build.md`](library-host-web-build.md) and the package/install contracts in [`library-system.md`](library-system.md).

## Requests are not grants

A package manifest may request runtime capabilities, but the manifest does not grant them by itself.

```text
package manifest capability declarations
        |
        v
verified capability requests
        |
        v
Backend maps only implemented/allowed requests
        |
        v
trusted Library Host CapabilityGrant set
```

A package therefore cannot gain authority merely by adding another capability name to its manifest. Backend remains the grant authority.

## Capability evidence is not worker identity

RBE deliberately keeps mutable-looking capability evidence out of `VerifiedPackageWorkerIdentity`.

The worker identity is reserved for the executable identity that must remain stable across the launch chain, including package/version, artifact identity, ABI, SDK/runtime identity, and worker entry information.

Capability requests are recovered separately when RBE needs them. This avoids accidentally treating a previously copied capability list as permanent authority.

## Re-read from the SHA-pinned artifact

`VerifiedPackageWorkerIdentity::read_requested_capabilities(...)` re-opens the package through the active project cache/lock state before returning capability requests.

The fail-closed sequence is:

```text
VerifiedPackageWorkerIdentity
        |
        v
read active project lock
        |
        v
find exact locked package root
        |
        v
resolve .cache/library/<artifact-sha256>/artifact.rbe
        |
        v
reject symlinked/unsafe cache path
        |
        v
re-hash complete artifact and match locked SHA-256
        |
        v
inspect package manifest
        |
        v
re-hash manifest and compare locked manifest identity
        |
        v
reconstruct worker identity from lock + manifest
        |
        v
require reconstructed identity == original verified identity
        |
        v
return enabled manifest capability requests
```

If the artifact, manifest, resolved runtime/SDK, package version, or reconstructed worker identity has drifted, RBE fails instead of returning capability evidence from that package.

## Cache location is never authority

The existence of:

```text
.cache/library/<sha>/artifact.rbe
```

is not sufficient evidence by itself. RBE re-hashes the artifact and rejects unsafe/symlinked cache entries before inspecting capability requests.

This follows the general RBE rule that caches are reconstructible state and become trustworthy only after their pinned identities are verified at the boundary that consumes them.

## Disabled capabilities

Only manifest capability entries whose value is enabled are returned as requests. Disabled entries are not converted into requests.

The resulting strings still remain requests, not host grants.

## Library Host boundary

The complete authority split is:

```text
package manifest
    -> declares requested authority

install/runtime verification
    -> proves which exact package bytes made the request

Backend
    -> decides which requests correspond to implemented/allowed host capabilities

Library Host session
    -> owns the actual trusted CapabilityGrant set

Container worker
    -> receives only authority admitted through the trusted host/session boundary
```

Capability request evidence must never be used to bypass the sealed worker proof, Library Host session identity, Container sandbox, or later `library.hello` / `library.accept` validation.

## Why the evidence is re-read

The separation protects against two classes of mistakes:

1. **stale copied authority** — a capability list captured earlier is not silently treated as valid forever;
2. **identity/evidence confusion** — adding or removing a requested capability does not redefine the executable identity object or turn a request into a grant.

When RBE needs capability evidence, it proves the current SHA-pinned package and reconstructed worker identity first, then reads the current verified manifest requests.
