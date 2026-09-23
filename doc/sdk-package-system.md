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

`rpx compile` validates the package/component graph and writes a canonical package index under `.cache/rbe/build/`. Language syntax/type compilation is delegated to the installed language SDK compiler. `rpx compile.package` creates the `.rbe.zip` distributable and embeds the canonical package index.

## SDK bootstrap

The SDK-specific backend is intentionally separate from the production backend. It installs only into a project:

```text
backend install sdk.latest -path=. -language=typescript
backend sdk repair -path=.
backend sdk update -path=.
backend sdk status -path=.
```

A `global` SDK install explicitly installs all language bindings; mixed-language packages are only valid when `language = "global"` is deliberately selected in `package.rbe.toml`.
