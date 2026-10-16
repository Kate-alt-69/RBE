# RBE SDK package system

RBE SDK packages use a root `package.rbe.toml` and a `components/` export surface.

```text
advancenet/
├── package.rbe.toml
├── components/
│   ├── endpoint/
│   │   └── endpoint.ts
│   └── request/
│       └── request.ts
└── internal/
    └── pool.ts
```

A directory under `components/` is a package export. Internal implementation code belongs outside that directory and is not discoverable through REL imports.

```rel
:import[request from advancenet]
:import[endpoint from advancenet]
```

Example manifest:

```toml
[package]
name = "advancenet"
version = "1.0.0"
language = "typescript"
runtime = "bun"
sdk = "1"

[components]
root = "components"

[dependencies.rbe]
rbe-packager = "^1.0"
rbe-compiler-syntax = "1.5"
```

`[dependencies.rbe]` is a different namespace from npm, Cargo or Python dependencies. RBE resolves those packages itself. Transitive RBE dependencies are package-private: installing `advancenet` does not make `rbe-packager` directly importable by an application. Two root packages can therefore resolve different private versions of the same dependency without widening the application's visible package set.

## RPX

RPX is the language-neutral package executor installed beside the project-local SDK backend.

```text
rpx check
rpx compile
rpx compile .
rpx compile ./components/request
rpx compile.package
rpx compile.package .
rpx info

rpx login
rpx whoami
rpx publish .
rpx status advancenet
rpx yank advancenet 1.0.0 --reason "broken release"
rpx logout
```

`rpx check` validates the package/component graph without invoking a language compiler.

`rpx compile` validates the graph, performs real language syntax/type/compiler checks and writes a canonical package index under `.cache/rbe/build/`. `rpx compile.package` performs the same compiler checks before creating the `.rbe.zip` distributable and embedding the canonical package index.

### Registry authoring and release management

RPX uses a browser/device authorization flow for interactive publisher login. `rpx login` never asks for or stores a UAC password, UAC private ID, package owner key, or registry-internal identity. The registry returns a scoped RPX bearer credential, stored per registry under `~/.rbe/rpx/auth.json` by default. On Unix the credential file is restricted to the current user; RPX does not change permissions on an already-existing external parent directory selected through `RPX_AUTH_FILE`.

The publisher commands are:

```text
rpx login [--registry <url>]
rpx whoami [--registry <url>]
rpx publish [path] [--registry <url>] [--allow-host-toolchain]
rpx status <package> [--registry <url>]
rpx yank <package> <version> [--reason <text>] [--registry <url>]
rpx logout [--registry <url>]
```

`rpx publish` first runs the canonical package compile/archive path. The client then asks the registry for a short-lived signed upload slot, uploads the `.rbe.zip` directly to the trusted object store, and finalizes publication through the authenticated publisher endpoint. The package version comes from `package.rbe.toml` and is also sent through the frozen `?version=` upload contract; the client verifies that the final registry response identifies the exact package and version it built.

`rpx status` is public and does not require login. It reads the package status endpoint and displays latest stable release, aggregate download count, last download time, release counts, every version's active/yanked state, artifact and manifest SHA-256 identities, and complete publish/yank history. RPX validates the returned package identity, SemVer version set, hashes and release statistics before displaying them.

`rpx yank` requires the `package.yank` publisher scope. Yanking does not delete or replace an immutable release: it hides that version from new resolution while preserving its artifact, metadata, analytics and registry history. An optional reason may be attached to the yank history event.

`rpx whoami` validates the active token against the publisher service and lists packages currently owned by that publisher. `rpx logout` revokes the server-side credential when possible and removes the locally stored registry credential. Credentials supplied through `RPX_TOKEN` are never persisted; CI is expected to unset or rotate that environment value itself.

The built-in RPX/DI registry default is:

```text
https://kastrick-backend.onrender.com
```

An explicit CLI/configuration override still wins. Environment overrides are:

```text
RPX_REGISTRY_URL=https://registry.example
RBE_PACKAGE_REGISTRY=https://registry.example
RPX_AUTH_FILE=/private/path/rpx-auth.json
RPX_TOKEN=<scoped-token>
```

Production registry URLs require HTTPS. Loopback HTTP remains available for local authoring and contract tests.

### Managed compiler authority

Normal RPX compilation is fail-closed and uses the project-local managed compiler map:

```text
.rbe/rpx-toolchain.json
```

Format 2 maps canonical RBE compiler tool names to an **absolute executable/entry path plus its pinned SHA-256 identity**. Example:

```json
{
  "format": 2,
  "tools": {
    "node": {
      "path": "/opt/rbe/node/bin/node",
      "sha256": "<64-hex-sha256>"
    },
    "tsc": {
      "path": "/opt/rbe/typescript/lib/tsc.js",
      "sha256": "<64-hex-sha256>"
    }
  }
}
```

The current compiler requirements are:

| Package language | Required managed tools |
| --- | --- |
| Rust | `cargo`, `rustc` |
| JavaScript + Node | `node` |
| JavaScript + Bun | `bun` |
| TypeScript + Node | `node`, `tsc` |
| TypeScript + Bun | `bun`, `tsc` |
| Python | `python` |

Compiler plans are shell-disabled and network-disabled. Managed invocations are launched from their exact configured absolute paths with a cleared environment plus only the minimal RBE-selected environment needed for the compiler contract. Rust checks are explicitly offline and bind Cargo to the selected managed `rustc`. Managed TypeScript invokes the selected `tsc` entry through the package's declared managed Node/Bun runtime rather than relying on a launcher finding a runtime through host `PATH`.

An absolute cache path is not authority. Every managed compiler/entry file is SHA-256 pinned in the toolchain map. RPX re-hashes the primary compiler executable and every auxiliary compiler input immediately before process creation. That includes `rustc` when Cargo is the spawned process and `tsc` when Node/Bun is the spawned process. A replaced/tampered managed compiler is rejected before the primary process can execute.

A present managed toolchain file is authoritative. If it is malformed, lacks a required tool, uses a non-absolute path, carries an invalid digest, or a selected file no longer matches its pinned SHA-256, RPX fails; it does **not** fall back to a similarly named program installed on the host machine.

For deliberate local development only, an author may opt into host-installed tools:

```text
rpx compile . --allow-host-toolchain
rpx compile.package . --allow-host-toolchain
```

That flag is an explicit authoring escape hatch, not the production/default compiler-discovery path. It only applies when no managed toolchain file exists; it cannot turn a partial or tampered managed toolchain into a host fallback.

## SDK bootstrap

RBE uses one **generic rolling SDK channel**. `sdk` and `sdk.latest` both mean "install the current verified SDK development build from `main`".

The production RBE `backend` and the project-local SDK backend have separate jobs:

1. The normal production `backend` recognizes `install sdk` and `install sdk.latest`, downloads the fixed Kastrick HTTPS bootstrap script, and invokes it with argument-safe process arguments.
2. RBE's `SDK Toolchain` workflow builds the current SDK for supported platforms whenever relevant SDK/toolchain code changes on `main`.
3. After all platform bundles and their checksum files are validated, the workflow publishes a historical development prerelease and atomically replaces the deterministic moving alias `sdk-v0.0.0-dev.latest`.
4. The bootstrap resolves that exact moving alias, downloads the platform archive and `.sha256`, verifies the archive before extraction, and rejects missing/incomplete release metadata.
5. The verified SDK archive contains the dedicated SDK backend, RPX, and the Rust/JavaScript/TypeScript/Python SDK bindings.
6. That SDK backend installs the selected language binding(s) into the project under `.rbe/`, writes `sdk.lock.json`, generates the project-local activation file, and owns later SDK status/toolchain operations.

The rolling alias is intentionally a prerelease rather than a stable semantic SDK version. Historical development releases keep unique `sdk-v0.0.0-dev.<run>.<attempt>` tags for traceability. A pinned stable `sdk.<version>` distribution contract is not currently provided; requests for a specific SDK version fail closed until RBE deliberately adds a stable SDK channel.

Public install examples:

```text
backend install sdk -path=. -language=typescript
backend install sdk.latest -path=. -language=typescript
```

On Windows PowerShell, a bootstrap `backend.exe` that only exists in the current directory must initially be invoked explicitly:

```powershell
.\backend.exe install sdk -path=. -language=typescript
```

After installation, the project-local tools are under:

```text
.rbe/bin/backend[.exe]
.rbe/bin/rpx[.exe]
.rbe/sdk/<language>/
.rbe/sdk.lock.json
```

### Project-local command activation

SDK installation does **not** write to the User PATH or Machine PATH. Each project owns its own command binaries and activation file.

On PowerShell:

```powershell
& .\.rbe\activate.ps1
backend sdk status -path=.
rpx check .
```

On POSIX shells:

```sh
. ./.rbe/activate.sh
backend sdk status -path=.
rpx check .
```

Activation affects only the current shell process. The activated `backend` and `rpx` commands are bound to that project's `.rbe/bin` and fail closed when the current working directory is outside the owning project tree. This prevents one project's SDK/RPX binaries from silently becoming another project's toolchain. A fresh terminal must activate the project again.

The installed project-local SDK backend also supports:

```text
backend sdk status -path=.
backend sdk repair -path=.
backend sdk update -path=.
```

`repair` and `update` deliberately re-enter the verified bootstrap path rather than keeping a second complete SDK payload inside the project-local backend.

A `global` SDK install explicitly installs all language bindings; mixed-language packages are only valid when `language = "global"` is deliberately selected in `package.rbe.toml`.

### Verified compiler handoff

The SDK backend never discovers compilers through host `PATH` and it does not pretend that merely finding a file under `.cache/rbe/sys` makes that file trusted. The official SDK installer, or the trusted system-runtime orchestrator once that execution path is connected, supplies a verified RPX format-2 toolchain descriptor.

A fresh SDK install may receive that handoff directly:

```text
backend install sdk.latest \
  -path=. \
  -language=typescript \
  -toolchain=/trusted/staging/rpx-toolchain.json
```

An already installed SDK can admit a freshly verified handoff without replacing the SDK bundle:

```text
backend sdk toolchain \
  -path=. \
  -file=/trusted/staging/rpx-toolchain.json
```

Before the descriptor is copied into `.rbe/rpx-toolchain.json`, `sdk-backend` parses the strict RPX schema and verifies every absolute compiler/entry file against its pinned SHA-256. A failed handoff does not replace the existing project toolchain. Reinstalling the SDK without a new handoff preserves an existing managed toolchain only after re-verifying every pin.

`sdk.lock.json` records whether the SDK installation has admitted a managed toolchain. `backend sdk status` re-verifies the managed compiler identities when that state is present. Older installs without a managed map remain readable and report `NOT CONFIGURED`; RPX compilation remains fail-closed unless an author explicitly uses `--allow-host-toolchain` for local development.

This handoff is deliberately separate from system-runtime download/hydration. The trusted installer/orchestrator owns acquiring and admitting `rbe.sys.*` toolchains; the project-local SDK backend owns validating and installing the resulting compiler map. That keeps unfinished system-runtime orchestration from turning into an implicit cache/PATH trust fallback.
