# `*.route` — Route REL

Route REL defines HTTP entrypoints. It uses the common REL grammar while keeping HTTP authority narrow and explicit.

## Structure

```text
:import[json, response]

function helper(value) {
    return { ok: value != null };
}

class Route {
    async get(req) {
        return response.json(helper(req.query.value), 200);
    }
}
```

Current HTTP method parsing includes:

```text
get post put delete patch head options
```

Additional methods/wildcard handlers may be added later.

## File routing

Filesystem routing is active, including dynamic/catch-all parameters:

```text
api/users/[uid].route       -> /api/users/:uid
api/files/[...path].route   -> /api/files/*path
```

Two routes whose shapes differ only by parameter name collide at boot; `[id]` and `[slug]` are not treated as distinct URL shapes.

## Request snapshot

A declared route parameter receives the bounded request snapshot:

```text
request.method
request.path
request.originalUrl
request.params
request.query
request.headers
request.cookies
request.body
request.rawBody
request.ip
request.forwardedFor
request.protocol
request.host
request.userAgent
request.contentType
request.contentLength
```

JSON bodies are decoded to REL values. Other current v1 body types are exposed as strings. Invalid JSON returns HTTP 400 and configured body-limit violations return HTTP 413 before REL execution.

Forwarded client/protocol data is trusted only when `trustedProxyHeaders` policy is enabled. Otherwise `request.ip` is derived from the socket peer.

Current transport limitations include collapsed duplicate query keys, joined duplicate request headers, and no first-class binary-body value type.

## Responses

A plain REL return value remains a JSON `200 OK` response for compatibility. Import `response` for explicit HTTP behavior:

```text
response.json(body, status?)
response.text(text, status?)
response.html(html, status?)
response.status(status, body?)
response.noContent()
response.redirect(location, status?)
response.withHeader(responseValue, name, value)
response.cookie(responseValue, name, value, options?)
response.clearCookie(responseValue, name, options?)
```

Header/cookie values are validated again at the Rust HTTP boundary. Cookie options include `path`, `domain`, `maxAge`, `httpOnly`, `secure`, and `sameSite`.

Streaming responses, first-class binary responses, file sends, and SSE remain later transport work.

## Imports and authority

Route-safe built-in concepts include the currently registered HTTP-side capabilities such as `net`, `json`, `crypto`, `time`, `http`, `request`, `log`, `security`, `response`, and read-only `private`, subject to each capability's implemented operation set.

Important deny rules:

- Route REL cannot directly import `service:*`.
- Route REL cannot import/read shared `ENV` by default.
- Route REL cannot import `quickDB`.
- Route REL cannot directly import Video Manager (`vm` / `video-manager`).

The normal route-to-service shape is Route -> Module -> Service.

## Server middleware

Global HTTP middleware/policy is configured by `server.server`, not imported package-by-package into each route. Server REL lowers middleware into an ordered `MiddlewarePlan`; the backend combines applicable plan-driven settings with its native security stack.

See [`../server.server/`](../server.server/).

## Runtime Image execution

Route sources are discovered/parsed during boot and linked as immutable `RouteFile` program snapshots in the Runtime Image. Normal HTTP execution does not reread the `.route` source on each request.

RELC also attempts native WebAssembly lowering for every route.

### Native subset today

A Route can currently compile to native WASM when all of the following are true:

- it has exactly one HTTP method;
- it has no imports;
- it has no helper functions;
- the method body is exactly one `return` statement;
- that returned expression is a static JSON literal/value.

Example:

```text
class Route {
    get(req) {
        return { ok: true, version: 1 };
    }
}
```

The compiler emits deterministic WASM using the RBE output ABI and stores the artifact SHA-256 in the Runtime Image. Dynamic request expressions, helpers, imports, multiple methods, and other unsupported constructs are recorded with an explicit interpreter-fallback reason.

The public HTTP dispatcher still retains the immutable REL evaluator path while native artifact dispatch coverage is expanded. “WASM artifact exists” and “this request is dispatched through WASM” are therefore intentionally documented as separate things.

## Server status gate

The active Runtime Image's `ServerStatus` is checked for normal requests:

- `online` — normal traffic allowed.
- `readonly` — GET/HEAD/OPTIONS allowed; mutating requests rejected.
- `maintenance`, `draining`, `offline` — normal traffic returns service-unavailable behavior.

Health/admin/maintenance control-plane routes are exempt so operators can inspect/recover the server.

## Thin-route pattern

Keep reusable logic in modules:

```text
:import[module&users]

class Route {
    async get(req) {
        return users.findById(req.params.id);
    }
}
```

See [`../x.module/`](../x.module/).
