# Workspace/script/archive integration phases

1. **Contract layer** — symbolic roots, workspace DAG, script runtime inference, archive operations.
2. **Language registration** — RELC/analyzer/evaluator recognize `workspace`, `script`, `archive` and reject them in `.field`.
3. **Managed runtime hydration** — RPX/install-runtime resolves/downloads/verifies/promotes `rbe.sys.*`, including PyPy.
4. **Container executor** — bind Script plans to the hardened one-shot Container worker path.
5. **Archive engine** — host-side ZIP first, then TAR/TAR.GZ/TAR.XZ; atomic local mutation and bounded extraction.
6. **Workspace executor** — allocate `??/`, run dependency-ready batches in parallel, cleanup, and return bounded results.
7. **Route-WASM warming** — cache verified archive indexes/handles when RELC can identify archive operations ahead of request execution.
8. **Server helper bridge** — connect Server REL helper calls to the same host-capability executor without expanding Server policy authority.

Every unfinished phase fails closed; no ambient host process/filesystem fallback is permitted.
