# Container dependency integrity

The production backend requires a separately packaged container executable.
The container is **not** embedded inside `backend.exe`.

## Release layout

```text
dist/<target>/
├── backend.exe       # Windows example
├── settings.json
├── api/
├── module/
└── dep/
    └── container.exe
```

Linux uses `backend` and `container` instead of `.exe`.

## Build-time binding

The combined build must compile `container-bin` first and set:

```text
RBE_CONTAINER_BIN_PATH=<exact-built-container-binary>
```

When that variable is present, `engine/crates/backend/build.rs`:

1. SHA-256 hashes the exact container bytes.
2. Determines the build ID from `RBE_BUILD_ID`, or the current Git commit when unset.
3. Records the Rust target triple.
4. Loads `RBE_CONTAINER_SIGNING_PRIVATE_KEY` from the release environment.
5. Creates an Ed25519 signature over the canonical statement:

```text
RBE-CONTAINER-INTEGRITY-V1
sha256=<container-sha256>
build_id=<build-id>
target=<target-triple>
```

The backend executable contains the resulting SHA-256, build ID, target, public key, and signature as generated Rust constants.

The private signing key is never written into the repository, backend binary, container binary, `dist/`, or an integrity sidecar file.

## Runtime verification

Before starting the container process, the backend resolves:

```text
<backend-directory>/dep/container(.exe)
```

It refuses to start the backend if the dependency is missing, the SHA-256 differs, the embedded metadata signature is invalid, or the backend was compiled without a bound container signature.

The verification order is:

```text
container exists
    ↓
SHA-256 matches
    ↓
Ed25519 signature verifies
    ↓
container process starts
```

The signature covers the container hash **and** build/target identity, preventing a valid signature from one build from being reused with mismatched metadata.

## Signing key

The release environment must provide a 32-byte Ed25519 private key as 64 hexadecimal characters:

```powershell
$env:RBE_CONTAINER_SIGNING_PRIVATE_KEY = "<64-hex-characters>"
```

A release/CI secret store should provide this value. It must never be committed.

For a production release, the corresponding public key is compiled into `backend.exe` and may safely be distributed with the binary.

## Important

Changing or replacing `dep/container(.exe)` after the build causes backend startup to fail. Changing the expected hash/signature requires rebuilding `backend.exe` with the release signing key.

A simple self-hash of `backend.exe` is intentionally **not** used: such a self-reference is circular. Authenticating the backend itself requires an external code-signing mechanism in addition to this container dependency signature.

## Runtime crash supervision

The verified container dependency is now supervised continuously, not only at
scheduled refresh time. Backend polls the owned child at a bounded interval. An
unexpected exit keeps the existing `ContainerClient` generation unavailable only
until a cryptographically verified replacement becomes healthy; backend then
atomically retargets the shared client to the replacement endpoint. Rapid
failures use exponential backoff from 250 ms up to 30 seconds, and a process
that survives the stable window resets accumulated crash attempts.

Scheduled rolling refresh remains a separate path: backend asks the healthy
container to drain, starts and verifies a replacement, switches IPC, and only
then releases the old process. A planned refresh is therefore not reported as a
crash and does not inflate crash-loop attempts.

When HostBootstrap granted CONTROL ER, an unexpected container exit also emits
a bounded authenticated `ProcessExitReport` before replacement. The report
contains only the stable component/image identity, PID, exit code or Unix
signal, uptime, previous recovery attempts, endpoint generation, configured
environment count, runtime phase, supervision scope, and a fixed activity label.
The container authentication token, request payloads, execution data, Vault
credentials, Runtime ENV values, and worker memory are never included. CONTROL
ER may increase the minimum recovery delay, but it cannot exceed the local
30-second cap or permanently stop this critical process; BASIC/dead/timed-out
ER falls back to the local supervisor.

