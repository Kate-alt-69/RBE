# RBE `quickDB`

`quickDB` is a **Service REL-only** in-memory probabilistic membership/indexing capability. It is designed to answer the cheap question before an authoritative database query:

> Is this value definitely absent, or might it exist?

It is an accelerator, not durable storage and not a source of truth.

## Import

```text
:import[quickDB]
```

RELC rejects `quickDB` from Route/Module/Server REL. It belongs to the Service process that imported it.

## Bound class syntax

A Service-local class can bind directly to a managed filter:

```text
class Usernames {
    const <= set => "bloom";
    const <= capacity => 100000000;
    const <= falsePositiveRate => 0.001;
    const <= tag => "auth-usernames";
}
```

Bound metadata uses:

```text
const <= name => literal;
```

The value must be literal metadata—strings, numbers, booleans, null, arrays, or literal objects. Runtime calls/identifiers/arithmetic are intentionally not accepted as bound configuration.

`class Service` remains reserved for Service lifecycle hooks; other Service-local classes can be QuickDB-bound namespaces.

## Filter kinds

Current `set` values include:

- `"bloom"` — compact insert/membership filter, no deletion;
- `"countingBloom"`, `"counting-bloom"`, `"counting"` — counting Bloom filter with known deletion support;
- `"scalableBloom"`, `"scalable-bloom"`, `"scalable"` — grows Bloom layers as declared capacity is reached.

`capacity` is required for bound filters. `falsePositiveRate` defaults to `0.01` when omitted.

## Automatic bound-class API

A QuickDB-bound class exposes:

```text
Usernames.add(value)
Usernames.addMany(values)
Usernames.load(values)
Usernames.rebuild(values)
Usernames.mightHave(value)
Usernames.mightContain(value)
Usernames.missing(value)
Usernames.definitelyMissing(value)
Usernames.isReady()
Usernames.clear()
Usernames.seal()
Usernames.stats()
```

Counting Bloom filters additionally support:

```text
Usernames.removeKnown(value)
```

Only use `removeKnown` after the authoritative database confirms the value existed and its delete/update succeeded. Removing from a probabilistic filter based only on a probabilistic answer can create false negatives.

## Readiness is fail-closed

Bound filters are allocated with the Service process but begin **unready**. An empty just-created filter must not tell application code that every existing database row is definitely absent.

Initial population should use:

```text
class Service {
    start() {
        Usernames.load(allUsernamesFromDatabase);
    }
}
```

`load(values)` populates an unready filter and seals it. `rebuild(values)` replaces a filter that may already be ready.

If the authoritative set really is empty, explicitly call:

```text
Usernames.seal();
```

Membership queries fail closed while the filter is unready/rebuilding.

## Correct membership semantics

This rule is the entire point of using Bloom-family structures safely:

```text
missing(value) == true
```

means the synchronized filter can say the value is definitely absent, so the database lookup may be skipped.

```text
mightHave(value) == true
```

means the authoritative database must still be checked because false positives are possible.

Example:

```text
function usernameExists(name) {
    if (Usernames.missing(name)) {
        return false;
    }

    // Query the authoritative database here.
}
```

`quickDB` never upgrades `mightHave()` into database truth.

## Custom class methods

A bound class can wrap normalization/policy around its automatic methods:

```text
class Usernames {
    const <= set => "bloom";
    const <= capacity => 100000000;

    function available(name) {
        return Usernames.missing(name);
    }

    function remember(name) {
        return Usernames.add(name);
    }
}

export function usernameAvailable(name) {
    return Usernames.available(name);
}
```

Class metadata is readable as a member. A custom method with the same name as an automatic QuickDB method overrides/wraps that class surface.

## Scoped `quickDB` calls

Inside a QuickDB-bound class, imported `quickDB` calls can target the current class filter implicitly:

```text
class Emails {
    const <= set => "countingBloom";
    const <= capacity => 100000000;

    function remember(email) {
        return quickDB.add(email);
    }

    function gone(email) {
        return quickDB.removeKnown(email);
    }
}
```

The lower-level named registry remains available when code deliberately manages a dynamic/different filter:

```text
quickDB.create("emails", {
    kind: "countingBloom",
    capacity: 100000000,
    falsePositiveRate: 0.001
});
quickDB.addMany("emails", emailsFromDatabase);
quickDB.seal("emails");
```

Outside a bound class, explicit filter names are required for registry operations.

## Rebuilds

`clear()` makes a filter unready again. A safe manual rebuild is:

```text
Usernames.clear();
Usernames.addMany(valuesFromDatabase);
Usernames.seal();
```

or simply:

```text
Usernames.rebuild(valuesFromDatabase);
```

During the unready window, membership checks fail closed instead of producing unsafe negative answers.

## Process lifetime

QuickDB state lives in Service process RAM. When an on-demand/hybrid Service process exits and later wakes, its filters are recreated unready and must be repopulated/sealed before negative answers are trusted.

That means QuickDB does **not** replace:

- SQL/another authoritative database;
- durable storage;
- database uniqueness constraints;
- transactional checks;
- durable caches that must survive Service restart.

Use it to avoid obviously unnecessary database work, not to invent facts. :D
