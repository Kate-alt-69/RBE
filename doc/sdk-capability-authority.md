# RBE SDK package capability authority

Status: current architecture contract for SDK/library package capability requests and host-owned privilege admission.

## Core rule

The SDK surface does **not** define the maximum feature set of an RBE package.

A package may create arbitrary internal APIs, services, queues, adapters, backends, provider integrations, protocols, and capability names. `advanced()` remains the language SDK's generic escape hatch for composing or requesting capability IDs that do not yet have a dedicated convenience helper.

The security boundary is not "is there an SDK method for this?". The security boundary is whether the package is attempting to cross into an RBE-owned host privilege.

```text
package worker
├── arbitrary package-private behavior        -> package-owned
├── arbitrary custom capability names         -> package-owned metadata/contract
└── RBE host privilege request                -> RBE-owned approval + grant boundary
```

Unknown or package-specific capability names therefore do not prevent a worker from starting merely because Backend does not have a built-in dispatcher with the same name.

## Requested is not granted

`library.toml` may declare open-ended runtime/build capability names. Those declarations are requests/evidence, never authority by themselves.

RBE must keep the following concepts separate:

```text
requested capability
    !=
approved capability
    !=
host grant
```

A supported RBE-owned capability becomes callable only after trusted RBE code converts an approved request into an exact `LibraryCapabilityGrant` for a verified package session.

The worker cannot widen its own grant by spelling a capability name at runtime.

## Trusted privilege disclosure

Dangerous/advanced RBE-owned privileges must be disclosed by RBE itself before they are admitted. Package-rendered UI, worker stdout, package logs, or package web content are not valid approval surfaces because an opaque backend could otherwise hide or spoof the disclosure.

The approval surface should identify at minimum:

- verified package name and version;
- artifact SHA-256 identity;
- the RBE-owned privilege being requested;
- a human-readable description of what that privilege can do;
- whether the privilege is required or optional when that metadata exists;
- the exact project receiving the grant.

A denied privilege must stay denied even if package code is hidden behind another internal service/module.

Headless/non-interactive installation must fail closed for new RBE-owned privileges unless an explicit trusted approval policy is supplied.

## Package-private capability names

Examples such as:

```text
mail:smtp
mail:queue
mail:provider:resend
```

may describe package-internal functionality without automatically becoming host authority. RBE does not reject these names merely because they are unknown to the built-in host dispatcher.

If a future RBE/plugin host provider registers one of those IDs as an actual host privilege, that provider must also register its trusted operation/limit policy and privilege disclosure metadata. Only then can approval create a host grant.

## Built-in host privileges

Current host-owned examples include:

```text
log
net:http
```

`log` is special: RBE supplies it as an implicit package capability scoped to the verified package identity. A package named `mail` logs through the host target:

```text
lib/mail
```

Child logger scopes are payload data, not authority targets:

```text
lib/mail/smtp
```

The package cannot retarget logging to another package identity.

Other RBE-owned capabilities must not be inferred from SDK method availability alone. A helper such as `sdk.net().http()` is convenience syntax; actual authority still comes from the verified host-session grant.

## SDK parity

Rust, JavaScript/TypeScript, and Python SDKs should expose the same conceptual contract:

- normal typed convenience surfaces where RBE has a stable primitive;
- `log(...)` with RBE-owned `lib/<verified-package>` scope;
- generic `advanced()` escape-hatch composition;
- no literal `advanced.net()` hierarchy requirement;
- no language SDK may silently grant more RBE authority than another SDK.

The generic advanced API must remain available so new package designs do not require an SDK release merely to invent package-private behavior or name a future capability.

## Install/runtime evidence chain

The target trust chain is:

```text
SHA-verified package artifact
  -> verified library.toml capability requests
  -> trusted RBE privilege disclosure/approval
  -> project/version/artifact-bound admitted capability state
  -> VerifiedPackageWorkerIdentity / root snapshot
  -> exact LibrarySessionBinding grant
  -> authorize_host_call()
  -> trusted host dispatcher
```

At no point may a worker-provided runtime string replace verified package/install evidence.

## Mail package implications

`mail` can therefore implement its own provider layer, SMTP abstractions, queues, templates, retry logic, or other package-specific systems without RBE knowing those concepts.

When `mail` needs RBE-owned authority—such as public HTTP today or future DNS/TCP/TLS primitives—the request crosses the trusted approval boundary. Adding a new typed SDK convenience helper later does not itself increase authority; it only improves DX for an already-defined host capability.
