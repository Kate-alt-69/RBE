# Library Host & Managed Web Build Security

Status: implemented RBE contracts through `LIBHOST-046` and `WEBBUILD-009` on `main`. This document covers the verified external-library worker launch path, the Container worker proxy, the live sandboxed Library Protocol relay, managed Bun/npm web-build plans, and the trust rules that external coordinators must preserve when they schedule RBE-backed work.

This document complements [`library-system.md`](library-system.md). The package lock and verified package graph remain the package activation authority; an external queue, preview service, scheduler, or build coordinator is not allowed to widen that authority.

## 1. Current trust chain

RBE does not launch an external library worker from a package name, mutable cache path, or host `PATH` lookup alone. The current worker contracts are built from verified package state and carry integrity evidence forward until immediately before process execution.

```text
package.rbe.yaml + package.lock.rbe.yaml
                 |
                 v
verified RPX root snapshot
                 |
                 v
Library Host accepted session identity
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
namespace/cgroup/no_new_privs/Landlock/seccomp
                 |
                 v
library.proxy.ready
                 |
                 v
raw Library Protocol relay
                 |
                 v
library.hello
                 |
                 v
package identity + ABI validation
                 |
                 v
library.accept
```

The important rule is that each boundary re-validates the identity it is about to trust. A previously verified path is not permanently trusted merely because it still exists at the same location.

`library.proxy.ready` is not package/session acceptance. It says only that the trusted Container-side sandbox boundary has been established for the expected child. Package identity, ABI compatibility, and admitted host capabilities remain Library Host decisions at the later `library.hello` / `library.accept` boundary.

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

`container --library-worker-proxy` is the trusted process boundary used for verified external library workers on the currently supported Unix/Linux path.

Before executing the worker, the proxy verifies the bootstrap and launch inputs again. The executor then applies the Container sandbox with the following properties:

- environment cleared before the child is executed;
- direct network policy set to deny-all for the worker execution path;
- no shell-mediated launch;
- cgroup-v2 process/memory/CPU enforcement;
- `no_new_privileges` before the final worker exec;
- workspace Landlock restrictions on the supported Linux path;
- restricted seccomp policy before the final worker exec;
- executable/source verification repeated after sandbox setup and before exec.

The proxy now has two deliberately different execution shapes.

### 3.1 One-shot proxy execution

The existing one-shot execution path remains available for bounded command-style execution. It captures bounded stdout/stderr, enforces a wall-time limit, terminates the cgroup on timeout/output overflow, waits for process completion, and returns a validated `LibraryWorkerProxyResult`.

That mode is not the long-lived Library Host transport.

### 3.2 Live Library Protocol relay (`LIBHOST-046`)

The live proxy mode uses `startup_timeout_seconds` only for establishment of the secure worker boundary. Once the child has completed sandbox setup and emitted `library.proxy.ready`, stdin/stdout become a transparent long-lived Library Protocol channel.

The live path:

```text
Backend-side proxy stdin
        |
        v
strict verified bootstrap
        |
        v
Container verification
        |
        v
sandbox child
  - cgroup
  - private network namespace / deny-all direct network
  - no_new_privs
  - Landlock
  - seccomp
  - final executable/source re-verification
        |
        v
library.proxy.ready
        |
        v
exec managed Bun / Node / Python worker
        |
        v
raw framed Library Protocol over stdin/stdout
```

The proxy parent validates that the ready PID is the exact child it launched. The internal sandbox child keeps the same PID across the final Unix `exec`, so a forged/mismatched ready identity is rejected before protocol forwarding begins.

If the controlling stdin closes, the live proxy treats that as loss of host authority and kills the worker cgroup instead of intentionally leaving an orphaned package process behind.

The proxy execution child has an explicit unsupported-platform failure outside the currently implemented Unix/Linux path. Do not describe the cgroup/Landlock/seccomp worker execution path as a portable Windows/macOS implementation until equivalent platform enforcement is actually wired.

## 4. Proxy startup status and Library Host handshake

The Container proxy emits exactly one framed startup status before raw Library Protocol traffic may begin:

```text
library.proxy.ready
```

or:

```text
library.proxy.reject
```

`ready` carries the proxy protocol version and a non-zero process ID. `reject` carries a bounded machine-readable code and printable bounded message. Unknown fields, unsupported protocol versions, invalid IDs, invalid codes, and invalid messages are rejected.

The complete intended live transition is:

```text
verified bootstrap
→ Container byte verification
→ sandbox establishment
→ library.proxy.ready
→ raw Library Protocol relay
→ library.hello
→ expected package/runtime/SDK/ABI identity validation
→ library.accept
→ package invocation + authorized host calls
```

Backend must not proceed to `library.hello` until it has consumed a valid `library.proxy.ready` frame. A failed/rejected proxy startup is not equivalent to a worker handshake failure after readiness; keeping these states separate prevents ambiguous partial startup.

Likewise, a valid `library.proxy.ready` does **not** imply that the worker has any host capability grants. Those grants are derived from the actual trusted `CapabilityGrant` set installed into the matching Library Host session and are exposed only after a successful worker hello.

### 4.1 Stdout is protocol-owned

After `library.proxy.ready`, worker stdout is reserved exclusively for framed Library Protocol bytes. Package/SDK diagnostics and normal logging must use stderr.

This separation is a protocol invariant, not merely a style preference: writing arbitrary log text to stdout can corrupt the framed channel and must never be interpreted as trusted proxy/session state.

The live proxy relays stderr separately from the Library Protocol channel.

### 4.2 Current integration boundary

`LIBHOST-046` implements the secure live Container relay itself. Route Engine now also exposes a Runtime-Image-only package-export injection point: a trusted host can supply `PackageExportCaller`, and linked `X from Y` Module REL calls are forwarded through that logical package/export/operation bridge. The legacy filesystem router deliberately has no such package runtime because it has not passed the immutable `PackageLinkContext` gate.

Backend still needs the final owner-side worker integration that selects the trusted packaged proxy executable, sends the sealed bootstrap, consumes `library.proxy.ready`, attaches the resulting channel to the retained `LibrarySessionBinding`, requires the existing `library.hello` handshake, and implements `PackageExportCaller` on top of that accepted live worker channel.

Until that owner path is connected, documentation must not claim that every installed package worker is already launched end-to-end through live proxy mode. A Runtime Image accepting a package caller is an execution injection boundary, not proof that Backend has started or accepted the corresponding worker.

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

### 7.1 Mutable queue/index updates require CAS

Safe attempt leasing prevents two workers from owning the same attempt, but it does not prevent two backend instances from losing each other's changes to a shared mutable queue or index.

A multi-instance coordinator should therefore mutate shared indexes with a storage-level compare-and-swap contract:

```text
GET queue/index + version token (ETag)
        |
        v
apply local add/remove cleanup
        |
        v
conditional replace: If-Match <ETag>
        |
   +----+----+
   |         |
updated    stale
   |         |
 done       re-read + retry (bounded)
```

The first creation of a shared index should remain create-if-absent. Existing-index replacement should require the exact version token returned by the same read. An unconditional `GET -> modify -> PUT` is not safe merely because each process holds its own local mutex.

CAS retries must be bounded. Exhausting the retry budget is an error, not permission to fall back to an unconditional write.

### 7.2 Terminal result publication requires attempt fencing

Lease validation only at the beginning of a long completion request is insufficient. A worker may validate attempt `N`, spend time verifying or materializing immutable output, then outlive its lease while another instance advances the mutable job to attempt `N+1`.

Before a worker publishes externally visible terminal state, the coordinator should therefore perform a final generation fence:

```text
verified immutable output
        |
        v
re-read mutable BuildJob + ETag
        |
        v
re-validate immutable attempt-N lease
        |
        v
verify attempt/source/output identity still matches
        |
        v
CAS terminal BuildJob state with If-Match
        |
   +----+----+
   |         |
updated    stale
   |         |
 winner     reject stale worker
   |
   v
publish externally visible record/result state
```

The terminal mutable job transition is the fence. A stale worker must not publish a public/live result after its generation has been replaced.

Output-upload preparation should use the same principle: read the mutable job with a version token, validate the current attempt lease, then CAS the output metadata instead of performing an unconditional mutable write.

Failure publication also needs ordering. A safe pattern is to publish the mutable error state while retaining the current lease as a short fence, update the externally visible error state, and only then release the mutable lease fields. While that active lease is retained, another retry attempt must remain busy. If the process crashes, the immutable lease expiry still provides eventual recovery instead of requiring unsafe lock deletion.

Cross-object publication is not automatically transactional merely because every individual object uses CAS. Coordinators should preserve ordering so that the object which authorizes the generation is fenced before dependent public/result records are published, and dependent record updates should themselves use CAS so concurrent user changes are preserved rather than overwritten.

### 7.3 Chunked source uploads require immutable slots and CAS progress

A chunked source upload has two different kinds of state: the chunk payloads themselves and the mutable upload-progress descriptor. They should not have the same write semantics.

The safe multi-instance pattern is:

```text
begin upload
   |
   v
create descriptor if absent
(or validate/resume matching existing descriptor)
   |
   v
GET descriptor + ETag
   |
   v
create exact chunk slot if absent
   |
   +--> existing identical chunk = idempotent retry
   |
   +--> existing different chunk = conflict / fail closed
   |
   v
advance uploadedChunks/uploadedBytes
   |
   v
CAS descriptor with If-Match <ETag>
   |
   +--> stale = re-read + bounded retry
```

Required invariants:

- beginning the same upload must not reset an already-progressed descriptor back to zero;
- the deterministic descriptor identity is create-if-absent, and an existing descriptor must match the expected owner/project/source/artifact configuration before it is resumed;
- chunk slots are immutable create-if-absent objects rather than mutable overwrite targets;
- an already-present chunk is accepted only when its index, declared size, and bytes/encoded payload match the retried request exactly;
- a different payload for an occupied chunk slot is a conflict, never an overwrite;
- descriptor progress is advanced with ETag/version CAS and bounded retries;
- future/out-of-order chunk slots remain rejected even though a repeated already-committed chunk may be treated idempotently;
- storing the chunk before CASing descriptor progress makes a crash between those two operations recoverable: retry observes the identical immutable chunk and can safely retry only the descriptor transition.

This keeps uploaded source bytes stable while allowing the progress record to remain a small mutable state machine. A process-local mutex may still reduce contention, but it is not the cross-instance authority.

### 7.4 Mutable records and deterministic jobs need distinct authority

An externally visible project/preview record is mutable authority and should be updated with ETag/version CAS. A writer must re-read and merge against the newest record instead of replacing fields it does not own. For example, publishing a new source revision should preserve a concurrently changed visibility value, while a visibility change should preserve the newest source/build state.

A public/discovery index is different when its mapping is immutable. If the public identity and project always map to the same authoritative record key, the index can be a create-once derived pointer. Visibility/security decisions must then be made from the authoritative record after following that pointer. This avoids delete/recreate races where one instance removes an index while another instance publishes a newer allowed state.

Deterministic build-job identities should follow the same create-once principle. If a job ID is derived from owner/project/source identity, creation should use create-if-absent. When the object already exists, the coordinator must verify its immutable identity fields and reuse it; an unconditional replacement could roll a claimed job back to `queued`, clear its attempt/lease state, or erase output metadata. Terminal `live`/`superseded` jobs should not be requeued.

Queue/status publication that depends on the mutable authoritative record should also use CAS and must not downgrade a newer state. In particular, a late queue writer must not replace `building` with `queued`, and it must not replace a clean `live` record for the same source with a dirty queued state.

The resulting ownership split is:

```text
immutable source/chunk/job identity   -> create-if-absent + identity verification
mutable progress/result/visibility   -> ETag CAS + bounded retry + field-aware merge
derived discovery pointer            -> create-once mapping; never security authority
```

## 8. Failure and restart behavior

RBE and external coordinators should prefer fail-closed recovery over guessing ownership after a crash.

For RBE worker launch:

- altered executable/source bytes invalidate the launch;
- invalid proxy bootstrap/status invalidates startup;
- sandbox setup failure invalidates startup;
- one-shot proxy timeout/output overflow terminates the cgroup and reports a bounded result;
- live-proxy startup timeout terminates the cgroup before Library Protocol begins;
- live controlling-stdin EOF kills the worker cgroup;
- Library Protocol begins only after a valid proxy-ready status;
- package authority still begins only after a valid `library.hello` / `library.accept` handshake.

For an external multi-instance build coordinator:

- an unexpired immutable lease blocks a second owner;
- an expired attempt may advance to a new attempt/slot;
- stale workers must fail output-prepare/completion authority checks;
- mutable queue add/remove paths use bounded CAS rather than unconditional read-modify-write replacement;
- terminal result publication re-reads the current generation, re-validates lease authority, and CASes the terminal job state before exposing dependent result state;
- stale CAS/version mismatches fail closed rather than overwriting a newer attempt;
- repeated identical source chunks are idempotent, while conflicting bytes for an occupied slot fail closed;
- descriptor CAS lets an upload recover after a chunk was durably stored but progress publication was interrupted;
- mutable public/result records are merged with CAS so source, visibility, and build-state writers cannot blindly overwrite one another;
- deterministic BuildJob creation is create-if-absent, and an existing claimed/terminal job is never reset by a later queue request;
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
- `install-executor/src/worker_launch.rs` and `install-executor/src/worker_proxy.rs` — sealed worker launch inputs and strict Container proxy bootstrap bridging;
- `install-executor/src/web_build.rs` — managed Bun/npm lock-resolution and final-build contracts;
- `container-runtime/crates/ipc-protocol/` — worker proxy bootstrap/result/status framing;
- `container-runtime/crates/container-bin/src/library_worker_proxy*.rs` — Container-side independent verification, one-shot execution, live sandbox setup, startup status, and raw Library Protocol relay.

When these boundaries change, update this document in the same RBE change so external hosts do not accidentally depend on an older trust model.
