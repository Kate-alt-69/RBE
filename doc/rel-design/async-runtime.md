# Proposed async model and resource ownership

Status: design proposal from the [roadmap](../rel-roadmap.md). Current REL accepts
async declarations and has asynchronous host/service paths. That is a foundation,
not a claim that Promise-style task values, async iterators, cancellation groups
or streams described here are already implemented.

## User-facing model

Authors should be able to write `async function` or `async funct`, then use an
explicit wait operation for I/O. Exact `await` grammar and task-value semantics
need to be specified across all evaluators. Keep request operations named around
their purpose: proposed `request.id()` and `request.deadline()` are clearer than
a general `context.run`. Existing request helpers and role authority still apply;
the module name must not silently gain access to an ambient request.

Names such as `task.group`, `task.all`, `task.allSettled`, `task.map`,
`request.scope`, and `channel` are provisional API candidates, not available calls.
Only the two async function declaration spellings are a user-set naming direction.

## Owned task contracts

| Primitive | Proposed contract | Why it matters |
| --- | --- | --- |
| Request scope | Own child tasks, deadline and cancellation; propagate only permitted request metadata | A disconnected user should not leave invisible work running |
| Task group | Join children before the owner exits; cancel siblings on fail-fast error, then drain cleanup | Failure must not leak work |
| `all` / `allSettled` | Preserve input ordering; bounded admitted tasks and retained result bytes | Convenient parallelism must not exhaust memory |
| Limited mapping | Accept an iterator and explicit concurrency; bound queued items and results | Useful for paged APIs and bulk operations |
| Deadline | One monotonic deadline across queuing, DNS, connection, response and child operations | A per-socket timeout does not bound the whole request |
| Cancellation | Cooperative signal plus host-operation interruption where supported | Define what actually stops and what only stops being awaited |
| Supervised background job | Explicit owner separate from the response; bounded queue, retry/backoff and shutdown policy | Durable intent should not be an accidental detached promise |

The fail-fast group should surface the primary error and preserve sibling/cleanup
errors as bounded diagnostic detail. Settled mode should retain per-item results
within a result budget. Timing out must not be reported as success with partial
work unless the operation explicitly documents partial results.

Do not add an unrestricted fire-and-forget escape hatch. Request-scoped and
background work require different lifetimes, not merely different spelling.

## Scheduling and overload

Use the runtime's existing async facilities where suitable. Do not spawn an OS
thread or process for each function or promise. Bound admitted tasks, queued work,
blocked producers and total retained bytes. Reject overload with a documented,
stable error; expose queue/wait counters without leaking request data.

CPU-heavy hashing and compression need bounded blocking execution. Cancelling a
wait does not necessarily stop a blocking operation: its permit and resource
accounting must stay owned until that work actually ends. Avoid retrying timed-out
CPU work into an ever-growing background queue.

Preserve current Service Fabric cycle rejection until a replacement model proves
deadlock freedom. More concurrency must not silently change single-worker service
state assumptions or permit concurrent mutation of previously serialized state.

## Buffers, streams and async iteration

Introduce a byte-buffer value distinct from text before exposing binary streams.
Specify host/IPC serialization and bounded ownership or sharing; never advertise
zero-copy without measuring and documenting the actual boundaries.

Streams need byte-based high-water marks, demand-driven reads, producer
backpressure, cancellation and explicit close/error behavior. Chunk counts alone
are insufficient because chunk sizes vary. Whole-body collection should require
an explicit maximum size. Response/client disconnect should cancel upstream reads
and release connections. Bounded channels need a defined full/closed behavior.

Async iterators can represent records, pages and stream chunks. Early exit,
iterator error and cancellation must close the source. Ordinary async functions
do not need generator `*`; a future generator producer syntax is a separate design
decision and should not become a prerequisite for consuming async iterables.

## Cleanup and isolated work

Evaluate `using`, `await using` and `defer` as alternatives for deterministic
resource cleanup. Choose a small consistent surface after specifying reverse
cleanup order, cancellation during cleanup, cleanup deadlines and errors during
an already failing operation. Do not require every alternative syntax at once.

Optional workers can isolate expensive CPU jobs. Give them bounded queues,
explicit serialization costs, limited capabilities and restart policies. Ordinary
pure helpers and business logic should remain in `.module` execution; neither
parallel I/O nor a reusable function requires another HTTP server.

## Implementation and acceptance

ASY-01 specifies scope/error/deadline semantics. ASY-02 adds bounded groups and
combinators. ASY-03 adds buffers, channels and backpressure. ASY-04 adds async
iteration and cleanup. ASY-05 adds supervised jobs and optional CPU isolation.

Test cancellation before admission, during DNS/I/O, after partial output and
during blocking work; group failure; client disconnect; full queues; shutdown;
service reentrancy; and cleanup failure. Use fake clocks/transports where possible.
Measure task/stream memory under load, rather than assuming async is cheaper.
