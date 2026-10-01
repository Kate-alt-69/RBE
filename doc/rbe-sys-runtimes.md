# RBE managed system runtimes

RBE-managed language tools are addressed by identity rather than by ambient executable path.

Current/target identities used by package build and REL script execution:

| Identity | Purpose | Default script extensions |
| --- | --- | --- |
| `rbe.sys.nodejs` | Node.js runtime | `.js`, `.mjs`, `.cjs` |
| `rbe.sys.bunjs` | Bun runtime | `.ts`, `.mts`, `.cts` |
| `rbe.sys.python` | CPython runtime | `.py` |
| `rbe.sys.pypy` | PyPy runtime | explicit Python/JIT selection |
| `rbe.sys.rust` | Rust authoring/compiler toolchain | explicit Rust build/script selection |

Managed runtimes live under a versioned, host-specific cache:

```text
.cache/rbe/sys/<runtime>/<version>/<host>/
```

The cache entry must be backed by a registry/runtime manifest that binds at least:

- runtime identity;
- version;
- host triple/platform;
- HTTPS artifact URL;
- SHA-256;
- size limit;
- archive kind;
- admitted executable/entrypoint.

RPX/install-runtime owns resolution, download/resume, verification, extraction and promotion.
REL and package code do not choose executable paths and do not fall back to PATH.

## PyPy

PyPy is first-class but not the implicit replacement for CPython. Long-running pure-Python
workloads may benefit from the JIT, while startup-heavy or C-extension-heavy workloads may not.
Callers select it explicitly through `script.runPyPy(...)` or a future runtime option.

## Production hydration

A production build may prehydrate required runtimes into its deployment artifact/cache. A trusted
runtime path may lazily hydrate a missing runtime when policy allows it. Both paths use the same
RPX/install-runtime resolver and verifier so custom registries/mirrors do not create a second
security model.
