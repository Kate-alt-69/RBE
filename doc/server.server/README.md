# `server.server` — Server REL

`server.server` is the root Server REL composition and policy source. It shares common REL function/expression grammar with the other source roles, but it owns server-wide authority that Route/Module/Service REL do not receive.

## Current responsibilities

Server REL currently participates in:

- server status;
- listener host/port policy;
- request/body/time limits;
- typed Runtime ENV defaults and FORCE values;
- native middleware ordering/configuration;
- CORS/security/CSP policy inputs;
- recursion/runtime safety budgets;
- settings precedence/locks;
- embedded Route/Module/Service REL blocks;
- Server helper functions and imports used by the Server source itself.

## Root syntax

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
        json { limit 2mb; strict true; }
        compression { algorithms [br, gzip]; threshold 1kb; }
        errorHandler;
    }
}
```

Policy entries support:

```text
name;
name value;
name { ... }
force name value;
```

and nested FORCE blocks:

```text
force {
    requestTimeoutMs 30s;
    listener { port 7044; }
}
```

Structural values include strings, numbers, booleans, null, identifiers, attached-unit quantities such as `2mb`/`30s`, and arrays.

## Policy precedence — implemented

Server policy is no longer just parser metadata. RELC resolves it before Runtime Image activation.

Current precedence is:

```text
RBE built-in defaults
    < normal server.server values
    < supported settings.json overlays
    < server.server FORCE values
    < non-disableable engine safety ceilings
```

Current built-in policy defaults include an `online` server, `127.0.0.1:8080`, a 30-second request timeout, a 10 MiB body limit, and bounded recursion/invocation budgets.

Current hard ceilings include a 1 GiB request-body maximum, recursion depth 1024, repeated-symbol depth 512, and 10,000,000 operations per invocation. FORCE cannot disable those ceilings.

Not every arbitrary `settings.json` field is a ServerPolicy overlay. RELC currently maps the supported API/security policy inputs such as listener host/port, request timeout/body size, trusted proxy behavior, CORS origins, JSON payload limit, and CSP.

## Typed Runtime ENV — implemented

The `env` block contributes typed Runtime Image configuration:

```text
server Main {
    env {
        APP_NAME "RBE";
        DEBUG false;
        SESSION_TTL 86400;
    }

    force env {
        REGION "production";
    }
}
```

Runtime ENV precedence is:

```text
built-ins
    < normal server.server env defaults
    < settings.json runtimeEnv
    < forced server.server env values
```

Values remain JSON typed; they are not flattened into OS-style strings. Authorized Module/Service/Server REL reads the linked snapshot via `ENV`. Route REL is denied `ENV` by default.

Secrets do not belong here; use Vault.

## Native MiddlewarePlan — implemented

RELC lowers the declared `middleware` block to a deterministic ordered `MiddlewarePlan`. Current recognized names are:

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

The compiler rejects unknown stages, duplicate stages, and `errorHandler` when it is not the final stage. Middleware-specific validation also checks values such as byte-size limits, compression algorithms, and boolean CORS options.

### What the active runtime currently materializes

Plan lowering and full runtime behavior are separate layers. Current boot/runtime wiring directly applies plan values for:

- `json.limit`;
- request `timeout`;
- disabling CORS through `cors.enabled false`;
- API rate-limit window/request count;
- IP-ban threshold/window/duration;
- CSP policy;
- enabling the native compression layer when `compression` is present.

RBE also has an always-installed native security/request stack for request metrics, status gating, request timing, IP-ban checks, correlation IDs, tracing, global/API rate limits, body limits, security headers, CORS, and request timeout. Some recognized MiddlewarePlan stages therefore still need finer plan-controlled enable/disable/options wiring before the plan alone describes every installed HTTP layer.

## Server status — active

Supported states are:

```text
online
readonly
maintenance
draining
offline
```

The active Runtime Image status gates normal HTTP requests:

- `online` allows normal traffic;
- `readonly` allows GET/HEAD/OPTIONS and rejects mutating requests;
- `maintenance`, `draining`, and `offline` reject normal traffic with service-unavailable behavior.

Health/admin/maintenance control-plane paths remain reachable so the server can be inspected/recovered.

## Recursion policy

ServerPolicy carries runtime recursion limits used by the REL invocation tracker:

```text
recursion.maxDepth
recursion.repeatedSymbolDepth
recursion.operationBudget
```

The current defaults are bounded and the hard ceilings above remain non-disableable.

## Embedded REL files — implemented

RELC extracts embedded blocks **before** compiling the remaining Server REL source:

```text
[file-start:module.Auth]
:import[ENV]
export function appName() {
    return ENV.require("APP_NAME");
}
[file-end:module]
```

Supported source roles are Route, Module, and Service. Embedded routes can carry route metadata such as `path`.

After extraction, each block is registered with a virtual `SourceId` and compiled according to its own role. An embedded Module therefore receives Module capabilities, not Server capabilities.

Physical and embedded sources share one logical namespace. Duplicate logical identities are rejected instead of silently overriding one another.

## Server helpers

Server REL helper functions use the common REL function parser rather than a separate Server-only programming language. They remain local to the Server source; reusable application logic belongs in Module REL.

Do not infer that an arbitrary helper can mutate every resolved policy value at runtime: policy/ENV/middleware are linked during RELC compilation and the Runtime Image is immutable after activation.

## Runtime Image ownership

The resolved ServerPolicy, RuntimeEnv, MiddlewarePlan, Server executable snapshot, and embedded source metadata are stored in the Runtime Image. `settings.json` and `server.server` remain boot/deployment inputs rather than mutable per-request authority.

See [`../relc.md`](../relc.md) and [`../source-security.md`](../source-security.md).

## Remaining work

Important incomplete pieces include:

- complete plan-driven control for every recognized middleware stage;
- richer Server helper/policy evaluation where deliberately designed;
- complete hot-reload/watch orchestration around `RuntimeImageSlot`;
- persistent signed source-less Runtime Image deployment.
