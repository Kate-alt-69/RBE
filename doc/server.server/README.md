# `server.server` — Server REL

`server.server` is the root Server REL composition and policy file. It uses the same global REL grammar as every other REL source type, but it owns server-only functionality.

## Server-only responsibilities

Server REL may configure or define:

- server status and lifecycle policy
- listener host/port
- request/body/time limits
- CORS
- native middleware
- compression
- security headers / CSP / HSTS
- proxy/trusted-forwarding policy
- rate limiting and IP-ban policy
- service-runtime requirements
- runtime recursion/deadlock policy
- environment defaults
- forced/locked settings
- environment/profile blocks
- embedded REL files
- server helper functions

A `.route`, `.module`, or `.service` file does not gain these server-control capabilities merely because it shares REL grammar.

## Configuration precedence

Target precedence:

```text
RBE hard safety invariants
        |
server.server FORCE declarations
        |
settings.json deployment/operator values
        |
normal server.server defaults
        |
RBE built-in defaults
```

`force` locks a server policy value against a conflicting deployment value. It cannot disable a hard engine safety invariant.

The Server REL compiler already separates normal defaults from forced values in the compiled `ServerPolicy`. Applying those maps to the typed runtime `Config` is the next boot-integration stage; until that stage is active, `settings.json` remains the source used by the running backend.

## Public runtime ENV

`settings.json` may provide shared public runtime values. Server REL may provide a value when settings did not set it.

```text
server Main {
    env {
        APP_NAME "RBE";
        REGION "local";
        SESSION_TTL 86400;
    }

    force {
        env {
            DEPLOYMENT_TIER "production";
        }
    }
}
```

Server REL ENV literals are compiled as typed JSON-compatible values rather than flattened strings. Normal values are stored separately from forced ENV values so the runtime can preserve the same precedence model as ordinary server policy.

Public ENV means readable by authorized backend REL code; it is not automatically exposed to HTTP clients.

The `ENV` runtime capability itself is not wired yet. See [`../rel.md`](../rel.md) for the target access surface and [`../compatibility.md`](../compatibility.md) for source-role capability rules.

## Native middleware

Server REL compiles native middleware declarations into an ordered policy list. The currently recognized built-ins are:

```text
correlationId
realIp
forwarded
requestTiming
requestLog
json
text
form
multipart
rawBody
cookies
cors
compression
securityHeaders
csp
hsts
rateLimit
ipBan
timeout
cache
etag
auth
errorHandler
```

The compiler rejects unknown or duplicate middleware names.

```text
server Main {
    middleware {
        correlationId;
        realIp;
        requestTiming;

        json {
            limit 2mb;
            strict true;
        }

        cors {
            enabled true;
            credentials true;
        }

        compression {
            threshold 1kb;
            algorithms [br, gzip, zstd];
        }

        securityHeaders;
        rateLimit;
    }
}
```

Middleware options are currently preserved as typed/atom values in `MiddlewarePolicy`. The runtime `MiddlewarePlan` lowering and Axum installation stage are still being implemented; declaring middleware does not yet change live request handling.

## Server status

The compiler recognizes:

```text
online
maintenance
draining
readonly
offline
```

Unknown states fail with an `SRV1xxx` compiler diagnostic. Runtime request gates and route exceptions for these states are a later Server REL runtime stage.

## General policy blocks

Normal and forced policy declarations may use nested blocks. They are compiled into deterministic dotted keys so the config merger can apply them without depending on declaration order.

```text
server Main {
    api {
        host "0.0.0.0";
        port 8080;
    }

    security {
        trustedProxyHeaders false;
    }

    force {
        api {
            requestTimeoutMs 30000;
        }
    }
}
```

This produces keys such as `api.port`, `security.trustedProxyHeaders`, and forced `api.requestTimeoutMs`.

## Server functions

Server REL may contain normal REL helper functions and classes. The policy compilation pass validates/skips their balanced bodies instead of interpreting their contents as configuration.

Both synchronous and `async function` declarations are accepted by this policy pass. Executable Server REL helper lowering belongs to the shared RELC linker/runtime stage and is not runtime-enabled yet.

A helper is not automatically global to every file. Reusable application code should live in a `.module` or an embedded Module REL file.

## Embedded literal REL files

`server.server` may contain complete literal sources.

```text
[file-start:module.Auth]
:import[ENV]

export function appName() {
    return ENV.get("APP_NAME");
}
[file-end:module]
```

Supported extraction forms are:

```text
[file-start:module.NAME]
[file-end:module]

[file-start:route.NAME path="/health"]
[file-end:route]

[file-start:service.NAME]
[file-end:service]
```

The first Server REL compiler stage now extracts these blocks before policy parsing, keeps source line positions stable for diagnostics, assigns stable virtual identities such as `server.server#module:Auth`, retains header attributes such as route `path`, and rejects nested/malformed or duplicate embedded identities.

The next RELC discovery stage must feed the extracted contents into their own Route/Module/Service compiler paths. Embedded source contents do not inherit Server REL privileges.

See [`../relc.md`](../relc.md).

## Duplicate identity

Duplicate embedded virtual identities inside one `server.server` already fail deterministically.

The full RELC discovery pass will additionally reject collisions between a physical source and an embedded source that claim the same logical identity. For example:

```text
module/Auth.module
server.server#module:Auth
```

must not silently pick one implementation.

## Diagnostics

The Server REL compiler foundation uses the `SRV1xxx` family for syntax/policy diagnostics, including invalid status, middleware, embedded source markers, duplicate policy keys, and unsupported/reserved declarations.

`profile` blocks are reserved by the grammar contract but intentionally rejected until profile selection/merging semantics are implemented.

## Current implementation status

Implemented in the current Server REL compiler foundation:

- root `server NAME { ... }` policy compilation
- typed `ServerPolicy`
- status parsing and validation
- nested normal policy defaults
- nested `force` policy values
- typed normal and forced ENV declarations
- ordered native middleware parsing and validation
- literal route/module/service extraction with stable virtual SourceIds
- embedded-header attributes such as route `path`
- duplicate embedded SourceId rejection
- synchronous and async helper/class body isolation during policy compilation
- `SRV1xxx` diagnostics for the implemented surface

Still being wired into the runtime/RELC pipeline:

- automatic root `server.server` boot discovery
- `ServerPolicy` + `settings.json` precedence application to the typed runtime config
- Runtime ENV capability and access checks
- physical-versus-embedded duplicate resolution across the complete source graph
- compiling embedded sources through the normal Route/Module/Service passes
- native `MiddlewarePlan` lowering and Axum request-stack installation
- live server-status request gates and route exceptions
- profile blocks
- executable Server REL helper linking
- full multi-pass `RuntimeImage`/transactional reload support
