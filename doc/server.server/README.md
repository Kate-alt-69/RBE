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

## Implemented structural syntax

The first `server.server` parser/compiler front-end is implemented. The root shape is:

```text
:import[json]

server Main {
    status online;

    listener {
        host "0.0.0.0";
        port 7044;
    }

    env {
        APP_NAME "RBE";
        REGION local;
        SESSION_TTL 86400;
    }

    middleware {
        correlationId;
        realIp;

        json {
            limit 2mb;
            strict true;
        }
    }

    function helper(value) {
        return value;
    }
}
```

Server policy entries currently have four structural forms:

```text
name;
name value;
name { ... }
force name value;
```

A FORCE block is also accepted:

```text
force {
    timeout 30s;
    listener {
        port 7044;
    }
}
```

FORCE propagates into nested entries in that block. This records lock intent only; final FORCE precedence against `settings.json` belongs to the later `ServerPolicy` resolution pass.

Supported structural values are:

```text
"string"
123
true
false
null
identifier
2mb
30s
[br, gzip, zstd]
```

Quantity units must be attached to their number (`2mb`, not `2 mb`). Unit interpretation and per-setting range validation happen in later typed policy lowering.

The parser does not duplicate common REL function/import grammar. Leading `:import[...]` declarations and Server REL helper functions are parsed through the shared REL parser, so adding grammar to the common function/import grammar does not require a Server-only copy.

Current semantic checks already include:

- only one root `server NAME { ... }` declaration per compiled source
- duplicate Server helper function rejection
- known server status validation (`online`, `maintenance`, `draining`, `readonly`, `offline`)
- `listener`, `env`, and `middleware` block-shape validation
- duplicate Runtime ENV default rejection inside one `env` block
- Runtime ENV defaults must have values rather than bare flags/nested blocks

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

The parser now records whether a setting is forced. The precedence algorithm itself is intentionally not implemented in the parser; it belongs to the later typed `ServerPolicy` resolution stage.

## Public runtime ENV

`settings.json` may provide shared public runtime values. Server REL may provide a value when settings did not set it.

```text
server Main {
    env {
        APP_NAME "RBE";
        REGION local;
        SESSION_TTL 86400;
    }
}
```

Settings values win over normal Server REL ENV defaults. Forced values may be introduced where policy requires them.

Public ENV means readable by authorized backend REL code; it is not automatically exposed to HTTP clients.

The structural `env` block is now parsed and validated. Runtime ENV construction/merging is the next RELC stage.

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

The structural middleware syntax is now implemented:

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

This front-end preserves middleware declaration order. Lowering these declarations to native Rust middleware and validating middleware-specific options belongs to the later `MiddlewarePlan` pass.

## Server status

Implemented states are:

```text
online
maintenance
draining
readonly
offline
```

Status policy may later define route exceptions and responses, for example allowing health/admin endpoints during maintenance while rejecting normal traffic.

## Server functions

Server REL may contain normal REL helper functions. Higher-level grammar is not exclusive to Server REL; the difference is that these helpers can participate in Server REL policy/configuration evaluation.

A helper is not automatically global to every file. Reusable application code should live in a `.module` or an embedded Module REL file.

The current Server REL front-end deliberately delegates helper-function parsing to the same parser used for common REL functions instead of maintaining a separate Server-only function grammar.

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

RELC will extract each block **before** normal Server REL parsing. The contents are then registered in `RelSourceRegistry` and compiled by their own source-role path with the same capability rules as physical files.

Embedded extraction is not implemented yet, so literal blocks are not currently accepted by `compile_server_source`. This is deliberate ordering rather than a separate grammar restriction.

Embedded sources may import each other. Dependencies are resolved by SourceId/symbol identity, not by deciding that one literal block must be fully compiled first.

See [`../relc.md`](../relc.md).

## Duplicate identity

If a physical source and embedded source claim the same logical identity, RELC now rejects the collision in `RelSourceRegistry` rather than silently choosing one.

Example:

```text
module/Auth.module
server:Main#module:Auth
```

Both identify the logical Module REL target `Auth`, so they cannot coexist without a future explicit override mechanism.

## Current implementation status

Implemented now:

- RELC source identity/registry foundation
- root `server NAME { ... }` structural parser
- shared REL import parsing for Server REL
- shared REL helper-function parsing for Server REL
- nested policy/config blocks
- flags, scalar values, quantities, arrays
- FORCE intent recording and FORCE blocks
- server-status validation
- structural `listener`, `env`, and `middleware` validation

Still subsequent stages:

- Runtime ENV construction and merge rules
- embedded `[file-start:*]` extraction
- typed `ServerPolicy`
- FORCE precedence resolution against `settings.json`
- native `MiddlewarePlan` lowering
- Runtime Image linking/reload

Existing `settings.json` remains the active runtime configuration source until those later Server REL stages are connected to boot/runtime activation.
