# RBE Documentation

This directory is the authoritative user-facing documentation for the current RBE/REL architecture and its planned extensions.

## Start here

- [`rbe.md`](rbe.md) — what RBE is, what it is trying to become, and when it is useful.
- [`rel.md`](rel.md) — REL (Runtime Engine Language), the language shared by `.route`, `.module`, `.service`, and `.server` sources.
- [`relc.md`](relc.md) — RELC (Runtime Engine Language Compiler), compilation, linking, dependency analysis, recursion, and Runtime Images.
- [`compatibility.md`](compatibility.md) — cross-file capability and runtime compatibility matrix.

## File-type documentation

- [`server.server/`](server.server/) — Server REL and the root `server.server` composition/policy file.
- [`x.route/`](x.route/) — Route REL (`*.route`) HTTP entrypoints.
- [`x.module/`](x.module/) — Module REL (`*.module`) reusable in-process backend logic.
- [`x.service/`](x.service/) — Service REL (`*.service`) isolated service programs.

## Documentation rules

1. **REL grammar is global.** A grammar feature such as functions, conditionals, objects, arrays, async syntax, classes, expressions, or future higher-level language constructs belongs to REL itself unless there is a strong syntactic reason otherwise.
2. **Capabilities are not global.** The same grammar does not imply the same powers. `.route`, `.module`, `.service`, and `server.server` have different capability and lifecycle surfaces.
3. **Physical and embedded files are equivalent after source discovery.** A module embedded in `server.server` is compiled as Module REL, not as Server REL.
4. **Implemented and planned behavior must be labelled separately.** Parser recognition alone is not enough to call a feature implemented.
5. **Examples cross-link instead of duplicating another file type's specification.**
6. **The old `docs/` tree is legacy documentation while it is migrated.** New REL/RELC work should update this `doc/` tree first and legacy pages should not override this architecture.

## Naming

- **RBE** — Rust Backend Engine.
- **REL** — Runtime Engine Language.
- **RELC** — Runtime Engine Language Compiler.
- **Runtime Image** — the validated, linked application image owned by the running RBE process after RELC finishes boot compilation.
