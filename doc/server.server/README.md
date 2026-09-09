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

`force` is intended to lock a server policy value against a conflicting `settings.json` value. It cannot disable a hard engine safety invariant.

## Public runtime ENV

`settings.json` may provide shared public runtime values. Server REL may provide a value when settings did not set it.

Example target shape:

```text
server Main {
    env {
        APP_NAME "RBE";
        REGION "local";
        SESSION_TTL 86400;
    }
}
```

Settings values win over normal Server REL ENV defaults. Forced values may be introduced where policy requires them.

Public ENV means readable by authorized backend REL code; it is not automatically exposed to HTTP clients.

See [`../rel.md`](../rel.md) for `ENV` access and [`../compatibility.md`](../compatibility.md) for which source types may read it.

## Native middleware

Server REL is the intended place to compile native Rust middleware into a deterministic `MiddlewarePlan`.

Target built-ins include:

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

Example target syntax:

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

The exact syntax remains subject to parser implementation, but middleware should be native REL policy instead of requiring ordinary package imports for standard server behavior.

## Server status

Target states include:

```text
online
maintenance
draining
readonly
offline
```

Status policy may define route exceptions and responses, for example allowing health/admin endpoints during maintenance while rejecting normal traffic.

## Server functions

Server REL may contain normal REL helper functions. Higher-level grammar is not exclusive to Server REL; the difference is that these helpers can participate in Server REL policy/configuration evaluation.

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

Planned forms:

```text
[file-start:module.NAME]
[file-end:module]

[file-start:route.NAME path="/health"]
[file-end:route]

[file-start:service.NAME]
[file-end:service]
```

RELC extracts each block before normal source compilation. The contents are compiled by their own file-type compiler and follow exactly the same capability rules as physical files.

Embedded sources may import each other. Dependencies are resolved by SourceId/symbol identity, not by deciding that one literal block must be fully compiled first.

See [`../relc.md`](../relc.md).

## Duplicate identity

If a physical source and embedded source claim the same logical identity, RELC should fail deterministically rather than silently choose one.

Example:

```text
module/Auth.module
server.server#module:Auth
```

should produce a duplicate-source diagnostic unless a future explicit override mechanism is deliberately designed.

## Current implementation status

`server.server`, force policy, embedded files, compiled ServerPolicy, and Runtime ENV merging are planned architecture at the time this page was introduced. Existing `settings.json` remains the implemented configuration source until the Server REL work lands.
