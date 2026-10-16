# Archive safety rules

The REL `archive` capability is host-backed and must treat every archive as untrusted input.

Required checks before read/extract/update:

- reject absolute entry paths;
- reject `..` traversal after slash normalization;
- reject NUL;
- reject symlink/hardlink escape outside the symbolic destination root;
- bound entry count, aggregate uncompressed bytes, per-entry bytes and compression ratio;
- preserve deterministic path normalization across Windows and Unix;
- never execute archive contents merely because an entry has an executable bit;
- never resolve a symbolic `$$/` or `??/` path through ambient process cwd;
- archive mutation is local/workspace-only and must use atomic replacement when the host engine rewrites an archive.

For Route-WASM, the guest does not parse archive formats directly. Backend may warm a verified
archive index into `.cache`; the cache key must include archive content identity, format, and
archive-engine version. Guest calls operate through a host capability handle.
