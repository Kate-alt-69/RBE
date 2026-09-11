# RBE Documentation

This directory is the authoritative user-facing documentation for the current RBE/REL architecture. It documents **what the engine on `main` actually does today** and labels partial/planned work instead of treating parser recognition as a finished runtime feature.

## Start here

- [`rbe.md`](rbe.md) — RBE's current architecture and why the runtime is structured this way.
- [`runtime.md`](runtime.md) — process boundaries, Runtime Image ownership, Service/Container/Vault supervision, CONTROL ER, and the Control Room.
- [`runtime-image.md`](runtime-image.md) — the Runtime Image data contract, cryptographic identity, capability binding, and atomic activation model.
- [`rel.md`](rel.md) — REL (Runtime Engine Language), shared grammar, capabilities, ENV, embedded sources, and recursion rules.
- [`relc.md`](relc.md) — the implemented RELC compilation/link pipeline and Runtime Image format.
- [`source-security.md`](source-security.md) — source integrity, Runtime ENV hardening, process/capability boundaries, and sealed deployment.
- [`compatibility.md`](compatibility.md) — current cross-file capability/runtime matrix.
- [`quickdb.md`](quickdb.md) — Service-only probabilistic membership/indexing through `quickDB`.
- [`video-manager.md`](video-manager.md) — Video Manager media pipeline, hardware normalization, and live-session control.

## File-type documentation

- [`server.server/`](server.server/) — Server REL: root composition, policy, Runtime ENV defaults/FORCE values, middleware, and embedded REL files.
- [`x.route/`](x.route/) — Route REL (`*.route`) HTTP entrypoints.
- [`x.module/`](x.module/) — Module REL (`*.module`) reusable in-process backend logic.
- [`x.service/`](x.service/) — Service REL (`*.service`) isolated managed service programs.

## Current architecture in one picture

```text
settings.json + server.server + *.route + *.module + *.service
                              |
                             RELC
                              |
                              v
                    immutable Runtime Image
                              |
               +--------------+--------------+
               |              |              |
          HTTP/Module      Service         Container
            runtime         Fabric          runtime
                              |              |
                        service process   Environment
                           workers          processes
                                             |
                                      disposable WASM worker
```

The active Runtime Image carries resolved ServerPolicy, typed Runtime ENV, middleware plan, source/symbol/dependency metadata, executable REL program snapshots, route-WASM artifacts/fallback reasons, service assignments, and capability metadata.

The current `sourceHash` and `imageId` are deterministic SHA-256 identities. `imageId` is the authority identity used when binding Container capability manifests to an exact linked application image; see [`runtime-image.md`](runtime-image.md).

## Documentation rules

1. **REL grammar is global.** Ordinary language features belong to REL rather than to one file extension.
2. **Capabilities are scoped.** Shared grammar does not mean `.route`, `.module`, `.service`, and `server.server` receive the same authority.
3. **Physical and embedded sources retain their role.** An embedded module is Module REL; an embedded route is Route REL.
4. **Implemented, partial, and planned behavior are separate.** A parsed name or lowered plan is not automatically a fully wired runtime feature.
5. **Runtime behavior wins over old design text.** If implementation and an old document disagree, update this tree to match implementation.
6. **The old `docs/` tree is legacy while migration continues.** New REL/RELC/runtime work should update this `doc/` tree first; legacy pages do not override this reference.
7. **Security boundaries must describe authority, not vibes.** Document who owns a capability, how it is authenticated, and what happens on failure/restart.

## Naming

- **RBE** — Rust Backend Engine.
- **REL** — Runtime Engine Language.
- **RELC** — Runtime Engine Language Compiler.
- **Runtime Image** — the immutable validated application snapshot produced by RELC and activated by RBE.
- **Runtime Image ID** — lowercase 64-character SHA-256 identity of the linked source/settings/compiler-ABI inputs.
- **Service Mother** — the canonical Service runtime supervisor process that owns Service REL worker creation/recovery.
- **Container Controller** — the standalone Container runtime control process that owns Environment execution infrastructure.
- **CONTROL ER** — the recovery-authority mode of the Error Reporter; it can authorize bounded recovery decisions but does not gain arbitrary process-spawn authority.
