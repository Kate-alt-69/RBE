# FieldManager

FieldManager is RBE's typed request-field layer. `.field` is a first-class REL source with normal shared REL expression grammar but a deliberately restricted capability surface.

## Imports

Route-local direct mode:

```text
:import[field]
```

Reusable sibling resolver:

```text
:import[field:awesomeness]
```

This binds the reusable `awesomeness.field` resolver to the FieldManager namespace (`field.awesomeness()` once request-time resolution is enabled).

A `.field` source may import only deterministic local helpers:

```text
:import[math, regx]
```

Field REL cannot import DB/network/service/module/filesystem/ENV/crypto/random capabilities.

## Declarative `.field`

```text
:import[math, regx]
:field[source = query, key = "awesomeness", optional = true]
resolve {
    page = optional("page", type = int, default = 1);
    debug = optional("debug", type = bool, default = false);
    tracking = dynamic("utm_", stripPrefix = true);
    cookie = required("cookie");
}
```

`required(...)`, `optional(...)`, and `dynamic(...)` compile into typed Field IR. `dynamic(prefix, stripPrefix = true)` represents a prefix family such as `utm_*`.

An empty `:field[]` uses the normal defaults (`source = query`, string type, required unless marked optional); a source must still provide a key, bindings, or an executable resolver.

## Executable deterministic resolver

```text
:import[math, regx]
field {
    source = query;
    type = string;
    required = true;
    key = "slug";
}
resolve(raw, context) {
    return math.trim(raw);
}
```

The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` keeps ordinary patterns on Rust's linear-time regex engine. `regx.test(value, descriptor)` provides readable validation descriptors (`allow`, `require`, `min`, `max`, `exclude`, `excludeContains`, `noConsecutive`, `ignoreCase`). `regx.raw(pattern)` validates advanced syntax, while `regx.raw(pattern, value)` uses a separately bounded backtracking engine for lookarounds and backreferences. `regx` is a shared REL builtin usable from `.route`, `.module`, `.service`, `server.server`, and `.field`; Field REL remains intentionally restricted to the pure `math` + `regx` capability set.

## Request-time runtime

FLD-002 resolves reusable `.field` sources exactly once from the existing immutable request query snapshot before Route execution. A reusable import is a namespace entry:

```text
:import[field:awesomeness]

class Route {
    get(req) {
        return field.awesomeness();
    }
}
```

For a Route such as `api/shop/item.route`, `field:awesomeness` resolves only the sibling `api/shop/awesomeness.field`. Nested Routes never fall back to an API-root `.field`; reusable field ownership is explicit and directory-scoped. A root Route such as `api/item.route` naturally resolves the root sibling `api/awesomeness.field`.

`:import[field]` enables the direct request helpers over the same snapshot. Direct helper keys/prefixes must be non-empty strings, matching declarative FieldManager identity rules:

```text
field.required("cookie")
field.optional("utm_source")
field.has("preview")
field.dynamic("utm_", true) // strip prefix
```

Required missing/invalid reusable fields fail before Route execution with HTTP 400 and a structured `field_validation_failed` response. Optional missing values resolve to their declared default or `null`; optional type failures resolve `null`. Dynamic prefix bindings resolve to an object. The resolved reusable values are also attached as `req.fields` for inspection. Dynamic families are fail-closed and bounded: they accept at most 64 matches by default, declarative `dynamic(...)` may set `maxMatches = N`, and `N` must be between 1 and 256. Exceeding the bound returns `FLD4004`; values are never partially truncated. Dynamic families are string maps: a matched non-string body value fails with `FLD4002` instead of leaking an untyped JSON value through the field namespace.

FieldManager keeps client validation and runtime invariants separate at the HTTP edge. `FLD4xxx` failures are client-facing validation errors and return HTTP 400 with `field_validation_failed`. `FLD5xxx` failures indicate an internal request-snapshot/compiler/runtime invariant failure; they are logged with the Route path and return HTTP 500 with `field_runtime_failed` instead of incorrectly blaming the request.

Field resolution is always host-owned and runs before Route dispatch. FLD-005 allows the native Route-WASM subset to consume that same pre-resolved context; native execution never creates a second FieldManager resolver.

## Route-local declarative fields (FLD-003)

Small endpoint-owned fields can stay directly in the `.route` file while using the same FieldManager resolver engine:

```text
:import[field]

fields {
    page = optional("page", type = int, default = 1);
    debug = optional("debug", type = bool, default = false);
    tracking = dynamic("utm_", stripPrefix = true);
    cookie = required("cookie");
}

class Route {
    get(req) {
        return {
            page: field.page(),
            tracking: field.tracking(),
            all: req.fields
        };
    }
}
```

The block is intentionally declarative: it reuses the same `required(...)`, `optional(...)`, `dynamic(...)`, coercion, default, and structured failure behavior as reusable `.field` files. It requires the direct `:import[field]` namespace import, may appear once per Route, and cannot reuse the reserved direct helper names `required`, `optional`, `has`, or `dynamic`.
Resolver options are strict and may appear at most once. `source` is available to every binding; `type` is available to required/optional bindings; `default` is optional-only; and `stripPrefix` / `maxMatches` are dynamic-only. Dynamic values have a fixed string type, so `dynamic(..., type = string)` is rejected rather than silently accepting a meaningless option. Optional defaults are type-checked at compile time after all options are parsed, so option order cannot bypass validation; a default must match the declared string/int/bool type or be `null`. Top-level `.field` metadata is strict too: each metadata key may appear once, and `required` / `optional` are mutually exclusive aliases rather than last-value-wins flags.

Inline and reusable FieldManager names share one per-Route namespace. A collision fails closed instead of silently shadowing one resolver. Native-capable field-backed Routes consume the same pre-resolved context; unsupported REL shapes continue to fall back to the linked evaluator.


## Multi-source fields (FLD-004)

FieldManager can resolve from the immutable request snapshot without re-parsing the HTTP request. Supported declarative sources are:

```text
query
body
param
header
cookie
```

A reusable `.field` chooses its default source in metadata:

```text
:field[source = header, key = "authorization", optional = true]
```

Declarative bindings inherit that source, while an individual binding may override it:

```text
:field[source = body, optional = true]
resolve {
    email = required("email");
    trace = optional("x-trace-id", source = header);
}
```

Route-local fields default to query for compatibility and can select a source per binding:

```text
:import[field]

fields {
    id = required("id", source = param);
    token = required("Authorization", source = header);
    session = optional("session", source = cookie);
    enabled = optional("enabled", source = body, type = bool, default = false);
}
```

Header lookup, including dynamic prefix matching, is ASCII case-insensitive. Named `body` bindings require a JSON object; an empty body behaves like a missing object so optional/default bindings still work. `body` integer and boolean fields may use native JSON numbers/booleans or their string forms. A `.field` with `source = body` and no key receives the complete body value in `resolve(raw, context)`, including non-object JSON or text bodies.

`type = int` is exact rather than lossy: FieldManager accepts only integers from `-9007199254740991` through `9007199254740991` (the exactly representable integer range of REL's numeric value type). Request values outside that range fail with `FLD4002`, and out-of-range integer defaults are rejected at parse time, instead of either path rounding to a different integer.

The direct helpers `field.required(...)`, `field.optional(...)`, `field.has(...)`, and `field.dynamic(...)` intentionally remain query shorthands. Use declarative bindings when selecting another source so source ownership remains visible in compiled Field IR.


## Native Route-WASM field inputs (FLD-005)

FieldManager resolution still happens exactly once on the host before dispatch. Route-WASM compiler generation 9 can now keep a field-backed Route native when the Route body is already inside the native subset and returns one of these shapes:

```text
return { ok: true };       // static output, even with fields declared
return req.fields;         // entire pre-resolved field object
return field.page();       // one pre-resolved inline/reusable field value
```

The guest receives only bounded JSON bytes selected by compiler-owned metadata and echoes them through the existing ABI. It does not parse the request again, execute `.field` code, or decide which field to read. `field.required(...)`, transforms, multi-expression Route bodies, and other unsupported dynamic shapes still use the linked evaluator.
