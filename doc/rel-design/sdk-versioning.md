# Proposed SDK compatibility improvements

Status: design proposal from the [roadmap](../rel-roadmap.md). RBE already has a
[Library Protocol handshake](../../sdk/protocol/README.md) with package identity,
SDK version, ABI range, runtime metadata and feature/capability separation. Extend
that foundation; do not introduce a competing handshake or assume it is absent.

## Distinct version meanings

| Identity | What it answers | Proposed treatment |
| --- | --- | --- |
| SDK package version | Which JS/Python/Rust wrapper API did the author install? | Immutable package versions with normal SemVer and migration notes |
| REL language level | Which grammar/type semantics does the application require? | Explicit supported level/features in authoring/link metadata; field names need design |
| Protocol version | Can host and worker decode each other's frames? | Retain existing versioned length-bounded framing and handshake |
| Capability ABI/range | Can a compiled caller invoke this operation with compatible semantics? | Retain/check admitted ABI range and exact operation contracts |
| Optional feature identifiers | Is a mechanism such as streaming available? | Discover through existing feature namespace; availability never grants authority |
| Binary version | Which human-readable runtime version/channel is selected? | Existing `VERSION` and `rbe-bin-*` policy |
| Immutable build identity | Exactly which source/artifact bytes ran? | Source revision, immutable build ref and verified digest for replay/rollback |

One number cannot answer all of these questions. A newer wrapper package need not
require a new protocol; a grammar change need not change network framing; a moving
binary version tag does not uniquely identify executable bytes.

## Compatibility and authoring

Inventory existing package manifests, lockfiles, RELC image metadata and accepted
session introspection before choosing new fields. Declare required language/API
features where the host can reject unsupported code before request handling.
Keep diagnostics actionable: required versus available level/ABI/feature and a
supported upgrade or downgrade path, without tokens or private package contents.

New syntax requires shared grammar fixtures, formatter/editor support, useful
source spans and documentation. SDK stubs should expose asynchronous operations,
cancellation and resource limits consistently across JS, Python and Rust. A wrapper
must not claim cancellation support if the host only abandons a wait while work
continues; document that distinction.

Keep features and capabilities separate. An older runtime can safely reject a
required unknown feature or allow an explicitly optional fallback. Missing feature
introspection must be reported as unavailable, not guessed from a version string.
Fallback must not broaden permissions or change transactional guarantees.

## Binary tags and reproducibility

The existing [binary distribution contract](../binary-updates.md) already provides
OS/architecture-specific downloads, refreshable `rbe-bin-v<version>` and
`rbe-bin-latest`, plus immutable internal build tags. Preserve the user's policy:
a newer successful commit with unchanged `VERSION` refreshes that version's
downloads; a version-file change starts the next numbered version.

Lock deployments and compatibility tests to immutable build identity/digests,
even when discovery starts from a moving alias. Never replace an already
published npm/PyPI/crates package version with different bytes; publish a new
package or development version. Binary alias refreshes and registry package
immutability serve different purposes.

Automatic standalone updates remain opt-in, with compatibility/preflight checks
and recovery as documented. Hosted deployments use their platform's rollout
mechanism. Binary rollback is not a rollback of application data migrations.

## Delivery and checks

SDK-01 inventories identities and settles language-level semantics. SDK-02 adds
only missing metadata, admission checks and diagnostics. SDK-03 adds a
producer/consumer compatibility matrix for each supported language and target.

Cover older SDK/newer runtime, newer SDK/older runtime, ABI mismatch, unknown
required/optional features, absent introspection, wrong source/digest, and the
same human-readable binary version built from two commits. Test that unsupported
features fail before host work, permissions do not expand through negotiation,
and lockfile pins remain reproducible after a moving alias refresh.
