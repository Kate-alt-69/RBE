# Proposed small-backend experience

Status: design proposal from the [roadmap](../rel-roadmap.md). The goal is useful
defaults and low measured resource use for arbitrary application authors.

## Preserve inexpensive in-process composition

Current `.module` files already provide reusable in-process logic. Use them for
validation, transformations and business rules; use `.service` when independent
lifecycle/isolation or owned service state warrants it. Neither a simple query
nor sharing a function inherently requires another HTTP server.

Improve templates first. An optional one-file authoring mode could generate the
existing embedded Route/Module/Server roles rather than flatten capability rules.
This keeps a tiny application approachable without creating a second runtime.

Illustrative **proposed** convenience API, not runnable current REL:

```text
:import[http, response]

function [health(req)] {
	return response.json({ok: true});
}

http.get("/health", health);
http.listen({port: 8080, maxInFlight: 64, requestBodyLimit: "64KiB"});
```

The existing `http.get` is an outbound client helper. Reusing it for route
registration would create ambiguity: decide a separate app/router surface or
clearly scoped entrypoint before implementing this sketch. Listener ownership
belongs to Server policy; ordinary Route/Module code must not gain arbitrary
listener authority through a convenience alias. This is an API design question,
not a change to the current `http` contract.

## Validation and predictable failures

Extend [FieldManager](../field-manager.md), which already resolves typed fields
once per request and keeps pure `.field` sources deterministic. Explore reusable
object/body schemas, bounded nested collections, clear field paths, stable error
codes and optional stricter coercion.

Preserve current missing/default/invalid-optional behavior until a deliberate
versioned or opt-in strict mode changes it. Distinguish validation failures from
runtime invariants. Validation errors should explain how to fix the request while
excluding submitted secrets. Reuse one immutable parsed/resolved request snapshot
across interpreted and native execution.

## Resource-conscious building blocks

| Building block | Requirement |
| --- | --- |
| Lazy iterators | Process records without copying the entire collection at each step |
| Byte buffers and streaming | Bound retained bytes and honor backpressure; see the [async design](async-runtime.md) |
| Bounded cache | Byte/entry caps, monotonic TTLs, explicit eviction and invalidation; no unbounded default Map |
| Connection pool | Bounded open/idle/waiting connections, deadlines and shutdown; retain network capability restrictions |
| Optional SQLite | Prepared statements, bounded results, transactions and busy-timeout/cancellation; inspect existing dependencies before adding one |
| Owned jobs | Finite queues/retries and explicit shutdown; no implicit request lifetime extension |
| Optional integrations | Avoid starting unused workers or loading large modules for a health-only backend |
| Runtime inspection | Safe task/queue/allocation/timing counters with access control; no raw secret/request dumps |
| Testing helpers | Deterministic clock, HTTP/storage fakes and request fixtures using the real capability admission model |

SQLite should remain optional. A database adapter needs transaction/isolation,
blocking-work and migration semantics before selecting a dependency. It must not
pretend that probabilistic `quickDB` filters provide durable rows or authorization.

Existing storage already offers application operations. Audit generic signing and
conditional provider writes against that contract: specify create-if-absent,
revision/ETag conflict, retries, integrity and ownership before adding adapters.
Do not bake a provider name or one application's object layout into REL.

## Measurement and acceptance

Define reproducible fixtures: idle health route; validated JSON CRUD; outbound
fan-out with a concurrency limit; streaming transfer; cache under pressure; and
an optional transactional local database. Keep workloads, versions and target
hardware recorded.

Measure cold start, idle/peak RSS, allocations where available, per-request retained
bytes, throughput and p50/p95/p99 latency. Compare like-for-like workloads and
correctness. Include shutdown and overload behavior, and account for spawned
processes rather than reporting only the parent process's memory.

No fixed memory claim is established by this proposal. First capture a baseline,
then set realistic regression budgets. Avoid a mandatory heavy observability or
database dependency merely to demonstrate a lightweight application.

APP-01 covers fields; APP-02 templates/one-file admission; APP-03 cache/pool/database
feasibility; APP-04 diagnostics, tests and benchmarks. CAP-01/CAP-02 in the
[roadmap](../rel-roadmap.md) track host archive completion and storage API
investigation separately.
