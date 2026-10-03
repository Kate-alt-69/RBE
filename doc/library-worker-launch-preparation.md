# Verified Library Worker Launch Preparation

Status: **implemented through `LIBHOST-047`** on RBE `main`.

This document records the trusted preparation boundary that turns one verified RPX package worker snapshot into a sealed interpreted-worker invocation suitable for the Container Library Worker Proxy. It complements [`library-system.md`](library-system.md) and [`library-host-web-build.md`](library-host-web-build.md).

## Why this boundary exists

Backend must not recreate package-worker trust from a package name, host `PATH`, a mutable cache path, or whatever interpreter happens to be installed on the machine.

The preparation chain is instead:

```text
verified RPX root snapshot
        |
        v
verified package artifact + runtime.entry
        |
        v
fresh materialized worker source tree
        |
        +---- every file gets size + SHA-256 identity
        |
        v
exact admitted rbe.sys.* runtime
        |
        +---- admission record + manifest must agree
        +---- executable is SHA-256 re-verified
        +---- version must equal the worker snapshot
        |
        v
PinnedManagedToolchain
        |
        v
WorkerLaunchPlan
        |
        +---- empty environment
        +---- no shell
        +---- direct networking disabled
        +---- bounded arguments / startup timeout
        |
        v
VerifiedWorkerInvocation
        |
        v
Container Library Worker Proxy bootstrap
```

`prepare_verified_library_worker_launch()` in `rbe-install-runtime` owns this join. Backend should consume the resulting proof instead of repeating archive extraction, system-runtime lookup, or executable hashing itself.

## Supported interpreted worker lanes

`LIBHOST-047` admits the following managed worker kinds:

| package runtime kind | RBE managed system runtime |
| --- | --- |
| `bun` | `rbe.sys.bunjs` |
| `node` | `rbe.sys.nodejs` |
| `python` | `rbe.sys.python` |
| `pypy` | `rbe.sys.pypy` |

The package worker must declare `runtime.managed = true`.

RBE does **not** fall back to host `PATH` for an unmanaged or missing interpreter. The exact runtime must already be admitted through the trusted `rbe.sys.*` hydration path.

### Rust packages

`rust` deliberately fails closed at this interpreted-worker preparation boundary.

A Rust source entry is not an executable worker merely because `rustc` or Cargo exists. RBE still needs a separate verified compiled-worker output contract that binds the final native worker artifact to its source/build/toolchain identity before a Rust Library Host process can be launched.

Until that contract exists, `LIBHOST-047` returns `CompiledWorkerRequired` rather than pretending a `.rs` source file is executable.

## Exact runtime version binding

The worker snapshot already contains the runtime version resolved by the package lock. `LIBHOST-047` loads the corresponding admitted system runtime and requires:

```text
admitted.version == verified_worker.runtime_version
```

A compatible-but-different version is not silently substituted at launch time.

This is intentional. Package resolution chooses the runtime version; worker launch proves that the exact resolved version is the one whose bytes RBE admitted.

## Fresh worker source materialization

The worker source is materialized through `prepare_verified_worker_source()` from the SHA-pinned `.rbe` artifact.

The existing materializer:

- re-hashes the package artifact;
- re-inspects package metadata;
- checks package/version/runtime/entrypoint identity against the verified snapshot;
- requires a fresh absolute destination root;
- rejects traversal and symlink-based extraction;
- writes files with create-new semantics;
- records SHA-256 and size for every materialized file;
- verifies `runtime.entry` is a regular non-symlink file.

`WorkerLaunchPlan::verify_before_spawn()` then rechecks the managed interpreter and the complete materialized source tree again before it emits `VerifiedWorkerInvocation`.

If launch preparation fails after a fresh source tree was materialized, install-runtime removes that fresh tree instead of leaving it behind as apparent trusted state.

## Sealed proof handoff

`PreparedLibraryWorkerLaunch` retains both:

- `MaterializedWorkerSource`, whose directory must remain alive while the worker uses it;
- opaque `VerifiedWorkerInvocation`, whose fields cannot be freely constructed or mutated by Backend code.

`rbe-install-runtime` also re-exports `library_worker_proxy_bootstrap()`. That function converts the sealed invocation into the strict Backend-to-Container proxy bootstrap without widening its authority.

The Container side still independently verifies the executable/source identities before sandboxed execution. A Backend-side proof is therefore not permanent trust in a path.

## What LIBHOST-047 does not claim

This commit is **launch preparation**, not complete end-to-end package execution.

The secure live Container proxy already exists and establishes the Linux sandbox boundary, but Backend still owns the next connection step:

```text
PreparedLibraryWorkerLaunch
        |
        v
verified packaged container-library-worker-proxy --live
        |
        v
library.proxy.ready
        |
        v
LibrarySessionBinding::accept_wire_handshake()
        |
        v
library.hello -> library.accept
        |
        v
PackageExportCaller request/reply loop
```

Until that owner-side bridge is connected, RBE documentation must not claim that every linked package export is already executed end-to-end by a live external worker.

## Security invariants

The following rules are intentional and should be preserved by later Library Host work:

1. package names and mutable cache paths are not execution authority;
2. host `PATH` is never a package-worker fallback;
3. the resolved runtime version must match the admitted runtime version exactly;
4. package source and interpreter bytes are re-verified immediately before the Container handoff;
5. worker launch remains shell-free and begins from an empty environment;
6. direct package networking stays disabled; network access must flow through admitted Library Host capabilities;
7. the Container sandbox, not a boolean in a launch plan, is the OS enforcement boundary;
8. `library.proxy.ready` means only that the sandbox boundary is ready; it does not replace `library.hello` identity/ABI validation;
9. stdout remains Library Protocol-owned after live startup; diagnostics belong on stderr;
10. unsupported execution lanes fail closed rather than silently weakening the model.

## Next RBE work

The next Library Host slice is the Backend owner-side bridge that keeps the prepared source tree alive, starts the build-bound signed live proxy, consumes `library.proxy.ready`, completes the retained `LibrarySessionBinding` handshake, services authorized `host.call` messages, and implements Route Engine's `PackageExportCaller` over the accepted worker channel.
