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

The resolver is compiled as REL but remains pure. `regx.test(pattern, value)` provides bounded deterministic regex matching and `regx.raw(pattern)` validates/returns a raw regex pattern.

## Current implementation boundary

FLD-001 makes `.field` a first-class RELC source, discovers physical/embedded Field sources, validates its pure import allowlist, links `field:NAME` imports, carries Field AST/IR in the immutable Runtime Image, and provides deterministic `math`/`regx` helpers.

Request-time FieldManager resolution, route-local `fields { ... }`, structured required-field HTTP 400 responses, optional `null`, and `field.NAME()` execution are the next runtime slice. They must not be documented as active before that slice lands.
