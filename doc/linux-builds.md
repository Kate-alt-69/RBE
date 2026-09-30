# Linux builds and toolchain recovery

RBE performs a Linux build-host prerequisite check before dispatching into the release, selective, or SDK builders. The check exists because a host can have `rustup`, `rustc`, or `cargo` shims in `PATH` while the selected rustup toolchain itself is incomplete or corrupt.

## Preflight only

Run the prerequisite check without compiling RBE:

```bash
./build.sh --check-tools --build-linux --arch-x64
```

On Linux the preflight verifies that:

- `rustup` exists;
- the selected `rustc` actually executes;
- the selected `cargo` actually executes;
- rustup can read the selected toolchain's installed target metadata;
- a C compiler/linker driver is available as `cc`;
- full release builds also have `git` and `openssl`.

A typical Debian/Ubuntu host therefore needs:

```text
build-essential
git
openssl
rustup
```

The check prints the selected Rust toolchain plus the actual `rustc`, `cargo`, `rustup`, and compiler versions so CI/deployment logs identify the build environment precisely.

## Corrupt rustup cache recovery

A cached rustup installation can report a toolchain as installed even though the toolchain directory is incomplete. A common failure looks like:

```text
stable-x86_64-unknown-linux-gnu unchanged - (error reading rustc version)
error: missing manifest in toolchain 'stable-x86_64-unknown-linux-gnu'
help: this may happen if the toolchain installation was interrupted
```

RBE does not treat the presence of a rustup shim as proof that the toolchain is usable. The preflight executes the selected toolchain and asks rustup to read its installed-target metadata.

If that health check fails, the Linux builder automatically attempts one bounded repair before compilation:

1. identify the selected rustup toolchain installation;
2. ask rustup to uninstall the damaged toolchain;
3. if rustup itself cannot uninstall the damaged installation, remove only the validated matching toolchain directory under `RUSTUP_HOME/toolchains/`;
4. reinstall the selected toolchain with the minimal rustup profile;
5. execute the health check again;
6. fail closed if the repaired toolchain is still unusable.

The repair never treats a caller-supplied arbitrary path as the toolchain directory. The fallback directory removal is restricted to the resolved rustup home and a validated toolchain name.

## Configuration

The default build toolchain is:

```text
stable
```

Select another rustup toolchain with:

```bash
export RBE_RUST_TOOLCHAIN=1.98.1
```

or another rustup-compatible channel/toolchain identifier.

Automatic repair is enabled by default. To make an unhealthy toolchain fail immediately instead of reinstalling it:

```bash
export RBE_BUILD_AUTO_REPAIR_RUST=0
```

`false`, `no`, and `off` are also accepted disable values.

## Shared Cargo artifact cache

RBE uses one shared Cargo target directory for release, selective, SDK, Engine, and Container builds launched through `build.sh`. This is intentional because the repository contains multiple Cargo workspaces. Without a shared target directory, the Container workspace and Engine workspace can independently rebuild the same third-party crates even when the compiler inputs are identical.

The cache location is selected in this order:

1. an explicit `CARGO_TARGET_DIR` supplied by the caller;
2. `<RBE_BUILD_CACHE_DIR>/cargo-target` when `RBE_BUILD_CACHE_DIR` is set;
3. `<XDG_CACHE_HOME>/rbe-build/cargo-target` when `XDG_CACHE_HOME` is available;
4. `<RBE>/.cache/rbe-build/cargo-target` as the local fallback.

For example, to place the reusable cache somewhere explicit:

```bash
export RBE_BUILD_CACHE_DIR=/var/cache/rbe-build
./build.sh --build-linux --arch-x64
```

or provide Cargo's target directory directly:

```bash
export CARGO_TARGET_DIR=/var/cache/rbe-cargo-target
./build.sh --build-linux --arch-x64
```

RBE only chooses and shares the cache location. Cargo remains the cache-validity authority. Target triple, profile, crate features, compiler/build-script inputs, package identity, and other Cargo fingerprints still determine whether an existing artifact is reusable. RBE does not load an arbitrary cached `.rlib`, object, or binary merely because a file with the expected name exists.

A shared target directory provides two useful forms of reuse:

- **same-build reuse** — `container-bin`, `service`, `cloud_node`, `backend`, RPX, and SDK builds can reuse matching dependency artifacts rather than recompiling common crates in isolated workspace `target/` directories;
- **cross-build reuse** — deployment hosts that persist the selected cache directory can restore unchanged Cargo artifacts on the next build.

This is specifically intended to prevent expensive dependencies from being rebuilt just because RBE moved from one binary/workspace to another. If Cargo determines that the fingerprint still matches, heavy crates such as `wasmtime`, `tokio`, `serde`, `syn`, `ring`, and their transitive dependencies are reused from the shared target directory instead of starting again from `Compiling ...` for each RBE component. A changed feature set, target, compiler version, build script input, or other Cargo fingerprint can still require a rebuild, which is correct.

Render exposes a persistent build cache through `XDG_CACHE_HOME`, so RBE automatically chooses that location when it is available. The first cache-aware Render build populates the shared target directory; later deploys can reuse unchanged artifacts from that same location. A Render deploy that explicitly clears its build cache intentionally removes this acceleration state; the following build repopulates it normally.

The shared cache is an optimization only. Deleting it must never affect project/package correctness; it only makes the next compilation slower.

## Normal builds

The same preflight runs automatically for normal Linux builds, so a separate `--check-tools` invocation is optional:

```bash
./build.sh --build-linux --arch-x64
./build.sh --only-backend --build-linux --arch-x64
./build.sh --build-sdk --only-rpx --build-linux --arch-x64
```

`--check-tools` is primarily useful for CI, deployment hosts, and troubleshooting because it validates the host without spending time compiling RBE.

## Deployment-host guidance

For build-on-deploy platforms, do not rely on a restored Rust cache merely because `rustup toolchain list` names the expected channel. Run the RBE preflight before compiling, or invoke the normal `build.sh` entry point so the preflight runs automatically.

If a deployment uses a pinned RBE checkout, the deployment wrapper should run the pinned checkout's preflight before the real build. Older pinned revisions that predate this feature should perform an equivalent `rustc` + `cargo` + rustup target-metadata health check before invoking RBE.

The build should fail before compilation when a required host tool is unavailable, rather than surfacing a linker/toolchain failure deep into the build.

## CI contract

`.github/workflows/build-tools-ci.yml` validates the Linux build path whenever the shell builders or build help change. It performs shell syntax checks and executes:

```bash
./build.sh --check-tools --build-linux --arch-x64
```

This CI is intentionally lightweight; the normal RBE CI remains responsible for compiling, linting, and testing the Rust workspace.
