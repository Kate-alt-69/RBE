# rbe-library-package

Shared `library.toml` parser and source-package ZIP inspector for RBE external libraries.

This crate is intentionally outside the Engine workspace so the package format can be reused by Backend install tooling, publishing/index tooling and future registry validation without coupling those components to private Engine types.

Implemented in LIB-002:

- strict TOML decoding with unknown-field rejection;
- ABI range validation;
- Rust / Bun / Node / Python / `other` runtime compatibility checks;
- SDK-family validation;
- hierarchical export/capability/dependency validation;
- structured per-OS build steps (`windows`, `linux`, `macos`, `other`) with exact-host then explicit-fallback selection;
- relative entry-path validation;
- bounded ZIP inspection;
- path traversal, absolute path, backslash ambiguity, symlink and case-collision rejection;
- bounded root `library.toml` loading and validation.

The crate only **inspects** archives. Atomic extraction/activation, runtime installation, lockfile creation and worker launch belong to later installer/runtime layers.
