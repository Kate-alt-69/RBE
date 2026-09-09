# `*.route` — Route REL

Route REL defines HTTP entrypoints. It uses the global REL grammar, but its runtime powers are intentionally route-scoped.

## Purpose

A `.route` file maps to an HTTP route and may define HTTP verb handlers such as:

```text
get
post
put
delete
patch
head
options
```

The current parser recognizes those verbs. Support for additional standardized methods such as `QUERY`, wildcard/all-method handlers, and richer route matching is planned.

## Grammar

Route REL should receive the same higher-level REL grammar as Module, Service, and Server REL:

- helper functions
- variables/constants
- objects/arrays
- expressions/operators
- conditionals
- async syntax
- classes/methods
- recursion where semantically valid
- future higher-level REL constructs

Route REL is not supposed to be a deliberately crippled grammar. Its restrictions are capability and lifecycle restrictions.

## Current structure

```text
:import[json, net]

function helper(value) {
    if (value) {
        return { ok: true };
    }
    return { ok: false };
}

class Route {
    async get(req) {
        return helper(req.path);
    }
}
```

## Imports and capabilities

Routes can import only capabilities allowed at the HTTP boundary. Privileged capabilities remain restricted even if another REL source type can use them.

Current built-in route-safe concepts include `net`, `json`, `crypto`, `time`, `http`, `request`, `log`, `security`, `response`, and read-only `private`, but several registered names still have incomplete runtime implementations. Documentation must distinguish a registered capability name from a working callable API.

Routes do not directly import `.service` sources. Service access should flow through reusable modules / controlled runtime interfaces rather than exposing service authority directly to every endpoint.

See [`../x.module/`](../x.module/) and [`../x.service/`](../x.service/).

## Request object

Current implementation is minimal: method/path exist while params/query are currently created as empty objects by the route runtime.

Target request context includes:

```text
method
path
originalUrl
params
query
headers
cookies
body
rawBody
ip
forwardedFor
protocol
host
userAgent
contentType
contentLength
```

Dynamic route segments and real request extraction are required before Route REL can replace typical production backend routing end-to-end.

## Dynamic routing target

Planned file routing includes forms such as:

```text
api/user/[uid].route       -> /api/user/:uid
api/files/[...path].route  -> wildcard path
```

Optional segments and constrained/pattern segments may be added after the basic dynamic-segment model is stable.

## Responses

Current successful route execution serializes the returned REL value as JSON, generally with `200 OK`.

Target response support includes:

- explicit status codes
- JSON/text/HTML
- response headers
- cookies and cookie clearing
- redirects
- files/downloads
- binary bodies
- streams
- SSE
- no-content responses

The `response` capability should become a real typed HTTP response API instead of a registered-but-unimplemented built-in.

## Middleware

Normal server middleware is configured through Server REL, not imported into every route as package-style middleware. Route-specific overrides/inheritance may be added through the MiddlewarePlan model.

Custom application logic that should run across routes belongs in Module REL or Service REL middleware stages once that feature lands.

See [`../server.server/`](../server.server/).

## Runtime model

Route sources are discovered and compiled during boot. The current active runtime still executes parsed/evaluated bodies rather than a final native AOT artifact. RELC's target is to link routes into the Runtime Image so normal requests do not reread route source from disk.

## Cross-file example

A route should stay thin and delegate reusable logic:

```text
:import[module&users]

class Route {
    async get(req) {
        return users.findById(req.params.id);
    }
}
```

For the imported file contract, see [`../x.module/`](../x.module/).
