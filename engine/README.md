# engine

The Rust backend itself — Cargo workspace, all crates, the compiled
`backend` binary. Per `rust-migration-plan.md` and the design
discussion in this repo's history.

**This folder compiles to `backend`/`backend.exe` and nothing else.**
`.route`/`.module` content and the sandboxed-container's own repo are
deliberately NOT in here — see the top-level layout below. The binary
never has route content baked into it; it discovers `/api/` and
`/module/` as sibling folders at runtime, resolved relative to
*itself* (`std::env::current_exe()`), never the current working
directory — so it finds them no matter where it's launched from.

## Top-level layout (outside this folder)

```
project-root/
├── engine/                 <- you are here — Cargo workspace, compiles to backend(.exe)
├── api/                     <- .route files — sibling of the compiled binary at runtime
├── module/                   <- .module files — sibling of the compiled binary at runtime
├── vault/                     SHARED crate — OS credential store + encrypted fallback (§8)
├── atomic-io/                  SHARED crate — the atomic global read/writer
└── container-runtime/       <- SEPARATE Cargo workspace, the sandbox container's own repo
```

`vault` and `atomic-io` are standalone crates, not members of this
workspace — both this engine AND the container runtime's payment
environment depend on them via relative path (see each crate's own
README). One implementation of "encrypt a secret" and "write a file
safely" to get right, not two copies in two separate processes to
keep in sync.

For local development, `engine/target/debug/backend(.exe)` is where
Cargo puts the binary — but at deploy time the expectation is
`backend(.exe)`, `api/`, `module/`, and `settings.json` all sitting
next to each other in one directory (copy `engine/target/release/backend`
plus `engine/settings.json` plus the top-level `api/` and `module/`
folders there together). See `api/README.md` for the full
`.route`/`.module` reference — this file covers the engine only.

## Layout (this folder)

```
engine/
├── Cargo.toml              workspace manifest
├── settings.json            engine config — see crates/config
├── crates/
│   ├── backend/               bin crate — main.rs (boot sequence, §3.2) + port_guard
│   ├── api/                   axum routes: Rust-native groups + .route engine + security layers
│   ├── core/                  AppState (config, supervisor state, rate limiters, IP strikes, vault)
│   ├── security/                CORS, headers, real-IP extraction, rate limiting, IP strikes
│   ├── config/                  settings.json loader + typed Config
│   ├── supervisor/              in-process async supervisor (§7)
│   ├── logging/                  Logger + colored terminal + global panic hook + signed error-reports log
│   └── route-engine/             .route file parser/interpreter/discovery + binary-relative path resolution
└── .github/workflows/ci.yml
```

(`vault` moved to the top-level shared location above — see that
crate's own README for why.)

## Running it

Rust toolchain required. **This has not been compiled yet** — it was
written without a Rust toolchain or network access available, so
treat it as a first draft to build and fix, not verified working code.
See the bottom of this file for exactly what to check if `cargo build`
fails, and what a clean run should look like.

```
cargo build
cargo run -p backend
```

`cargo run` puts the binary at `engine/target/debug/backend(.exe)`,
which means its sibling `api`/`module` lookup resolves to
`engine/target/debug/api` and `engine/target/debug/module` — **not**
the top-level `api/`/`module/` folders — while running via `cargo run`
during development. Two ways to actually exercise the real `/api/`
routes locally:

1. Copy (or symlink) the top-level `api/` folder into
   `engine/target/debug/api` after each build, **or**
2. Build once (`cargo build`), then copy `engine/target/debug/backend(.exe)`
   plus `engine/settings.json` next to the top-level `api/`/`module/`
   folders and run the binary directly from there — closer to the
   real deployment shape anyway.

## What's built vs. not (quick status)

See `README.md` history / `rust-migration-plan.md` for the full
picture. Short version: `.route` files work (interpreter, not
compiled — see `api/README.md`), the security layers from the 13-item
list that don't need storage are done, the vault is done (OS
credential store + encrypted-file fallback), `.module` files are
designed but not implemented, storage (`sqlx`) and the container
runtime (Phase 2) are not started.

## Testing this on Windows — what to check and how

Since none of this has been compiled yet, here's what to actually run
and what "working" looks like, roughly in the order that catches
problems earliest:

1. **Install Rust**, if not already: https://rustup.rs — grab
   `rustup-init.exe`, default options are fine. Restart your terminal
   after.

2. **Build just the config/security/route-engine crates first** —
   smallest, fastest feedback if something's wrong in the parts most
   likely to have a mistake I couldn't verify without a compiler:
   ```
   cd engine
   cargo build -p config -p security -p route-engine
   ```
   `vault` and `atomic-io` live outside this workspace now (shared
   with the container runtime — see `../vault/README.md`), so build
   those separately:
   ```
   cargo build --manifest-path ../atomic-io/Cargo.toml
   cargo build --manifest-path ../vault/Cargo.toml
   ```
   If any of these fail, the error output is the single most useful
   thing to share back — Rust's compiler errors are usually specific
   about the exact line and what's wrong.

3. **Run the unit tests** — several crates have tests specifically
   written to catch the kind of subtle mistakes hand-written,
   never-compiled code tends to have (route-engine's parser/
   interpreter, security's real-IP header parsing):
   ```
   cargo test --workspace
   ```
   Then the same for the two shared crates and the container runtime:
   ```
   cargo test --manifest-path ../atomic-io/Cargo.toml
   cargo test --manifest-path ../vault/Cargo.toml
   cargo test --manifest-path ../container-runtime/Cargo.toml --workspace
   ```
   The vault and payment-environment tests (encryption round-trip,
   tamper-detection) matter most here — those are the hand-written
   crypto files flagged as highest-risk in their own doc comments.
   Everything passing is a much stronger signal than just "it built."

4. **Build the whole thing**:
   ```
   cargo build
   ```

5. **Run it**:
   ```
   cargo run -p backend
   ```
   Expected: colored log output ending in something like
   `backend ready` with the address it's listening on
   (`0.0.0.0:8080` per the default `settings.json`). If it exits
   immediately instead, the printed error is exactly what failed in
   the boot sequence (§3.2) — config load, vault init, or the port
   bind are the likely candidates.

6. **Hit the health endpoint** (new terminal, backend still running):
   ```
   curl http://localhost:8080/health
   ```
   Expect JSON like `{"state":"running"}` (or `"ready"`).

7. **Hit the example `.route` file** — per the "Running it" section
   above, this only works if `api/` is actually next to wherever the
   binary is running from:
   ```
   curl http://localhost:8080/api/example/ping
   ```
   Expect `{"ok":true,"pong":{"ok":true},"path":"/api/example/ping"}`.

8. **Check the vault actually used Windows Credential Manager**: open
   Control Panel → Credential Manager → Windows Credentials, look for
   entries under the service name `backend-rs` after the app has run
   once. (It won't have written anything unless something calls
   `vault.credential()`/`set_credential()` — nothing does yet in
   Phase 0/1's stub routes, so this may show nothing yet; that's
   expected, not a bug, until Phase 1's storage work wires a real
   caller.)

9. **Check port reclaim**: start the backend, note it's listening,
   then kill the process *forcefully* (Task Manager → End Task, not a
   graceful Ctrl+C) so it doesn't get a chance to release the port
   cleanly, then immediately `cargo run -p backend` again. Expect a
   log line like `reclaiming port from a previous crashed run` and a
   successful second start, rather than a bind failure.

10. **Run the container-runtime wiring demo** (separate workspace, so
    a separate `cargo run`):
    ```
    cd ../container-runtime
    cargo run -p container-bin
    ```
    Expect it to print a health snapshot for all six environments
    (`general-1`..`general-5`, `payment`), all `Healthy`, then exit.
    This is *not* the real container process yet — see
    `container-runtime/README.md` for exactly what's real here and
    what's still a stub.

10. **Check `data/admin/`** appears (relative to wherever you ran the
    binary from) with `error-reports.log`, `error-reporter-status.json`,
    `error-reporter.key`, and (if the OS credential store fallback
    path triggered — shouldn't happen on Windows, DPAPI should always
    work) `vault-master.key`/`vault-store.json`.

Whatever fails first, the exact `cargo build`/`cargo test` output or
runtime log line is what I need back to actually fix it — happy to
work through errors one at a time rather than guessing at what a
compiler would have said.
