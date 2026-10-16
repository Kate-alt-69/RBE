# Library Host

Status: **implemented on RBE `main`**.

Library Host is RBE's trusted boundary for invoking verified package/library work that must execute through a managed external runtime such as Bun, Node.js, Python, or PyPy. It is **not** a second package manager, not a generic subprocess API, and not permission for REL code to spawn arbitrary host programs.

## Why it exists

RELC/Library Host may need to execute code that cannot be lowered directly into the in-process REL evaluator or the native OID path. Examples include an approved package worker or `script.run*` plan that targets one of RBE's managed `rbe.sys.*` runtimes.

The backend therefore prepares a sealed execution request while Container owns the operating-system sandbox boundary:

```text
verified package / REL request
          |
          v
Backend Library Host
  - resolve admitted rbe.sys runtime
  - bind runtime SHA-256
  - inventory source files + hashes
  - clear/limit arguments and environment
          |
          v
Container IPC bootstrap
          |
          v
dep/container --library-worker-proxy
  - re-verify runtime/source bytes
  - cgroup resource limits
  - no-new-privileges
  - Landlock workspace restriction where supported
  - restricted seccomp
  - no direct network by default
  - bounded stdout/stderr
  - bounded wall time
          |
          v
managed runtime process
```

## One Container binary

Library Host does **not** ship a separate proxy sidecar executable. The trusted entrypoint is an internal mode of the already-packaged and already-attested Container binary:

```text
dep/container --library-worker-proxy
```

Private child modes such as `--library-worker-exec-child` and `--library-worker-live-child` are implementation details used when Container establishes the sandbox. They are not public application APIs.

This matters for integrity: backend has one Container SHA-256/signature binding and re-verifies the same `dep/container` bytes before Library Host execution. There is no second sidecar binary and no second integrity metadata set that can drift away from the main Container artifact.

The IPC type names still use `LibraryWorkerProxy*` for protocol compatibility. In architecture/documentation, **Container Library Host mode** is the preferred name for the execution boundary.

## Trust rules

Library Host fails closed when any required identity or sandbox primitive cannot be established. In particular:

- no PATH fallback for managed runtimes;
- no shell execution for ordinary Library Host plans;
- runtime executable identity is pinned before launch;
- source files are inventoried and hashed before launch and checked again at the Container boundary;
- direct network is denied unless a future capability explicitly authorizes it;
- cgroup enforcement is required on the supported Linux execution path;
- output and execution time are bounded;
- Container is launched from the packaged `dep/container` path and must match backend's build-time integrity binding.

`REL2216` means the trusted REL host executor itself could not be installed. Because `REL2216` is already an Error Book code, backend boot must preserve it as the primary diagnostic rather than relabeling it as generic `RBE5099`.

## Relationship to RPX

RPX determines the verified package graph/artifacts. Library Host does not change package identity or grant capabilities. It consumes an already-verified package/runtime/source plan and gives it a bounded execution boundary.

## Relationship to OIDs

OID-native Service work and Library Host are complementary:

- an operation that RELC successfully lowers to a native OID does not need Library Host just to execute that operation;
- package/library work that still requires Bun/Node/Python/etc. can use a package OID whose native fragment bridges into an approved Library Host/runtime path;
- OID allocation never grants package capability by itself;
- the OID cache remains project-local compiler cache state, while Library Host remains an execution/sandbox authority boundary.

See [`oid-compiler-cache.md`](oid-compiler-cache.md), [`library-system.md`](library-system.md), and [`library-host-web-build.md`](library-host-web-build.md).
