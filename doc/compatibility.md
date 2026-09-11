# REL File Compatibility Matrix

This page compares **current runtime authority/functionality**, not whether one file type gets a “better” programming language.

Legend:

- ✅ implemented/currently supported
- 🟡 implemented only in part, or constrained by a current compatibility/runtime limit
- ❌ intentionally unavailable to that source role

| Capability / responsibility | `.route` | `.module` | `.service` | `server.server` |
|---|---:|---:|---:|---:|
| Common REL expressions/functions | ✅ | ✅ | ✅ | ✅ helper functions |
| Role-specific class/declaration syntax | `class Route` ✅ | exports/functions ✅ | `:service` + `class Service` ✅ | `server NAME` ✅ |
| HTTP verb entrypoints | ✅ | ❌ | ❌ | ❌ |
| Reusable callable exports | helpers only | ✅ | ✅ through Service Fabric | server-local helpers |
| Module imports | ✅ | ✅ | ✅ | ✅ helpers/embedded sources |
| Direct Service imports/calls | ❌ | ✅ | ✅, acyclic only | ❌ application calls |
| Typed Runtime `ENV` read | ❌ | ✅ | ✅ | ✅ |
| Lowercase OS-process `env` | ❌ | ❌ | ❌ | ❌ |
| `quickDB` | ❌ | ❌ | ✅ | ❌ |
| Video Manager (`vm` / `video-manager`) | ❌ | ✅ | ❌ current language channel | ❌ |
| Route request snapshot | ✅ | through arguments only | through calls/events only | ❌ |
| Typed HTTP response helpers | ✅ | ❌ direct HTTP ownership | ❌ direct HTTP ownership | ❌ |
| Server listener/status policy | ❌ | ❌ | ❌ | ✅ |
| CORS/security policy ownership | ❌ | ❌ | ❌ | ✅ |
| Native middleware plan config | ❌ | ❌ | ❌ | ✅ |
| FORCE/locked server values | ❌ | ❌ | ❌ | ✅ |
| Runtime recursion budget config | ❌ | ❌ | ❌ | ✅ |
| Service process lifecycle | ❌ | ❌ | ✅ | policy/composition only |
| Process-local Service memory | ❌ | ❌ | ✅ | ❌ |
| Embedded literal REL container | ❌ | ❌ | ❌ | ✅ |
| Native Route-WASM lowering | 🟡 strict static subset | ❌ | ❌ | composition only |
| Runtime Image source/program entry | ✅ | ✅ | ✅ | ✅ root |

## Important current limits

### Route -> Service

A Route cannot import `service:*` directly. Put the Service call behind Module REL:

```text
// module/users.module
:import[service:users as users]

export async function find(id) {
    return users.find(id);
}
```

```text
// api/users/[id].route
:import[module&users]

class Route {
    async get(req) {
        return users.find(req.params.id);
    }
}
```

### Service -> Service

Service REL can import another service through the authenticated Service Mother Fabric. RELC rejects direct **synchronous dependency cycles** because the current single-request Service worker model could deadlock.

### Module cycles

RELC builds symbol-level recursive groups, but the compatibility ModuleProgram runtime validator still rejects source-level cyclic module dependency graphs. Ordinary function recursion is supported within configured runtime budgets; cyclic module imports should still be avoided for now.

### Native Route WASM

Native `.route` compilation is real but intentionally small. Static one-method literal-return routes can produce deterministic WASM; dynamic routes/imports/helpers receive an explicit interpreter-fallback reason in the Runtime Image. The HTTP dispatcher still retains the linked REL evaluator path while native coverage/dispatch expands.

### Middleware

Server REL middleware declarations are lowered and validated. Some stages directly change boot/runtime configuration today (`json`, `timeout`, `cors`, `rateLimit`, `ipBan`, `csp`, `compression`), while other recognized stages are still supplied by RBE's fixed native security stack or await dedicated plan-controlled behavior.

## Embedded files

After extraction, role rules still apply:

```text
[file-start:module.Auth]
:import[ENV]
export function appName() { return ENV.get("APP_NAME"); }
[file-end:module]
```

That block is Module REL, not Server REL with bonus powers. Physical and embedded sources share the same logical target namespace and duplicate identities are rejected.

## Capability rule

A capability must be both:

1. meaningful for the source role; and
2. explicitly granted/recognized by the compiler/runtime.

Shared syntax never becomes ambient host authority. If this matrix disagrees with a legacy page under `docs/`, this `doc/` tree is the current reference.
