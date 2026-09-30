# Library Host & Managed Web Build Security

Status: implemented RBE contracts through `LIBHOST-044` and `WEBBUILD-009` on `main`. This document covers the verified external-library worker launch path, the Container worker proxy, managed Bun/npm web-build plans, and the trust rules that external coordinators must preserve when they schedule RBE-backed work.

This document complements [`library-system.md`](library-system.md). The package lock and verified package graph remain the package activation authority; an external queue, preview service, scheduler, or build coordinator is not allowed to widen that authority.

## 1. Current trust chain

RBE does not launch an external library worker from a package name, mutable cache path, or host `PATH` lookup alone. The current worker path is built from verified package state and carries integrity evidence forward until immediately before process execution.

```text
package.rbe.yaml + package.lock.rbe.yaml
                 |
                 v
verified RPX root snapshot
                 |
                 v
Library Host accepted session
                 |
                 v
sealed worker launch proof
  - exact package/version
  - artifact identity
  - managed runtime identity
  - exact executable path + SHA-256
  - source/worker file identities
                 |
                 v
framed Container worker bootstrap
                 |
                 v
re-verify executable + source bytes
                 |
                 v
Container Library Worker Proxy
                 |
                 v
bounded sandbox process
                 |
                 v
library.proxy.ready
                 |
                 v
library.hello / Library Protocol
```

The important rule is that each boundary re-validates the identity it is about to trust. A previously verified path is not permanently trusted merely because it still exists at the same location.

## 2. Sealed worker launch authority

Library Host worker launch state is derived from the same verified root snapshot used for package linking. RBE therefore does not intentionally rebuild worker identity from a later unrelated package-state read.

The launch proof binds the worker to the package/version and the verified runtime/tool inputs selected for that package. Managed executable identity is pinned by absolute path and SHA-256, and the executable is re-hashed before spawn.

A launch must fail closed when any of the following changes unexpectedly:

- package or version identity;
- artifact/source identity;
- selected managed runtime;
- runtime executable path or SHA-256;
- worker entry/source file identity;
- accepted Library Host session metadata;
- protocol or bootstrap shape.

External workers never receive raw Backend router pointers or mutable internal maps. They communicate through the Library Protocol boundary after the trusted host has accepted the worker session.

## 3. Container Library Worker Proxy

`container-library-worker-proxy` is the trusted process boundary used for verified external library workers on the currently supported Unix/Linux path.

Before executing the worker, the proxy verifies the bootstrap and launch inputs again. The executor then applies a bounded Container sandbox with the following properties:

- environment cleared before the child is executed;
- direct network policy set to deny-all for the worker execution path;
- no shell-mediated launch;
- cgroup-v2 resource accounting/limits;
- bounded wall time;
- bounded process count and memory policy;
- bounded stdout and stderr capture;
- cgroup-wide termination on timeout/output overflow;
- `no_new_privileges` before the final worker exec;
- workspace Landlock restrictions on the supported Linux path;
- restricted seccomp policy before the final worker exec;
- executable/source verification repeated after sandbox setup and before exec.

The proxy execution child currently has an explicit unsupported-platform failure outside the Unix path. Do not describe the cgroup/Landlock/seccomp worker execution path as a portable Windows/macOS implementation until equivalent platform enforcement is actually wired.

## 4. Proxy startup status before Library Protocol

The Container proxy emits exactly one framed startup status before raw Library Protocol traffic may begin:

```text
library.proxy.ready
```

or:

```text
library.proxy.reject
```

`ready` carries the proxy protocol version and a non-zero process ID. `reject` carries a bounded machine-readable code and printable bounded message. Unknown fields, unsupported protocol versions, invalid IDs, invalid codes, and invalid messages are rejected.

Backend must not proceed to `library.hello` until it has consumed a valid `library.proxy.ready` frame. A failed/rejected proxy startup is not equivalent to a worker handshake failure after readiness; keeping these states separate prevents ambiguous partial startup.

## 5. Managed web build contracts

RBE has source-only and executable contracts for managed JavaScript web builds. The current supported managed web tools are Bun and npm.

The web build model deliberately separates dependency-lock resolution from the final build:

```text
verified source tree
      |
      v
managed Bun/npm executable (absolute path + SHA-256)
      |
      v
LOCK RESOLUTION PHASE
  restricted network
  npm registry origin only
  package scripts disabled
  shell disabled
  environment cleared
      |
      v
verified dependency lock
      |
      v
locked dependency cache
      |
      v
FINAL BUILD PHASE
  tool re-verified
  source re-verified
  lock re-verified
  environment cleared
  shell disabled
  direct network disabled
  output constrained to approved source/output root
```

The current default registry origin for web lock resolution is `https://registry.npmjs.org/`. Lock resolution is bounded by download-size and timeout policy. The final build is not allowed to silently regain direct network access merely because dependency resolution previously required network access.

RBE also disables Next.js telemetry in the managed build environment. That is a build-environment hardening choice, not permission for package code to access other network destinations.

## 6. External build coordinators are scheduling authority, not package authority

A hosted service may coordinate RBE-backed builds—for example a preview service, CI dispatcher, or project build queue. That coordinator may decide *when* a verified build is attempted, but it must not decide *what package/runtime bytes become trusted*.

The RBE trust boundary remains:

- exact package lock / verified root snapshot;
- exact runtime/tool identity;
- exact source identities;
- sealed worker launch proof;
- trusted Container proxy verification;
- output verification/attestation before activation or publication.

A coordinator-provided job ID, queue record, lease token, URL, cache key, or claimed worker identity is therefore metadata to validate, not an RBE package trust proof.

## 7. Multi-instance coordinator lease rule

When an external coordinator is deployed with more than one backend instance, an in-process mutex is not sufficient to establish single-worker ownership.

The recommended coordinator contract is:

```text
mutable queue/index (advisory discovery)
        |
        v
read candidate job
        |
        v
create immutable attempt-scoped lease record
with storage-level conditional create
        |
   +----+----+
   |         |
created    exists
   |         |
 winner     busy / inspect expiry
   |
   v
re-read mutable job state
   |
   v
publish mutable building state
   |
   v
best-effort queue cleanup
```

Required invariants:

- lease ownership is won through a storage-level conditional create such as `If-None-Match: *` / create-if-absent, not through a local mutex alone;
- lease records are immutable and attempt-scoped;
- the mutable job is re-read after the immutable claim is won so a completed/superseded job cannot be resurrected as `building`;
- queue removal happens only after durable ownership is established;
- a busy candidate must not block scanning unrelated queued work;
- output-upload preparation re-validates the immutable lease authority;
- completion/failure handling re-validates the immutable lease authority again;
- lease-token comparison remains hash-based/constant-time at the trust boundary;
- expiry is part of the authority check, not only UI/status metadata;
- a crashed claimant leaves an expiring immutable attempt record rather than requiring unsafe distributed lock deletion.

The queue itself may remain mutable/advisory. If the queue index is updated by multiple instances, its mutation path still needs a separate compare-and-swap/ETag-style lost-update solution; safe leasing prevents double execution but does not by itself make a mutable read-modify-write queue index linearizable.

## 8. Failure and restart behavior

RBE and external coordinators should prefer fail-closed recovery over guessing ownership after a crash.

For RBE worker launch:

- altered executable/source bytes invalidate the launch;
- invalid proxy bootstrap/status invalidates startup;
- sandbox setup failure invalidates startup;
- timeout/output overflow terminates the cgroup and reports a bounded result;
- Library Protocol begins only after a valid proxy-ready status.

For an external multi-instance build coordinator:

- an unexpired immutable lease blocks a second owner;
- an expired attempt may advance to a new attempt/slot;
- stale workers must fail output-prepare/completion authority checks;
- queue cleanup failure should not transfer ownership to another worker while the durable lease remains active.

## 9. What this document does not claim

This document does **not** claim that RBE owns a particular product's preview URLs, account system, S3 queue schema, or web UI. Those are host-application concerns.

It also does not make a hosted coordinator's lease record part of `package.lock.rbe.yaml`. The package lock is the RBE package-graph activation boundary; a coordinator lease is temporary scheduling authority for one build attempt.

Similarly, the existence of a compiled web artifact does not automatically activate a package/application graph. Activation still requires the relevant RBE verification and activation contracts described in [`library-system.md`](library-system.md).

## 10. Current implementation references

The current implementation is primarily split across:

- `engine/crates/backend/` — verified package-root loading, Library Host sessions, launch proof construction and worker/proxy coordination;
- `engine/crates/core/src/library_session.rs` — Library session metadata/authority contracts;
- `install-runtime/` — verified package snapshots and worker identity evidence;
- `install-executor/src/web_build.rs` — managed Bun/npm lock-resolution and final-build contracts;
- `container-runtime/crates/ipc-protocol/` — worker proxy bootstrap/result/status framing;
- `container-runtime/crates/container/` — proxy verification and bounded sandbox execution.

When these boundaries change, update this document in the same RBE change so external hosts do not accidentally depend on an older trust model.
