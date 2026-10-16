# RBE SDK local authoring

The RBE SDK is project-local. Installing an SDK creates the following project-owned files:

```text
.rbe/
├── bin/
│   ├── backend[.exe]
│   └── rpx[.exe]
├── sdk/
│   └── <language>/
├── activate.ps1 | activate.sh
└── sdk.lock.json
```

RBE never writes the SDK tools into the User or Machine `PATH`.

## Activate `backend` and `rpx` in the current shell

A child executable cannot mutate the environment of its parent shell. Therefore `backend install sdk.latest ...` can create the activation file, but it cannot make bare `backend` / `rpx` commands appear in the PowerShell process that launched it.

On Windows PowerShell, run once per shell:

```powershell
& .\.rbe\activate.ps1
```

On Linux/macOS shells:

```sh
. ./.rbe/activate.sh
```

The activation is process-local. It does not edit persistent PATH state. The generated wrappers also verify that the current directory is the owning project (or one of its children); leaving the project tree makes the project-local `backend` / `rpx` commands fail closed.

`backend sdk status -path=.` reports whether the activation file exists and whether the current shell is active.

## Rust compiler authority

`rpx compile` is fail-closed by default. A normal SDK install does not silently trust `cargo`, `rustc`, Node, Bun, Python, or any other compiler found through host `PATH`.

The long-term production path is RBE-managed `rbe.sys.*` runtime hydration. Until the Rust system-runtime endpoint is connected, local Rust authors have two explicit choices.

### One-shot host-authoring escape hatch

```powershell
rpx compile . --allow-host-toolchain
rpx compile.package . --allow-host-toolchain
```

This does not create a managed toolchain file. It is deliberately per-invocation.

### Pin the local Rust toolchain into the project

For repeated local authoring, explicitly admit the current rustup toolchain:

```powershell
backend sdk toolchain host -path=. -language=rust
```

The SDK backend asks `rustup` for the concrete `cargo` and `rustc` binaries, resolves their absolute paths, SHA-256 hashes the exact files, verifies the resulting descriptor, and atomically writes:

```text
.rbe/rpx-toolchain.json
```

After that, normal commands use the pinned managed map:

```powershell
rpx compile .
rpx compile.package .
```

RPX re-hashes the selected compiler files immediately before execution. If either file changes, compilation fails instead of silently accepting the replacement.

`backend sdk toolchain host` is an explicit local-authoring bridge, not production runtime hydration. It currently supports Rust. Other languages should use a trusted `-file=<verified-rpx-toolchain.json>` handoff until their managed system-runtime hydration is connected.

## Example: `mail`

```powershell
# SDK is already installed in this project.
& .\.rbe\activate.ps1

backend sdk status -path=.
backend sdk toolchain host -path=. -language=rust

rpx check .
rpx compile .
rpx compile.package .
```

If a package build helper deliberately chooses host authoring instead, it should pass `--allow-host-toolchain` explicitly. Package scripts must not silently weaken RPX compiler authority.