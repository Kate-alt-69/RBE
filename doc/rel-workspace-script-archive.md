# REL Workspace, Script, and Archive

This document freezes the public direction for three global REL capabilities.

## Symbolic roots

`$$/` is the frozen application ProjectRoot already owned by RBE.

`??/` is an execution-scoped temporary Workspace root owned by Backend/Container.
It must never resolve through the process current directory or an ambient OS temp
path supplied by REL.

Both roots are symbolic capability paths, not raw host filesystem paths.

## `workspace`

```rel
:import[workspace]
```

The short form creates one temporary workspace, evaluates an operation in it,
and schedules cleanup after completion:

```rel
return workspace.temp(script.run("$$/scripts/build.ts"));
```

The construction form describes a dependency graph. Independent ready steps may
run in parallel; `after` creates an explicit dependency edge.

```rel
const deploy = workspace.construct();

const repo = deploy.step("repo", workspace.fetch({
    source: "https://github.com/example/project.git",
    to: "??/repo"
}));

const package = deploy.step("package", script.run("$$/scripts/package.ts"));
deploy.after("package", "repo");

const archiveStep = deploy.step("archive", archive.create("??/repo/dist", "??/release.zip"));
deploy.after("archive", "package");

return deploy.run();
```

Network fetches are host capabilities and are not implemented by shelling out to
`git`, `curl`, or another ambient executable.

## `script`

```rel
:import[script]
```

Normal runtime selection is inferred from the file extension:

```rel
script.run("$$/scripts/job.js"); // rbe.sys.nodejs
script.run("$$/scripts/job.ts"); // rbe.sys.bunjs
script.run("??/generated/job.py"); // rbe.sys.python
```

PyPy is an explicit first-class Python runtime rather than a silent replacement
for CPython:

```rel
script.runPyPy("??/generated/heavy.py"); // rbe.sys.pypy
```

Rust remains explicit because `.rs` requires a compilation boundary:

```rel
script.runRust("$$/tools/generator.rs"); // rbe.sys.rust
```

REL never supplies an executable path. RBE resolves the runtime identity to a
verified managed runtime under `.cache/rbe/sys/<runtime>/<version>/<host>/`.
There is no PATH fallback.

The trusted executor must reuse Container's hardened no-shell worker boundary:
cleared environment, bounded stdout/stderr, timeout, denied direct networking,
verified runtime/source identities, and platform sandboxing. Until the Windows
Container executor has equivalent isolation, secure script execution on Windows
must fail closed rather than use an ambient process spawn.

## Managed runtime acquisition

The managed identities are:

- `rbe.sys.nodejs`
- `rbe.sys.bunjs`
- `rbe.sys.python`
- `rbe.sys.pypy`
- `rbe.sys.rust`

RPX/install-runtime owns discovery, download, resume, SHA-256 verification,
extraction, promotion, and runtime admission. `rbe-tools.rbe.zip` may request a
runtime but may not download/extract arbitrary executables itself.

Registry/custom-domain resolution should reuse RPX's existing trusted HTTPS
index discovery so official upstream artifacts can be mirrored without changing
REL syntax.

Production builds may prehydrate these runtimes. Missing runtimes may also be
hydrated lazily by a trusted Backend/RPX path when policy permits it.

## `archive`

```rel
:import[archive]
```

The stable surface is format-independent:

```rel
archive.list("??/release.zip");
archive.read("??/release.zip", "package.rbe.toml");
archive.extract("??/release.zip", ["dist/app.js"], "??/out");
archive.create("??/dist", "??/release.zip");
archive.replace("??/release.zip", "config.json", "??/config.json");
archive.remove("??/release.zip", "old.txt");
```

Initial trusted execution may support ZIP first. The contract reserves TAR,
TAR.GZ/TGZ and TAR.XZ/TXZ without requiring a later REL API change.

Archive entry paths must reject absolute paths, `..` traversal, NUL, and unsafe
symlink escape. Archive bytes remain host-owned. Route-WASM receives capability
operations/handles; it does not parse ZIP structures inside the guest.

When a Route references an inbound archive, RELC/Backend may warm a verified
archive index into `.cache` so request-time archive operations do not repeat
structural parsing. Cache keys must include content identity, archive format,
and the relevant archive-engine version.

## Source roles

`.module`, `.service`, `.route`, and Server REL share the common import grammar.
`workspace`, `script`, and `archive` are host capabilities and therefore remain
unavailable to pure `.field` sources.

Module-to-module linking is already supported by RELC. Route-to-route linking
is a separate same-role linking feature; adding it must not remove the existing
ability of `.service`, `.server`, or other roles to import their normal builtins,
modules, services, packages, and environment capabilities.
