# RBE External Library & Project Package System

Status: current package/install contracts on `main`, implemented through LIB-019 plus CLI-002. Named-package `backend install` now performs real pre-bootstrap registry hydration and root-scoped dependency resolution. Full durable installation is intentionally not reported as complete until verified artifact inspection, manifest hashing, promotion, session activation, and project-lock commit are connected end to end.

This document is the authoritative overview for the current external-library and project-package architecture. It distinguishes implemented contract/runtime behavior from planned UX so old design text does not override code.

## 1. Library namespaces in REL

Built-ins and external packages use one hierarchical import model:

```rel
:import[net:http]
:import[net:p2p]
:import[crypto:argon]
:import[advancenet:retry]
```

A root import is a namespace:

```rel
:import[net]

const <= http => net.http()
const <= peer => net.p2p()
```

Direct import and root accessor resolve to the same sub-library identity. Importing a root namespace does not grant every child capability. Compatibility aliases may map older flat imports onto the hierarchical name without changing authority.

## 2. Current project package state

The current project package path is built around `package.rbe.yaml`, `package.lock.rbe.yaml`, a content-addressed package cache, RBE-owned managed tools, dependency hydration caches, verified artifact staging, and a durable install session.

```text
project/
├── backend(.exe)
├── server.server
├── package.rbe.yaml
├── package.lock.rbe.yaml
├── library/                              # optional local package inputs
└── .cache/
    ├── library/
    │   ├── <artifact-sha256>/            # verified package/artifact cache
    │   └── .staging/                     # resumable/incomplete install staging
    └── rbe/
        ├── sys/
        │   ├── python/<version>/<host>/
        │   ├── nodejs/<version>/<host>/
        │   ├── bunjs/<version>/<host>/
        │   └── rust/<version>/<host>/
        ├── build-deps/
        │   └── <ecosystem>/<lock-sha256>/
        │       └── hydration.rbe.json
        └── install/
            ├── journal.rbe.json
            └── install.lease
```

The package cache is reconstructible state, not trust. Presence under `.cache/` never proves integrity by itself.

Older library/runtime code still contains `.rbe/runtimes/...` paths. That legacy path is not the authoritative cache layout for the newer project-package/system-runtime install path described here; do not silently rewrite compatibility code merely to make strings match this document.

## 3. Project manifest and lockfile

`package.rbe.yaml` is the human-edited project dependency manifest.

`package.lock.rbe.yaml` is the exact resolver output and the activation boundary for the project package graph. The lock records the exact package/runtime/SDK resolution needed to restore the graph, including pinned artifact identity such as exact URLs and SHA-256 values.

Important invariants:

- deleting `.cache/library` does not require version re-resolution when the lock is intact;
- deleted bytes still have to be downloaded again;
- cached bytes must be re-verified against the lock before use;
- a partially prepared new graph does not become active merely because some packages finished building;
- graph activation occurs only after all required packages have reached the verified/ready state;
- the installer must not invent a manifest SHA-256 before the verified package artifact has been inspected.

## 4. RBE-owned system runtimes

RBE can hydrate private portable tools without installing them globally. The current managed system-runtime identities are:

```text
rbe.sys.python
rbe.sys.nodejs
rbe.sys.bunjs
rbe.sys.rust
```

A system-runtime manifest is requested from the trusted registry under:

```text
/registry/v1/system-runtime/<runtime-key>/<host>/manifest.json
```

The manifest binds runtime identity, version, host, HTTPS source, SHA-256, size, archive kind, and entrypoint. Optional publisher/signature metadata can also be carried.

For a project cache root, runtime material belongs under:

```text
.cache/rbe/sys/<runtime>/<version>/<host>/
```

System runtime hardening requires:

- no machine-wide PATH mutation;
- not exposed as a user-installable package merely because RBE uses it internally;
- HTTPS artifact sources;
- SHA-256 verification on every use rather than trusting the cache location;
- user-private cache permissions (`0700` policy on Unix and a user-only ACL policy on Windows);
- host/runtime identity validation before activation.

These tools are infrastructure for trusted install/build/discovery work. They are distinct from ordinary external packages.

## 5. External package source layout

A source package carries package metadata, source, and the lock material needed for deterministic preparation. The package metadata remains separate from the project-level `package.rbe.yaml`.

A representative source archive can contain:

```text
advancenet.zip
├── library.toml
├── src/
├── Cargo.toml
├── Cargo.lock                     # Cargo ecosystem
├── package.json
├── package-lock.json              # npm ecosystem
├── bun.lock                       # Bun ecosystem
├── pyproject.toml
├── requirements.lock              # hash-locked Python ecosystem
└── other package-owned files
```

Extraction is not a normal unzip into a trusted directory. The install path performs bounded, traversal-safe extraction into fresh staging state, rejects path escapes/unsafe archive structure, and computes a deterministic source identity independent of archive ordering/compression metadata.

The current deterministic source-tree identity is `rbe-source-tree-sha256-v1`.

## 6. Download, cache, and verified artifact ingress (LIB-014 / LIB-019)

Artifact acquisition is bounded and resumable rather than an unbounded download straight into the final cache.

The execution contracts include:

- disk-space preflight;
- maximum artifact/download bounds;
- `.part` staging;
- resumable transfer policy;
- streaming SHA-256 verification;
- final size/hash verification;
- atomic promotion planning only after verification;
- fresh extraction staging;
- source-tree hashing before activation.

LIB-019 adds the trusted host-side network executor for these contracts. `rbe-install-runtime::stage_artifact` performs bounded HTTPS retrieval into the installer staging path, re-hashes reusable partial bytes, validates Range/Content-Range behavior, validates pinned size when present, streams bytes through the verifier, and returns a verified download plus its promotion plan.

The network layer rejects credential-bearing/non-HTTPS URLs, non-public destinations, unsafe redirect behavior, oversized registry graphs/bodies, and unsafe staging filesystem state. Package-controlled code never receives raw sockets or a shell merely because installation requires network access.

Cache paths are intentionally not authority. A matching path with altered bytes must fail verification.

## 7. Package attestation and quarantine

RBE separates package retrieval from package trust.

Attestation can evaluate:

- the locked artifact SHA-256 versus downloaded bytes;
- declared publisher signature state;
- local/extracted source identity versus trusted remote source identity;
- shipped binary identity versus a rebuilt binary when the package explicitly declares a reproducible-build contract.

Byte-for-byte reproducibility is not assumed for arbitrary builds. The shipped/rebuilt binary comparison is meaningful only when the package opts into an appropriate reproducible-build contract.

A mismatch or failed trust gate is a quarantine condition, not an activation condition. Public-registry failures may support privacy-safe reporting; local/private package failures must not be silently reported as public telemetry.

## 8. Controlled build-dependency hydration (LIB-017)

The actual package build stays network-dead. Ecosystem dependencies are hydrated in a separate bounded phase before compilation.

```text
pinned dependency lock
        |
        v
verify lock SHA-256
        |
        v
managed rbe.sys tool only
        |
  restricted network
  approved HTTPS origins only
  no shell
  cleared environment
  dependency scripts disabled where applicable
        |
        v
.cache/rbe/build-deps/<ecosystem>/<lock-sha256>/
        |
        +-- hydration.rbe.json
        |
        v
NETWORK OFF
        |
        v
actual package build
```

### 8.1 Ecosystem rules

| Ecosystem | Required lock | Hydration command contract | Default approved origins | Offline build contract |
| --- | --- | --- | --- | --- |
| Cargo | `Cargo.lock` | `cargo fetch --locked --manifest-path <Cargo.toml>` | `https://index.crates.io/`, `https://static.crates.io/` | same hydrated `CARGO_HOME`, `CARGO_NET_OFFLINE=true` |
| npm | `package-lock.json` | `npm ci --ignore-scripts --no-audit --no-fund --cache <cache>` | `https://registry.npmjs.org/` | same cache, `npm_config_offline=true` |
| Bun | `bun.lock` | `bun install --frozen-lockfile --ignore-scripts --cache-dir <cache>` | `https://registry.npmjs.org/` | same `BUN_INSTALL_CACHE_DIR`; build network remains disabled by RBE |
| Python | `requirements.lock` | `python -m pip download --require-hashes --only-binary=:all: ...` | `https://pypi.org/`, `https://files.pythonhosted.org/` | `PIP_NO_INDEX=1`, `PIP_FIND_LINKS=<wheelhouse>`, `PIP_REQUIRE_HASHES=1` |

Registry allowlist entries must be credential-free HTTPS origins. Paths, query strings, fragments, duplicate origins, and insecure schemes are rejected.

Default hydration bounds are currently 1 GiB maximum observed download bytes and 10 minutes timeout unless a stricter policy is supplied.

### 8.2 Hydration receipt

A successful hydration produces `hydration.rbe.json` under the lock-addressed dependency cache. The receipt binds at least:

- receipt format;
- ecosystem;
- dependency-lock SHA-256;
- lock-addressed cache root;
- absolute managed program path;
- hydration arguments;
- approved registry origins;
- cleared-environment state;
- shell-disabled state;
- dependency-script-disabled state;
- origin-restricted-network state;
- hydrated artifact count;
- observed bytes.

Receipt validation rejects a cache root that is not consistent with `rbe/build-deps/<ecosystem>/<lock-sha256>`.

### 8.3 Actual build isolation

Hydration does not relax the build sandbox contract. Build invocations must still satisfy all of the following:

```text
network_allowed   = false
use_shell         = false
clear_environment = true
program           = absolute path from ManagedToolchain
RBE_BUILD_NETWORK = disabled
```

Only explicitly managed tools are selected. There is no fallback to an arbitrary `cargo`, `npm`, `bun`, `python`, compiler, or shell discovered from the host PATH.

## 9. Durable install sessions and atomic graph activation (LIB-016)

Project installation is treated as a graph transaction rather than a sequence of independently activated packages.

The durable session owns:

```text
.cache/rbe/install/journal.rbe.json
.cache/rbe/install/install.lease
```

The lease prevents concurrent project installers from racing the same activation boundary. The journal records durable progress so verified cache work can be reused after interruption.

The intended end-to-end flow is:

```text
package.rbe.yaml / CLI request
→ resolve complete root-scoped graph
→ fetch pinned artifacts
→ verify artifact bytes
→ inspect package manifest and obtain manifest SHA-256
→ construct exact target lock
→ safe extract and source-hash
→ hydrate required rbe.sys tools
→ hydrate locked ecosystem dependencies
→ NETWORK OFF
→ build
→ source/binary attestation
→ every graph member READY?
→ write package.lock.rbe.yaml.next
→ atomic replace package.lock.rbe.yaml
→ new graph becomes active
```

If the process crashes halfway through a multi-package install, the previously active lock remains the activation boundary. Completed content-addressed cache work can be reused; incomplete staging/journal state is recovered or discarded according to the session contract.

## 10. `backend install` command surface and current boundary

The request grammar supports one install surface for packages, SDKs, runtimes, registry targets, and direct external locators. Representative syntax includes:

```text
./backend install advancenet
./backend install advancenet.4.0.1
./backend install sdk.0.1.0
./backend install runtime.python
./backend install runtime.python.3.10
./backend install mycooldevwebsite.here/advancenet/download -version=4.0.1
```

The `install-request` crate is intentionally source-only: it parses/validates requests and produces discovery/runtime plans but performs no network I/O, process spawning, PATH mutation, or machine-wide install itself.

### 10.1 Typed package registry and resolver bridge (LIB-018)

Public package index metadata has a typed source-only contract in `rbe-install-request`. The contract validates requested package identity and release metadata, including artifact source, SHA-256, and byte-size pins, before the metadata can be consumed downstream.

The boundary is intentionally split:

```text
registry response bytes
        |
        v
rbe-install-request::registry
  parse + validate wire/trust metadata
        |
        v
rbe-library-registry
  source-only adapter
  transactional catalog ingestion
        |
        v
rbe-library-resolver
  deterministic semver solving only
```

The resolver does not know registry URLs, JSON structure, or artifact hashes. Conversely, registry artifact trust data is not discarded from the validated registry object merely because it is irrelevant to dependency solving; download/verification code consumes those pins later at the artifact boundary.

Registry-to-catalog ingestion is all-or-nothing. If any release cannot be represented by the resolver, the caller's existing resolver catalog remains unchanged rather than exposing a partially imported package index.

### 10.2 Real pre-bootstrap named-package resolution (CLI-002)

`backend.exe` now intercepts ordinary named-package `install` commands before normal Backend boot. The standalone `service` binary does not receive install authority.

Named-package installation requires an explicit trusted registry base:

```text
RBE_PACKAGE_REGISTRY=https://<trusted-registry-base>/
```

For a normal named package such as `advancenet`, Backend now performs:

```text
canonical InstallCommand parse
→ validate RBE_PACKAGE_REGISTRY
→ fetch + validate root package index
→ hydrate dependency package indexes
→ transactional registry-to-catalog bridge
→ root-scoped deterministic semver resolution
→ select pinned artifact metadata
```

This path runs before settings/bootstrap/server startup. The install resolver uses a short-lived worker/runtime and returns before the normal server boot path can begin.

CLI-002 deliberately stops after real resolution today. A successful resolution returns an unavailable/non-success outcome describing the selected package/artifact instead of printing `installed`, because verified artifact inspection and durable graph activation are not yet connected to `backend.exe`.

That is a correctness boundary, not a placeholder package-not-found response: registry/HTTP/requirement failures now surface from the real registry and resolver path.

Reserved SDK/runtime targets and external/local archive targets remain in their existing lanes until those execution paths are connected through the same trust model.

## 11. Language-neutral Library Protocol and SDKs

External workers remain outside `backend.exe`'s trusted address space and communicate through the RBE Library Protocol / SDK boundary.

Startup identity includes the admitted package identity, content identity, SDK/runtime identity, ABI range, and protocol version. A worker does not gain new authority merely by requesting it during handshake.

First-party SDK families are designed around equivalent authority:

```text
Rust          sdk/rbe-sdk
Bun/Node.js   sdk/js
Python        sdk/python
Other         Protocol v1 directly or through a language wrapper
```

The Rust, JavaScript/Bun/Node.js, Python, and shared Protocol v1 source surfaces are present in the repository. Publishing reproducible downloadable SDK artifacts and binding their final registry URLs/hashes remains distribution/integration work rather than a missing protocol design.

Host capabilities remain policy-checked operations such as `net:*`, router operations, storage, and crypto rather than raw access to Backend internals.

## 12. Router extension model

Router authority stays split:

- `router:read` — approved route/runtime metadata and inspection;
- `router:register` — validated middleware/protocol/route-factory registration.

External workers never receive raw Axum router pointers or mutable internal maps. Registration remains a request to trusted RBE code, which validates ownership, collisions, lifecycle, and policy.

## 13. Provider portability

Packages target RBE capabilities rather than assuming a particular provider such as Render, Vercel, Fly, AWS, Azure, or Google Cloud.

The host capability layer can describe what the environment actually permits, such as inbound/outbound transports, persistent/background execution, filesystem persistence, IP families, port visibility, and connection lifetime. Unsupported authority must fail explicitly or use an RBE-defined fallback; it is not silently granted.

## 14. Expanded network / P2P / MASK direction

The external-library model is intended to support hierarchical namespaces such as:

```text
net
├── http
├── cookies
├── headers
├── url
├── dns
├── ip
├── tcp
├── udp
├── quic
├── websocket
├── webtransport
├── tls
├── mime
├── form
├── p2p
└── mask
```

Expanding the namespace does not hand REL or third-party workers unrestricted raw sockets.

`net:p2p` can coordinate multiple RBE-owned transports and discovery mechanisms. Long-lived listeners naturally belong to `.service` lifecycle and should remain represented as RBE-owned listener/port leases.

`net:mask` remains the direction for a content-addressed mesh distribution/cache layer. Peers are untrusted byte sources; authenticity comes from signed manifests and content hashes, and MASK should reuse RBE/Cloud Node content-addressed storage instead of inventing an unrelated cache trust model.

## 15. Package-system implementation milestones

The current package-system foundation has advanced through these layers:

1. **LIB-010** — cache-backed RBE system-runtime manifests (`rbe.sys.python`, Node.js, Bun, and Rust).
2. **LIB-011** — project `package.rbe.yaml` / `package.lock.rbe.yaml` state and content-addressed cache identity.
3. **LIB-012** — package source/binary attestation and quarantine decisions.
4. **LIB-013** — project install orchestration gates.
5. **LIB-014** — bounded/resumable acquisition and managed build execution contracts.
6. **LIB-015** — safe source extraction and deterministic source-tree identity.
7. **LIB-016** — durable project install session, exclusive lease, and atomic lockfile activation.
8. **LIB-017** — controlled build-dependency hydration followed by network-dead package builds.
9. **LIB-018** — typed package-registry metadata and a transactional source-only bridge into the deterministic resolver.
10. **LIB-019** — trusted host-side registry/artifact network execution, bounded resumable staging, streaming verification, and promotion planning.
11. **CLI-002** — real pre-bootstrap named-package registry hydration and root-scoped resolution in `backend.exe` without granting install authority to the standalone `service` binary.

## 16. Remaining integration work

The major remaining work is not to redesign the package trust model again. It is to connect the already-implemented contracts across the last activation boundaries:

- connect CLI-002's selected artifact metadata to `rbe-install-runtime::stage_artifact`;
- inspect the verified package artifact and derive/validate the package manifest SHA-256 required by `ProjectPackageLock`;
- construct the exact root-scoped target lock only after that inspection;
- connect extraction, source identity, managed runtime/dependency hydration, network-dead build, attestation, and quarantine decisions to the durable install session;
- atomically promote verified cache entries and activate `package.lock.rbe.yaml` only after the complete graph is ready;
- connect worker/Library Protocol admission to the activated project lock;
- finish SDK/runtime/external/local-archive execution through the same trust model;
- finish public registry publishing/artifact-distribution UX and external-index integrations;
- expand capability bridges without weakening the package/process boundary.

When implementation and this document diverge, the implementation on `main` wins and this document should be updated in the same change.
