# RBE quickDB

`quickDB` is the `.service` probabilistic membership/indexing capability. It sits in front of an authoritative database and answers the cheap question first: **is this value definitely absent, or might it exist?**

It is not durable storage and it must never be treated as the source of truth.

## Import

```text
:import[quickDB]
```

`quickDB` is a `.service` host capability. It is explicit rather than ambient.

## Bound class syntax

A service-local class can bind itself to a QuickDB filter with RBE bound constants:

```text
class Usernames {
    const <= set => "bloom";
    const <= capacity => 100000000;
    const <= falsePositiveRate => 0.001;
    const <= tag => "auth-usernames";
}
```

The syntax is:

```text
const <= name => literal;
```

Bound constants are read-only class metadata. Their values may be strings, numbers, booleans, null, arrays, or literal objects. Calls, identifiers, arithmetic, and other runtime expressions are intentionally rejected in bound constants.

Any number of service-local classes may exist in one `.service`. `class Service` remains reserved for lifecycle methods.

A class using `const <= set => ...;` requires `:import[quickDB]`.

## Filter kinds

Supported `set` values are:

- `"bloom"` — compact membership filter; insertion and membership checks, no deletion.
- `"countingBloom"`, `"counting-bloom"`, or `"counting"` — packed counting Bloom filter for authoritative known deletions.
- `"scalableBloom"`, `"scalable-bloom"`, or `"scalable"` — grows additional Bloom layers as the declared capacity is reached.

`capacity` is required for bound QuickDB classes. `falsePositiveRate` defaults to `0.01` when omitted.

## Automatic class methods

A QuickDB-bound class automatically exposes:

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

Counting Bloom classes additionally support the deletion operation:

```text
Usernames.removeKnown(value)
```

`removeKnown` should only be called after the authoritative database has confirmed that the value existed and the corresponding delete/update succeeded. Removing a value based only on a probabilistic membership answer can introduce false negatives.

Bound filters cannot be dropped through their class facade. Their lifetime belongs to the class declaration and service process.

## Safe startup

Bound class filters are allocated when the service process starts, but they deliberately start **unready**. This prevents an empty in-memory filter from producing unsafe negative answers before existing database rows have been loaded.

Populate the first snapshot with `load(values)`:

```text
class Service {
    start() {
        Usernames.load(allUsernamesFromDatabase);
    }
}
```

`load(values)` requires the filter to be unready, inserts the supplied values, then seals it. Use `rebuild(values)` when replacing a filter that may already be ready.

For an authoritative set that is genuinely empty, `Usernames.seal()` is the explicit opt-in that marks the empty filter ready.

Until a filter is sealed, `mightHave()` / `missing()` fail closed instead of returning an unsafe membership answer.

## Custom class methods

Classes are service-local namespaces, so they can add their own methods beside the automatic QuickDB surface:

```text
class Usernames {
    const <= set => "bloom";
    const <= capacity => 100000000;
    const <= falsePositiveRate => 0.001;
    const <= tag => "auth-usernames";

    function available(name) {
        return Usernames.missing(name);
    }

    function remember(name) {
        return Usernames.add(name);
    }
}
```

Methods can be called from exported functions, ordinary service functions, lifecycle methods, or other service-local classes:

```text
export function usernameAvailable(name) {
    return Usernames.available(name);
}
```

Class metadata is readable as a member:

```text
return Usernames.tag;
```

A custom method with the same name as an automatic QuickDB method wins, allowing a service to wrap normalization or policy around the built-in operation.

## Scoped quickDB calls inside a class

When a method belongs to a QuickDB-bound class, imported `quickDB` calls can omit the filter name because the class is the scope:

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

Outside a bound class, the explicit lower-level registry API remains available:

```text
quickDB.create("emails", {
    kind: "countingBloom",
    capacity: 100000000,
    falsePositiveRate: 0.001
});
quickDB.addMany("emails", emailsFromDatabase);
quickDB.seal("emails");
```

This lower-level form is useful for dynamic filters and explicit registry management.

## Correct membership semantics

Bloom-family filters have one critical rule:

- `missing(value) == true` means the value is definitely absent from the synchronized filter and the database lookup may be skipped.
- `mightHave(value) == true` means the real database must still be queried because false positives are possible.

For example:

```text
function usernameExists(name) {
    if (Usernames.missing(name)) {
        return false;
    }

    // Query the authoritative DB here.
}
```

quickDB never turns a `mightHave` result into database truth.

## Rebuilds

`clear()` marks a filter unready. A manual rebuild can therefore use:

```text
Usernames.clear();
Usernames.addMany(valuesFromDatabase);
Usernames.seal();
```

The shorter equivalent is:

```text
Usernames.rebuild(valuesFromDatabase);
```

Membership checks during the unready rebuild window fail closed instead of returning false negatives.

## Process lifetime

quickDB lives in the `.service` process RAM. If an on-demand or hybrid service exits and later wakes, its filters are recreated as unready. A service must repopulate or explicitly seal each bound filter before relying on negative membership answers again.

quickDB does not replace SQL, durable storage, uniqueness constraints, or transactional checks.
