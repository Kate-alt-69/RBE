# REL File Compatibility Matrix

This page compares functionality, not grammar. **Higher-level REL grammar is global** unless a construct is inherently meaningless for a source role.

Legend:

- ✅ supported/owned by this file type in the target architecture
- 🟡 partially implemented or planned
- ❌ intentionally not owned by this file type

| Capability / responsibility | `.route` | `.module` | `.service` | `server.server` |
|---|---:|---:|---:|---:|
| Global REL grammar | ✅ | ✅ | ✅ | ✅ |
| Helper functions | ✅ | ✅ | ✅ | ✅ |
| Conditions / expressions / structured data | ✅ | ✅ | ✅ | ✅ |
| Recursion | ✅ | ✅ | ✅ | ✅ |
| HTTP verb entrypoints | ✅ | ❌ | ❌ | ❌ |
| Reusable exports | 🟡 | ✅ | ✅ | 🟡 server-local helpers only |
| Module imports | ✅ | ✅ | ✅/policy | ✅ in server helpers / embedded files |
| Service calls | ❌ direct | ✅ | 🟡 via Service Fabric target | ✅ policy/control only |
| Shared Runtime ENV read | ❌ target default | ✅ | ✅ | ✅ |
| Server listener config | ❌ | ❌ | ❌ | ✅ |
| CORS policy | ❌ | ❌ | ❌ | ✅ |
| Native middleware ordering/config | ❌ | ❌ | ❌ | ✅ |
| Custom middleware implementation | 🟡 route-local hooks later | 🟡 planned | 🟡 planned | ✅ selects/configures pipeline |
| Force/lock settings | ❌ | ❌ | ❌ | ✅ |
| Server status (`online`, `maintenance`, etc.) | ❌ | ❌ | ❌ | ✅ |
| Service process lifecycle | ❌ | ❌ | ✅ | ✅ config/policy only |
| Process-local service memory | ❌ | ❌ | ✅ | ❌ |
| Embedded literal REL files | ❌ | ❌ | ❌ | ✅ |
| Runtime Image policy ownership | ❌ | ❌ | ❌ | ✅ root composition input |

## Grammar versus functionality

The table must never be interpreted as saying `.route` has a weaker language grammar. For example, if REL supports functions, `if/else`, recursion, arrays, objects, classes, or future pattern syntax, Route REL should be able to parse/use that grammar where semantically valid.

What `.route` cannot do is declare a server listener or force CORS policy. That is a capability boundary, not a grammar boundary.

## Shared examples

### Route -> Module

```text
// api/user/[id].route
:import[module&users]

class Route {
    async get(req) {
        return users.findById(req.params.id);
    }
}
```

See [`x.route/`](x.route/) and [`x.module/`](x.module/).

### Module -> Service

```text
// module/mail.module
:import[service:mail as mail]

export async function sendWelcome(user) {
    return mail.sendWelcome(user);
}
```

See [`x.module/`](x.module/) and [`x.service/`](x.service/).

### Server -> Embedded Module

```text
[file-start:module.Auth]
:import[ENV]
export function appName() {
    return ENV.get("APP_NAME");
}
[file-end:module]
```

The embedded source is Module REL after extraction. See [`server.server/`](server.server/) and [`relc.md`](relc.md).

## Current implementation caveat

The matrix expresses both current behavior and the architecture being implemented. Individual reference pages call out important branch-specific gaps such as minimal route request data, incomplete response built-ins, absent Server REL, and service-to-service calls that are not yet enabled on the active branch.
