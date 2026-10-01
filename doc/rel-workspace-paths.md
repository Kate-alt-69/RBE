# REL symbolic workspace roots

RBE recognizes two symbolic roots for host-backed workspace capabilities:

- `$$/` — frozen ProjectRoot;
- `??/` — execution-scoped temporary Workspace root.

These spellings are part of the REL capability path language. They are not filesystem
paths exposed directly to user code. Backend/Container resolves them against trusted root
authority established at boot/execution time.

Examples:

```rel
script.run("$$/scripts/build.ts");
archive.create("??/dist", "??/release.zip");
```

Rules:

- no `..` traversal;
- no absolute path suffix;
- no NUL;
- no current-directory fallback;
- no raw OS temp-directory fallback;
- workspace cleanup must not affect ProjectRoot;
- path resolution must remain stable across Windows/Unix separators.
