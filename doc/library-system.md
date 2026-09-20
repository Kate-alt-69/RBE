# RBE External Library System

Status: foundation / Library Protocol v1 design

RBE external libraries are project-local packages that can extend REL/RBE without
being linked into `backend.exe`'s trusted address space. The system is intentionally
language-neutral: Rust, Bun/Node.js, Python and future runtimes all speak the same
RBE Library Protocol and receive the same capability decisions.

## 1. Library namespaces in REL

Built-ins and external packages use one hierarchical model:

```rel
:import[net:http]
:import[net:p2p]
:import[crypto:argon]
:import[advancenet:retry]
```

A root import is only a namespace:

```rel
:import[net]

const <= http => net.http()
const <= peer => net.p2p()
```

Direct import and root accessor resolve to the same sub-library identity.
Importing `net` does not grant every child capability. Existing `:import[http]`
remains a compatibility alias for `net:http`.

## 2. Project-local package layout

All package state is relative to the directory in which `backend` starts:

```text
project/
├── backend(.exe)
├── server.server
├── library/
│   ├── advancenet.zip
│   └── image-tools.zip
├── rbe.lock
└── .rbe/
    ├── library-cache/
    ├── runtimes/
    │   ├── rust/
    │   ├── bun/
    │   ├── node/
    │   └── python/
    ├── sdk/
    └── registry/
```

RBE does not require global package installation. Managed runtimes/toolchains are
installed into `.rbe/runtimes` and pinned by project state unless policy explicitly
chooses a compatible system installation.

## 3. Package ZIP

A source package contains source, lockfiles and a required `library.toml` rather
than prebuilt artifacts for every OS:

```text
advancenet.zip
├── library.toml
├── src/
├── Cargo.toml + Cargo.lock        # Rust, when applicable
├── package.json + bun.lock        # Bun/Node, when applicable
├── pyproject.toml/requirements    # Python, when applicable
└── vendor/                        # optional offline dependencies/tooling
```

Installer requirements include traversal-safe extraction, bounded file count and
size, no absolute paths or `..` escapes, no symlink escape, canonical-path duplicate
rejection, hash/signature checks and atomic activation.

## 4. Language/runtime declaration

`library.toml` declares what the package is, which SDK/runtime it uses and how
RBE should prepare it. Example Bun package:

```toml
name = "advancenet"
version = "1.0.0"
language = "javascript"
rbe_abi_min = 1
rbe_abi_max = 1

[sdk]
family = "javascript"
package = "@rbe/sdk"
version = "0.1"

[runtime]
kind = "bun"
version = "1.x"
managed = true
entry = "src/index.js"

[exports]
root = true
sublibraries = ["retry", "proxy", "mesh"]

[capabilities]
"net:http" = true
"net:p2p" = true
"router:read" = true
"router:register" = false
```

Python can declare `runtime.kind = "python"`; Node can use `node`; Rust uses
`rust`/Cargo. `language = "other"` is allowed only with an explicit runtime or
build/launch contract. `other` is not permission to blindly execute an unknown
binary.

## 5. OS-specific build/install scripts

A package may define structured steps for the supported OS families:

```toml
[[build.windows]]
program = "bun"
args = ["install", "--frozen-lockfile"]

[[build.linux]]
program = "bun"
args = ["install", "--frozen-lockfile"]

[[build.macos]]
program = "bun"
args = ["install", "--frozen-lockfile"]

[[build.other]]
program = "bun"
args = ["install", "--frozen-lockfile"]
```

Selection order is exact host OS first and `other` only as an explicit fallback.
Steps are run in RBE's package build environment, not as unrestricted commands in
Backend's own process. Build-time network/filesystem/process powers are separate
from runtime library capabilities.

A package does not need to ship a compiled artifact. `backend install` prepares
it for the current host:

- Rust: acquire/choose a compatible Rust toolchain and build with the locked graph.
- Bun: acquire/choose Bun, install locked dependencies and optionally build/bundle.
- Node.js: acquire/choose Node and the declared package-manager flow.
- Python: acquire/choose Python, create a project-local environment and install
  locked dependencies; bytecode/native-extension compilation may occur as needed.
- Other: follow its declared managed runtime/toolchain and OS steps.

Interpreted/JIT packages remain source packages but still run as isolated RBE
library workers. They do not execute inside `backend.exe`.

## 6. Language-neutral Library Protocol

All workers speak `sdk/protocol` / RBE Library Protocol v1. Startup begins with a
handshake containing at least:

```text
package identity + content hash
SDK language/name/version
runtime kind/version
ABI min/max
protocol version
```

RBE compares the hello frame against the admitted package manifest and `rbe.lock`.
The worker cannot request new authority during the handshake.

Privileged operations are host calls such as:

```text
capability = net:http
target     = net:http
operation  = request
payload    = ...
```

The host checks the package's admitted capabilities on every call.

## 7. SDK family

First-party SDK surfaces begin with:

```text
Rust                  sdk/rbe-sdk       -> registry package `rbe-sdk`
Bun + Node.js         sdk/js            -> registry package `@rbe/sdk`
Python                sdk/python        -> registry package `rbe-sdk`
Other languages       Protocol v1 directly or a language wrapper
```

SDKs provide language-native helpers but have identical authority. For example,
`net:http` called from Python, Bun or Rust reaches the same host capability.

The SDK deliberately exposes sanctioned bridges rather than private Backend Rust
structures:

```text
net:*
router:read
router:register
storage
crypto
future service/runtime extension points
```

This enables an `advancenet` package to call RBE's normal network functionality,
add custom fallback/retry/cache/protocol logic, and expose a better/different REL
API without forking RBE.

## 8. Router extension model

Router access is split so introspection does not imply mutation:

- `router:read` — approved route/runtime metadata and inspection.
- `router:register` — validated middleware/protocol/route-factory registration.

External workers never receive raw Axum router pointers/maps. Registration is a
request to trusted RBE code, which validates namespace collisions, ownership,
lifecycle and policy before accepting it.

## 9. Registry / SDK distribution

Kastrick Backend can act as the central index while exposing protocol-compatible
front doors for each ecosystem:

```text
RBE package index/API
├── package search and versions
├── signatures/hashes/publishers
├── dependency metadata
└── ZIP/object download locations

Cargo sparse registry
└── `rbe-sdk` Rust crate

npm-compatible registry
└── `@rbe/sdk` for both Node.js and Bun

Python Simple Repository API
└── `rbe-sdk` wheel/sdist
```

`backend sdk setup` writes only project-local configuration and chooses SDK/runtime
versions compatible with the running Library ABI. Cargo supports alternate sparse
registries; npm supports project `.npmrc`; Bun can consume npm registry config or
`bunfig.toml`; Python installers can consume a standards-compatible simple index.

Normal library authors should usually run RBE commands instead of configuring
these registries by hand:

```text
./backend sdk setup
./backend library new advancenet --language rust
./backend library new advancenet --language bun
./backend library new advancenet --language python
```

## 10. `backend install`

The install command is the package/environment resolver, not merely a downloader:

```text
./backend install advancenet
./backend install https://example.com/advancenet.zip
./backend install
```

Pipeline:

```text
resolve package/index or URL
→ download package
→ verify size/hash/signature
→ safe ZIP inspection
→ parse/validate library.toml
→ resolve dependencies
→ resolve SDK family/version
→ resolve or install project-local runtime/toolchain
→ select windows/linux/macos/other steps
→ execute build/install in package build environment
→ verify resulting worker/entry point
→ perform Library Protocol ABI handshake
→ atomically activate
→ update rbe.lock
```

`backend install` can therefore see a Python/Bun/Node package and understand how
it must be prepared without requiring a precompiled binary in the ZIP.

## 11. Reproducibility

`rbe.lock` records at least exact package version/hash, publisher/signature
identity, dependencies, SDK family/version, runtime kind/version, ABI requirement
and build identity. Deployments restore from the lock instead of silently choosing
newer packages or runtimes.

## 12. Provider portability

Packages target RBE capabilities, not Render/Vercel/Fly/AWS/etc. The Host
Capability Adapter advertises what the current environment actually permits:

```text
inbound HTTP / WebSocket / TCP / UDP
outbound TCP / UDP
QUIC
persistent/background process
public/private ports
filesystem / persistent filesystem
IPv4 / IPv6
connection lifetime
```

RBE selects compatible execution/fallback behavior or fails admission explicitly.

## 13. Expanded network library

Target namespace:

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

The existing public HTTP broker remains a policy boundary. Expanding `net` does
not hand REL or third-party workers unrestricted raw sockets.

## 14. P2P

`net:p2p` coordinates multiple transports rather than meaning one protocol:

- TCP
- UDP
- QUIC
- WebSocket
- WebTransport
- HTTP/2 streams
- HTTP/3 streams

Discovery may include static peers, DNS, mDNS, rendezvous, DHT and RBE Cloud.
NAT traversal can grow into STUN/TURN, ICE-like selection, UPnP, NAT-PMP and PCP,
with relay fallback. Long-lived listeners naturally belong to `.service`
lifecycle and are represented as RBE-owned listener/port leases.

Transport and application protocol are separate. For example QUIC may carry
`RBE-MASK/1`, while WebSocket may carry the same MASK protocol on a restricted
host. External libraries can register their own application protocols when
explicitly granted the relevant extension capability.

## 15. MASK

`net:mask` is a content-addressed mesh distribution/cache layer for updates,
installers, assets and other large versioned artifacts. Signed origin manifests
identify hash-addressed chunks. A transfer may fetch chunks concurrently from
local cache, multiple peers and HTTPS origin.

Peers are untrusted byte sources; authenticity comes from signed manifests and
content hashes. MASK should reuse RBE/Cloud Node content-addressed chunk storage
rather than inventing a second cache filesystem.

## 16. Implementation order

1. Language-neutral Protocol v1 + Rust/JS/Python SDK contracts.
2. Package manifest parser and safe ZIP inspection.
3. Project-local runtime/toolchain manager and `rbe.lock` model.
4. `backend sdk setup` and `backend library new`.
5. URL/indexed `backend install` with OS build selection.
6. Library Host worker lifecycle + authenticated ABI handshake.
7. Net/router/storage/crypto host bridges.
8. Generic REL sub-library linker + `http` compatibility alias.
9. `net:cookies`, `net:headers`, `net:url` baseline.
10. P2P transports/provider adapters.
11. MASK + Storage/Cloud Node integration.
12. Kastrick package publishing/search/signing UI and registry protocol front ends.
