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
```

`rpx check` validates the package/component graph without invoking a language compiler.

`rpx compile` validates the graph, performs real language syntax/type/compiler checks and writes a canonical package index under `.cache/rbe/build/`. `rpx compile.package` performs the same compiler checks before creating the `.rbe.zip` distributable and embedding the canonical package index.

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

An absolute cache path is not authority. Every managed compiler/entry file is SHA-256 pinned in the toolchain map and RPX re-hashes the selected file immediately before process creation. A replaced/tampered managed compiler is rejected before it can execute.

A present managed toolchain file is authoritative. If it is malformed, lacks a required tool, uses a non-absolute path, carries an invalid digest, or the selected file no longer matches its pinned SHA-256, RPX fails; it does **not** fall back to a similarly named program installed on the host machine.

For deliberate local development only, an author may opt into host-installed tools:

```text
rpx compile . --allow-host-toolchain
rpx compile.package . --allow-host-toolchain
```

That flag is an explicit authoring escape hatch, not the production/default compiler-discovery path. It only applies when no managed toolchain file exists; it cannot turn a partial or tampered managed toolchain into a host fallback.

## SDK bootstrap

The SDK-specific backend is intentionally separate from the production backend. It installs only into a project:

```text
backend install sdk.latest -path=. -language=typescript
backend sdk repair -path=.
backend sdk update -path=.
backend sdk status -path=.
```

A `global` SDK install explicitly installs all language bindings; mixed-language packages are only valid when `language = "global"` is deliberately selected in `package.rbe.toml`.

The SDK bootstrap/install path is responsible for supplying project-local SDK bindings and, as the managed compiler integration is completed, the verified compiler/tool paths and SHA-256 identities consumed by RPX. RPX itself does not silently discover or trust arbitrary host compilers in its default mode.
