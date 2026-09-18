from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# ---------------------------------------------------------------------------
# Keep user-facing compiler diagnostics tied to the actual compiler generation.
# ---------------------------------------------------------------------------
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    """/// Native lowering remains intentionally strict: one HTTP method, one return,\n/// no route helper functions. Compiler generation v7 keeps ABI v3 and permits\n/// one immutable linked Module function supplied by RELC. The linked function\n/// must itself be exactly one return of one direct HTTP, Video, Service, or\n/// Storage host call, and every Route/host argument must resolve to static JSON.\n/// Namespace imports, dynamic arguments, wider Module bodies, and nested chains\n/// remain interpreter-only.""",
    """/// Native lowering remains intentionally strict: one HTTP method, one return,\n/// no route helper functions. Compiler generation 8 keeps ABI v3 and permits\n/// one immutable linked Module function supplied by RELC. Linked host calls are\n/// normally static JSON; generation 8 additionally permits one Module-owned\n/// `storage.write` call whose `data[]` value receives `req.body` unchanged.\n/// Namespace imports, wider Module bodies, and other dynamic host arguments\n/// remain interpreter-only.""",
    "Route-WASM generation comment",
)
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    '"native Route-WASM v7 only supports one direct http.get/post/request import or one linked Module function import",',
    'format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} only supports one direct http.get/post/request import or one linked Module function import"),',
    "Route-WASM single import diagnostic",
)
replace_once(
    "engine/crates/route-engine/src/wasm_compiler.rs",
    '"native Route-WASM v7 supports at most one direct or linked host-call import",',
    'format!("native Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} supports at most one direct or linked host-call import"),',
    "Route-WASM import count diagnostic",
)

replace_once(
    "engine/crates/route-engine/src/relc.rs",
    """use crate::wasm_compiler::{compile_route_with_links, RouteWasmCompilation, RouteWasmLinkContext};""",
    """use crate::wasm_compiler::{\n    compile_route_with_links, RouteWasmCompilation, RouteWasmLinkContext,\n    ROUTE_WASM_COMPILER_VERSION,\n};""",
    "RELC compiler generation import",
)
replace_once(
    "engine/crates/route-engine/src/relc.rs",
    '"Environment Storage authority requires native Container execution; Route-WASM v7 could not lower this Route: {reason}"',
    '"Environment Storage authority requires native Container execution; Route-WASM generation {ROUTE_WASM_COMPILER_VERSION} could not lower this Route: {reason}"',
    "RELC Storage generation diagnostic",
)

# ---------------------------------------------------------------------------
# Authoritative Storage reference. Keep planned CN behavior explicitly separate
# from what main actually implements today.
# ---------------------------------------------------------------------------
storage_doc = r'''# RBE Storage

RBE currently has **two distinct storage planes** behind the Storage capability. They share capability authorization, but they do not mean the same thing and they must not be treated as interchangeable.

1. **Environment transactional storage** — namespace-scoped state owned by one Container Environment process (`read`, `list`, `snapshot`, `commit`).
2. **Project-root writes** — an explicitly authorized `storage.write` operation that writes beneath the frozen backend ProjectRoot through the symbolic `$$/` prefix.

The second plane is the unified write primitive introduced by the Storage work. It does **not** expose an arbitrary host filesystem API to REL.

## Authority

Environment Storage is **Module-owned**. Route REL cannot import Storage directly. Put Storage behind an exported Module function and import that function from the Route.

```text
// module/profile.module
:import[storage.write as writeFile]

export function save(profile) {
    return writeFile(
        encode["UTF8"],
        data[profile],
        write[$$/data/profile.json],
        level[1]
    );
}
```

```text
// api/profile.route
:import["./module/profile".save]

class Route {
    post(req) {
        return save(req.body);
    }
}
```

RELC lowers the Module owner to the exact Storage capability principal (for example `storage:profile`). Direct Route-to-Storage authority remains rejected.

## `storage.write(...)`

The public write surface is one operation with four required descriptors:

```text
storage.write(
    encode[...],
    data[...],
    write[$$/...],
    level[1 | 2 | 3]
)
```

When an exact function import is used, the local binding may be called directly:

```text
:import[storage.write as writeFile]

return writeFile(
    encode["UTF8"],
    data[{ ok: true }],
    write[$$/generated/data.json],
    level[2]
);
```

The descriptor syntax is parsed as REL syntax, but the trusted Container boundary receives one normalized descriptor object:

```json
{
  "path": "$$/generated/data.json",
  "data": { "ok": true },
  "encoding": "UTF8",
  "level": 2
}
```

Descriptor meaning:

- `encode[...]` — output encoding. Current trusted encodings are `UTF8`, `UTF16`, `UTF16LE`, `UTF16BE`, `HEX`, and `BYTES`.
- `data[...]` — value to write. Static JSON values are supported; the native linked Route/Module path also supports passing `req.body` unchanged into one Module parameter and then into `data[...]`.
- `write[$$/...]` — destination beneath the frozen backend ProjectRoot. Native Storage write lowering requires this destination to stay static and symbolic.
- `level[1..3]` — validated Data-Level metadata carried by the write request. It must be an integer `1`, `2`, or `3`.

### Encoding behavior

- `UTF8`: a string is written as its UTF-8 bytes; non-string data is JSON-serialized to UTF-8.
- `UTF16` / `UTF16LE`: text is encoded as little-endian UTF-16.
- `UTF16BE`: text is encoded as big-endian UTF-16.
- `HEX`: `data[...]` must resolve to a hexadecimal string and is decoded into bytes.
- `BYTES`: `data[...]` must resolve to an array of integers from `0` through `255`.

For UTF-16 text modes, non-string values are serialized to JSON text first.

## `$$/` — frozen ProjectRoot

`$$/` means the **persistent RBE project root from which `backend.exe` was originally started**. It is not the Container temporary directory, Environment transactional root, or a WASM-worker filesystem.

At backend boot RBE captures and canonicalizes the process working directory once, then propagates that frozen ProjectRoot through Container bootstrap into each Environment process. Later working-directory changes do not redefine `$$`.

REL and Route-WASM never receive the real host path. They carry only a symbolic value such as:

```text
$$/data/users/kate.json
```

Trusted Rust resolves that symbolic destination against the frozen ProjectRoot.

## Project-root containment

The trusted Storage boundary rejects project-write destinations that can escape or ambiguously reinterpret the root. Current checks include:

- `..`, `.` and rooted path components;
- drive/prefix switching and `:` stream/drive separators;
- backslash-based non-portable separators in the symbolic path;
- symbolic links that resolve outside the frozen ProjectRoot;
- symbolic-link final targets;
- parent components that already exist as non-directories.

Missing parent directories are created automatically. After each parent step, the resolved parent is canonicalized and checked to remain under ProjectRoot.

The final file update uses RBE's `AtomicIo::write_atomic()` rather than exposing raw write handles to REL.

## Dynamic `data[req.body]` and Route-WASM

Route-WASM compiler **generation 8** keeps capability ABI **v3** and adds one narrowly scoped dynamic path for Storage:

```text
Route req.body
      |
      v
exported Module parameter
      |
      v
data[parameter]
      |
      v
WASM guest builds the normalized storage.write JSON envelope
      |
      v
Container capability_call ABI v3
```

The WASM guest does not parse or reinterpret the request body. The HTTP edge serializes the evaluator-visible `req.body` to JSON; the guest inserts those raw JSON bytes between compiler-generated descriptor prefix/suffix bytes.

The compiler calculates the remaining body budget after the static Storage envelope. The HTTP edge rejects a body that exceeds that capability budget (or the Container execution-input ceiling) before execution.

Other dynamic linked host-capability arguments remain outside the native subset. This exception does not make arbitrary Module calls or direct Route Storage access ambient.

## Environment transactional operations

The existing Environment-owned storage surface remains separate from project-root writes:

- `read(path)`
- `list()`
- `snapshot()`
- `commit(mutations)`

These operations act on the namespace-scoped `EnvironmentStorageManager`, not on `$$/` files. The Environment process remains authoritative for that transactional state; backend code does not reopen it as a fallback.

## Data-Level status

`level[...]` currently has the following accepted values:

| Level | Intended meaning |
|---|---|
| `1` | critical / highest replication priority |
| `2` | important / normal delayed replication is acceptable |
| `3` | expendable or rebuildable data |

**Current implementation boundary:** Storage validates and carries the level through the trusted write request and response, but Cloud Node does **not yet automatically ingest `$$` project writes or schedule sync by this level**. Cloud Node currently receives objects only through its own ingest/store paths. Until the Storage-to-Cloud-Node bridge is implemented, Data-Level must not be described as active replication scheduling.

There is currently no public `durability[...]` descriptor, and `$env/` / `$tmp/` roots are not part of the locked Storage language contract.

## Failure model

Project-root `write` fails closed when:

- ProjectRoot authority was not propagated;
- the descriptor is malformed;
- the level or encoding is invalid;
- the symbolic destination is invalid or escapes containment;
- a parent directory cannot be inspected/created;
- the atomic file update fails;
- the capability payload or response exceeds protocol limits.

A Storage-backed Route that requires native Container authority is not silently downgraded into an in-process interpreter path. RELC reports a compilation/link error when the required native Storage path cannot be lowered safely.
'''
Path("doc/storage.md").write_text(storage_doc, encoding="utf-8")

replace_once(
    "doc/README.md",
    """- [`quickdb.md`](quickdb.md) — Service-only probabilistic membership/indexing through `quickDB`.\n- [`video-manager.md`](video-manager.md) — Video Manager media pipeline, hardware normalization, and live-session control.""",
    """- [`quickdb.md`](quickdb.md) — Service-only probabilistic membership/indexing through `quickDB`.\n- [`storage.md`](storage.md) — Environment Storage, unified `storage.write(...)`, frozen `$$/` ProjectRoot semantics, encoding, and Data-Level status.\n- [`video-manager.md`](video-manager.md) — Video Manager media pipeline, hardware normalization, and live-session control.""",
    "documentation Storage index",
)

replace_once(
    "doc/compatibility.md",
    """| `quickDB` | ❌ | ❌ | ✅ | ❌ |\n| Video Manager (`vm` / `video-manager`) | ❌ | ✅ | ❌ current language channel | ❌ |""",
    """| `quickDB` | ❌ | ❌ | ✅ | ❌ |\n| Environment Storage (`read/list/snapshot/commit/write`) | ❌ direct | ✅ exact Module-owned capability | ❌ current language channel | ❌ |\n| `storage.write` frozen `$$/` ProjectRoot writes | ❌ direct | ✅ | ❌ | ❌ |\n| Video Manager (`vm` / `video-manager`) | ❌ | ✅ | ❌ current language channel | ❌ |""",
    "compatibility Storage rows",
)
replace_once(
    "doc/compatibility.md",
    """### Native Route WASM\n\nNative `.route` compilation is real but intentionally small. Static one-method literal-return routes can produce deterministic WASM; dynamic routes/imports/helpers receive an explicit interpreter-fallback reason in the Runtime Image. The HTTP dispatcher still retains the linked REL evaluator path while native coverage/dispatch expands.""",
    """### Native Route WASM\n\nNative `.route` compilation is real but intentionally constrained. Compiler generation 8 keeps capability ABI v3. Static one-method literal-return routes can produce deterministic WASM, strict linked Module wrappers can lower exact HTTP/Video/Service/Storage calls, and one Module-owned `storage.write` path can receive `req.body` unchanged through `data[...]`. Other dynamic linked host arguments/import shapes receive an explicit interpreter-fallback reason in the Runtime Image. Storage-backed authority is never silently widened into direct Route access.\n\nSee [`storage.md`](storage.md) for the exact `storage.write(...)` and `$$/` contract.""",
    "compatibility native Route-WASM status",
)
