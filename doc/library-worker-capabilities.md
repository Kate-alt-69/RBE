# Library Worker Capability Evidence

Status: implemented on `main` for the verified external-library worker path.

This page documents how RBE treats capability declarations from published library packages. It complements [`library-host-web-build.md`](library-host-web-build.md) and the package/install contracts in [`library-system.md`](library-system.md).

## A package is not limited to the SDK surface

An RBE package is free to implement its own APIs, services, internal backend, queues, adapters, provider integrations, protocols, caches, and other package-specific behavior.

The SDK is the bridge into **RBE-owned privileged resources**. It is not an allowlist of everything a package is allowed to create internally.

For example, a `mail` package may internally define concepts such as:

```text
mail:smtp
mail:queue
mail:provider:resend
mail:provider:sendpulse
```

Those names do not automatically become RBE host privileges and they do not prevent the package worker from starting. They may be entirely private implementation details.

The security boundary begins when package code asks RBE to do privileged work on its behalf, such as network access, storage, router mutation, or another trusted host operation.

## Requests are not grants

A package manifest may request runtime capabilities, but the manifest does not grant them by itself.

```text
package manifest capability declarations
        |
        v
verified capability requests
        |
        v
Backend recognizes the RBE-owned subset it implements
        |
        v
trusted Library Host CapabilityGrant set
```

A package therefore cannot gain RBE authority merely by adding another capability name to its manifest. Backend remains the grant authority.

Unknown, package-specific, or currently unavailable capability names are not promoted into RBE host grants. They also do not make the whole package invalid; package code can inspect the accepted host session and choose its own fallback behavior.

## Current host-owned privileges

`log` is an implicit host-owned capability for every verified package. RBE fixes its target to:

```text
lib/<verified-package-name>
```

Child logger scopes remain structured data below that target, for example:

```text
lib/mail
lib/mail/smtp
lib/mail/smtp/delivery
```

A package cannot retarget logging to another package identity.

`net:http` is an explicit verified runtime request mapped into an RBE host grant. It is dispatched through RBE's hardened public-HTTP broker rather than giving package code a raw socket or unrestricted host network handle.

`net:dns` is also an explicit verified runtime request. Approved package workers may request only the bounded `lookup`, `ip`, and `mx` operations through RBE's DNS broker. The broker normalizes names, rejects local-only/single-label/private targets, and never gives the package a raw resolver or socket. The trusted Backend dispatcher accepts the call only when the retained Library Host session contains the matching `net:dns` grant.

Other package-defined capabilities remain package-owned unless and until RBE deliberately implements a privileged host surface for them.

## Capability evidence is not worker identity

RBE deliberately keeps capability evidence out of `VerifiedPackageWorkerIdentity`.

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

Backend performs this re-read while constructing the retained Library Host session. It then maps only the supported RBE-owned requests into grants.

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

## Privilege disclosure UX

Installer/UI permission text should describe **RBE privileges**, not package architecture.

A package having an internal backend, queue, worker, or provider adapter is not itself a permission and does not need to be exposed as implementation trivia. A user-facing disclosure should instead describe meaningful authority such as:

- network access;
- persistent storage;
- route registration or mutation;
- other advanced RBE host access.

This keeps the permission model simple without hiding actual RBE authority. CLI and future GUI/Studio installers should derive the disclosure from the same verified package capability evidence used by Backend, rather than trusting package-written prose.

## Library Host boundary

The complete authority split is:

```text
package implementation
    -> may define arbitrary package-private behavior

package manifest
    -> requests RBE privileges when package code needs host authority

install/runtime verification
    -> proves which exact package bytes made the request

Backend
    -> maps only supported RBE-owned requests into trusted grants

Library Host session
    -> owns the actual CapabilityGrant set

Container worker
    -> receives only authority admitted through the trusted host/session boundary
```

Capability request evidence must never be used to bypass the sealed worker proof, Library Host session identity, Container sandbox, or later `library.hello` / `library.accept` validation.

## Why the evidence is re-read

The separation protects against two classes of mistakes:

1. **stale copied authority** — a capability list captured earlier is not silently treated as valid forever;
2. **identity/evidence confusion** — adding or removing a requested capability does not redefine the executable identity object or turn a request into a grant.

When RBE needs capability evidence, it proves the current SHA-pinned package and reconstructed worker identity first, then reads the current verified manifest requests.
