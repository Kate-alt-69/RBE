# atomic-io

Standalone crate — not a member of `engine/`'s or `container-runtime/`'s
Cargo workspace. Both depend on it via relative path (`../atomic-io`
from `vault/`, `../../../atomic-io` from crates two levels deep in
either workspace). "All writes/reads happen through the atomic
writer," for the whole backend system, means one shared implementation
here, not one per process.

Two real guarantees — full detail in `src/lib.rs`'s doc comment:

- `write_atomic` — genuinely atomic full-file replace. The temp file
  is flushed before commit; Unix also fsyncs directory namespace
  changes, while Windows uses a replace-existing, write-through move.
  Rewriting an existing target therefore keeps the same atomic-replace
  contract on both platforms.
- `append_locked` / `read` — serialized via an in-process per-path
  lock. Safe against races between tasks in **this process**; not a
  claim of cross-process atomicity for appends (there's no OS
  primitive for that the way `rename` gives full-replacement).

`stats()` exposes running byte/op counts — what the container
runtime's disk-abuse detection watches.

Currently wired into: `vault`'s encrypted fallback store,
`logging`'s error-reporter (engine side), and the payment
environment's audit log (container-runtime side).
