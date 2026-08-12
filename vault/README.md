# vault

Standalone crate — moved out of `engine/`'s workspace so both the main
engine (`engine/crates/core`, `engine/crates/backend`) and the
container runtime's payment environment
(`container-runtime/crates/environments`) can depend on it via
relative path. One implementation of OS-credential-store access +
AES-256-GCM encrypted-file fallback to get right, not two copies in
two separate processes to keep in sync.

Full detail in `src/lib.rs`'s doc comment (backend selection, the ACL
model, memory-dump-mitigation status) and `src/file_store.rs`'s (the
highest-risk file in this crate — hand-written crypto, flagged
explicitly as needing real review, not "compiles so it's fine").

Each caller is expected to pass its own service-name/data-dir pair so
processes get their own vault namespace by default — see
`container-runtime/README.md`'s note on what that does and doesn't
guarantee on its own.
