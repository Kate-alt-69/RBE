# RBE install executor

`install-executor` defines the trusted execution contracts used by RBE package installation. It does **not** itself open sockets, extract archives, write final cache state, or spawn arbitrary shell commands. Trusted Backend orchestration executes these plans and must preserve their security invariants.

The crate currently owns contracts for:

- bounded/resumable artifact acquisition;
- streaming SHA-256 verification and atomic promotion;
- managed RBE system-runtime artifacts;
- safe source extraction and deterministic source-tree identity;
- managed build invocation planning;
- controlled build-dependency hydration (LIB-017);
- transition from restricted hydration networking to network-dead package builds.

## Managed tool boundary

Build and hydration programs come from `ManagedToolchain`. Managed tool paths must be absolute and are selected explicitly by tool name. RBE must not fall back to a host executable discovered through `PATH` merely because a package asks for `cargo`, `npm`, `bun`, `python`, or another compiler/tool.

Normal package build invocations are created with:

```text
clear_environment = true
network_allowed   = false
use_shell         = false
```

LIB-017 adds the dependency cache environment without weakening those values.

## Controlled dependency hydration

Package ecosystems often need registry access before they can build offline. RBE handles that as a separate phase:

```text
verify pinned dependency lock
        |
        v
managed RBE tool
        |
restricted network to explicit HTTPS origins
no shell
cleared environment
install scripts disabled where supported
        |
        v
.cache/rbe/build-deps/<ecosystem>/<lock-sha256>/
        |
        +-- hydration.rbe.json
        |
        v
actual package build with network disabled
```

The dependency cache identity is the ecosystem plus the pinned dependency-lock SHA-256. Cache location alone is never trust.

### Supported ecosystems

| Ecosystem | Required lock | Hydration contract | Offline build state |
| --- | --- | --- | --- |
| Cargo | `Cargo.lock` | `cargo fetch --locked --manifest-path <Cargo.toml>` | hydrated `CARGO_HOME`, `CARGO_NET_OFFLINE=true` |
| npm | `package-lock.json` | `npm ci --ignore-scripts --no-audit --no-fund --cache <cache>` | same cache, `npm_config_offline=true` |
| Bun | `bun.lock` | `bun install --frozen-lockfile --ignore-scripts --cache-dir <cache>` | same `BUN_INSTALL_CACHE_DIR`; RBE build network stays disabled |
| Python | `requirements.lock` | `python -m pip download --require-hashes --only-binary=:all: ...` | `PIP_NO_INDEX=1`, wheelhouse via `PIP_FIND_LINKS`, hashes required |

Default registry origins are currently:

```text
Cargo  https://index.crates.io/
       https://static.crates.io/

npm    https://registry.npmjs.org/
Bun    https://registry.npmjs.org/

Python https://pypi.org/
       https://files.pythonhosted.org/
```

An allowed registry value must be a credential-free HTTPS origin. Paths other than `/`, query strings, fragments, duplicate origins, insecure schemes, and credentials are rejected.

Default hydration bounds are currently:

```text
maximum observed bytes = 1 GiB
timeout                = 10 minutes
```

A stricter policy may be supplied by trusted orchestration.

## Hydration receipts

A successful hydration produces `hydration.rbe.json`. The receipt binds:

- format version;
- ecosystem;
- dependency-lock SHA-256;
- lock-addressed cache root;
- absolute managed executable;
- hydration arguments;
- approved network origins;
- cleared-environment state;
- shell-disabled state;
- scripts-disabled state;
- origin-restricted-network state;
- hydrated artifact count;
- observed bytes.

Receipt validation rejects a cache root that does not end in:

```text
rbe/build-deps/<ecosystem>/<lock-sha256>
```

The receipt is evidence about the performed hydration policy; it does not make unverified cache bytes trusted by path alone.

## Build transition

After hydration, `apply_offline_build_environment` refuses to modify a build invocation if it has lost any of the core isolation properties:

```text
network_allowed = false
use_shell = false
clear_environment = true
```

The build environment also receives:

```text
RBE_BUILD_NETWORK=disabled
RBE_HYDRATION_CACHE_ROOT=<lock-addressed cache>
```

Ecosystem-specific offline variables are added from the same dependency cache. Hydration is therefore not permission for the actual compilation/build step to use the network.

## Artifact acquisition

Artifact downloads use bounded staging and verification contracts rather than writing directly into the final cache. The executor plans include:

- maximum byte limits;
- connect/idle timeout policy;
- redirect limits;
- disk-space preflight;
- resumable `.part` state;
- re-hashing reusable partial prefixes;
- final pinned hash/size verification;
- atomic promotion after verification only.

System-runtime artifacts use the same general rule: the RBE cache path is not authority; the manifest identity and pinned bytes are.

## Source extraction and identity

Source preparation is traversal-safe and deterministic. The current source identity algorithm is:

```text
rbe-source-tree-sha256-v1
```

The source identity is designed to represent the extracted source tree rather than ZIP ordering, compression metadata, or unrelated archive timestamp differences.

## Relationship to the rest of the package system

- [`../project-package/README.md`](../project-package/README.md) documents project manifest/lock/cache state.
- [`../doc/library-system.md`](../doc/library-system.md) is the authoritative high-level package/library architecture.
- `install-request` parses install targets and produces trusted plans but intentionally performs no I/O itself.
- the install orchestrator owns durable session/lease/activation behavior.
- package attestation owns trust/quarantine decisions after source/build verification.

The remaining work is primarily orchestration: connect these existing contracts through the real Backend install path without bypassing the isolation and activation boundaries described here.
