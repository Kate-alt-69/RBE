# FieldManager

FieldManager is RBE's typed URL/query-field layer. `.field` is a first-class REL source with normal shared REL expression grammar but a deliberately restricted capability surface.

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

For a Route such as `api/shop/item.route`, `field:awesomeness` checks `api/shop/awesomeness.field` first and then the API-root `api/awesomeness.field` fallback.

`:import[field]` enables the direct request helpers over the same snapshot:

```text
field.required("cookie")
field.optional("utm_source")
field.has("preview")
field.dynamic("utm_", true) // strip prefix
```

Required missing/invalid reusable fields fail before Route execution with HTTP 400 and a structured `field_validation_failed` response. Optional missing values resolve to their declared default or `null`; optional type failures resolve `null`. Dynamic prefix bindings resolve to an object. The resolved reusable values are also attached as `req.fields` for inspection.

Field-backed Routes deliberately remain on the linked evaluator path in FLD-002. Native Route-WASM adoption must consume the same pre-resolved Field context; it must not invent a second resolution model.

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

Inline and reusable FieldManager names share one per-Route namespace. A collision fails closed instead of silently shadowing one resolver. Field-backed Routes continue to use the linked evaluator path until native Route-WASM can consume the same pre-resolved Field context without creating a second resolution model.
