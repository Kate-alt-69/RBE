# REL evolution roadmap

Status: design proposal, 2026-10-17. Implementation baseline:
[`f39cf229`](https://github.com/Kate-alt-69/RBE/commit/f39cf22998d684450ae076b456a0ed87070c7678).
This roadmap records the standalone language discussion. It does not make the
proposed syntax or APIs available, and it is not a completed security audit.

REL should make a small backend easy to write, keep memory and concurrency
predictable, and offer deeper features without requiring a process per helper.
Design decisions must work for unrelated applications and hosting providers.

## Documents

| Document | Contents |
| --- | --- |
| [Syntax and types](rel-design/syntax.md) | Function spelling, bracket declarations, types, collections and Python conveniences |
| [Async and resource ownership](rel-design/async-runtime.md) | Tasks, deadlines, cancellation, streams, cleanup and background work |
| [Built-ins and security](rel-design/builtins-security.md) | Current inventory, source findings, upgrade contracts and regression checks |
| [Small backends](rel-design/small-backends.md) | Authoring, validation, storage, caches, optional features and measurement |
| [SDK compatibility and versions](rel-design/sdk-versioning.md) | Language levels, ABI/protocol negotiation, package identity and binary pins |

For current behavior, start with [REL](rel.md), [compatibility](compatibility.md),
[FieldManager](field-manager.md), and [binary updates](binary-updates.md).

## Feature and use-case table

The origin column describes the inspiration, not a promise of identical language
semantics. **Existing** means the named foundation exists; **extension** means
new behavior is still required; **proposal** means it needs design and code.

| Feature | Why an application author would use it | Inspiration | Status / stage |
| --- | --- | --- | --- |
| `async function`, `async funct` | Write a handler that waits for I/O with familiar function declarations | JavaScript async; REL alias preference | Existing async declarations; alias and full task semantics need verification/extensions, P1–P2 |
| Bracketed function declarations and optional types | Describe inputs/outputs while keeping short helpers concise | REL bracket style; TypeScript/Rust typing | Proposal, P1 |
| Inference, interfaces and readonly values | Catch a misspelled field or mutation before deployment | TypeScript/Rust | Proposal, P1 |
| Static methods and compile-time constants | Put a pure helper or lookup table beside its type without constructing instances | JavaScript/TypeScript; Rust associated items | Proposal, P1 |
| Exhaustive matching and explicit error types | Handle every state of a job or validation result | Rust; Python matching | Proposal, P1 |
| Indexing, bounded `for...of`, closures and collection helpers | Filter invoice lines or compute totals without hand-unrolling items | JavaScript iterables and array methods | General extension, P1; inventory exact current support first |
| Destructuring and spread | Extract selected fields or build a response object | JavaScript; Python unpacking | Proposal, P1 |
| `?.` and `??` | Read an optional nested value without replacing legitimate `false` or `0` | JavaScript | Proposal, P1 |
| Slices | Read a limited section of a string or sequence | Python | Proposal, P1 |
| Task groups and bounded parallel mapping | Query several services without launching unlimited work | JavaScript Promise combinators; Python TaskGroup | Proposal, P2 |
| Request deadlines and cancellation | Stop spending resources after a timeout or disconnect | JavaScript AbortSignal; Rust cancellation patterns | Extension, P0–P2 |
| Byte buffers, streams and backpressure | Upload/download a large object without buffering the whole object | Node.js Buffer/streams; Rust streams | Proposal, P2 |
| Async iterators | Process pages or records incrementally | JavaScript `for await...of`; Python async iteration | Proposal, P2 |
| `using` / async cleanup | Release a file, cursor or connection on success, failure and cancellation | JavaScript explicit resource management; Python context managers; Rust Drop | Proposal, P2 |
| Supervised jobs and bounded channels | Run a webhook queue with ownership, capacity and shutdown rules | Node.js jobs/events; Rust channels | Proposal, P2 |
| Optional CPU workers | Isolate image/CPU work so ordinary handlers can continue | Node.js worker threads; Rust blocking pools | Proposal, later P2 |
| Structured `log.info/warn/error/debug` | Diagnose a route failure with request ID and safe fields | Node.js structured loggers; Rust tracing | `info/warn` existing; extension, P0 |
| `.field` schema improvements | Validate query/body data once and return useful errors | Existing REL FieldManager; schema libraries | Existing foundation; extension, P3 |
| Single-file application mode | Start a tiny health/API service with minimal scaffolding | Node.js/Python application entrypoints | Proposal, P3; preserve source-role authority |
| In-process reusable modules | Share transformations without an extra server hop | Existing REL modules | Existing; improve templates/guidance, P3 |
| Bounded caches and reusable connection pools | Reuse expensive results/connections without unbounded heap growth | Node.js caches; Rust pools | Proposal, P3 |
| Optional SQLite transactions | Store a small local application's data without deploying another server | SQLite ecosystem | Proposal/feasibility, P3 |
| Monotonic time and deterministic test clocks | Make TTLs/deadlines correct despite wall-clock changes | Rust Instant; Node.js performance APIs | Proposal, P0–P3 |
| Dependency-light packaging and runtime inspection | See where memory/time goes and omit unused integrations | Rust feature gating; Node.js diagnostics | Extension/proposal, P3 |
| SDK/runtime compatibility discovery | Know whether a deployed runtime supports a library before serving traffic | Existing RBE handshake; package ecosystems | Existing foundation; extension, cross-cutting |
| Trusted archive and storage operations | Inspect a package or perform a conditional write without an application-specific helper server | Existing RBE host contracts; ZIP/S3 standards | Partial/archive pending at baseline; capability work |

## Delivery order and dependencies

| Stage | Work | Exit condition |
| --- | --- | --- |
| P0 | Argon2 budgets, complete HTTP deadline, logging bounds, service allocation limits | Focused regression tests; no loss of existing security checks or normal input compatibility |
| P1 | Grammar inventory, function/type decisions, collections and developer tooling | Parser/evaluator/RELC agreement; operation budgets; existing programs preserved |
| P2 | Task ownership before parallel combinators; buffers/channels before streaming; cleanup with cancellation | Disconnect, failure, overload and shutdown tests show bounded owned work |
| P3 | Small-app authoring, schema improvements, caches/pools, optional database, diagnostics | End-to-end examples and measured resource costs across supported targets |
| SDK | Compatibility work alongside each stage | Old/new SDK and runtime pairs reject incompatibility before application work |
| Host capabilities | Archive executor and provider/storage audit alongside relevant stages | Working authorized execution with quotas and negative tests, rather than declarations alone |

Start with SEC-01 through SEC-04 in the [built-in plan](rel-design/builtins-security.md).
The syntax work needs an accepted grammar before implementation; richer async
work needs a task model before adding convenient surface spellings.

Host work has two independent items: CAP-01 completes and validates authorized
archive execution after rechecking its merge status; CAP-02 audits existing
storage guarantees and specifies any missing portable signing/conditional-write
operations. Neither requires changing REL into an application-specific framework.

## Decision register

| Decision | State |
| --- | --- |
| Standalone RBE design, usable by arbitrary backends | Required scope |
| Preserve existing `.route`, `.module`, `.service`, `server.server` and pure `.field` boundaries | Required compatibility/security constraint |
| Async function spelling uses `async function` or `async funct`, with no generator `*` | User preference; alias acceptance must be implemented/tested |
| `function [total(items: Item[])], num()` | User-proposed form; grammar, type namespace and tooling still need design |
| `async => function` / `async = function` | Discussed alternative; recommendation is prefix `async`, reserving `=` and `=>` for assignment/arrows |
| Meaningful `request`/`task`/resource names instead of generic `context.run` | Naming direction; individual API names provisional |
| Low-memory defaults and opt-in heavier integrations | Design requirement; benchmark numbers not yet established |
| Version tags refresh on newer builds of the same `VERSION` | Existing binary policy; retain immutable build identities underneath |

## Definition of an implemented feature

A feature needs validated grammar/types, evaluator behavior, capability admission,
correct RELC metadata and native lowering or an explicit fallback, bounded
resources, useful diagnostics, documentation and focused tests. Test all source
roles that can use it and rejection in roles that cannot. Pure `.field` evaluation
must stay deterministic. Runtime work must not silently become remote RPC.

Every implementation change should update its design status and current-reference
docs. Parser recognition, a proposed example, an open feature branch, and a
successful build on one OS are not equivalent to portable runtime support.

## Adjacent work carried forward

Existing [first-run credentials, updates and OS-specific binary distribution](binary-updates.md)
remain part of RBE's distribution story, rather than new language syntax. Build
cache improvements should preserve Cargo's dependency fingerprints and include
source, toolchain, target, features and build options in any outer cache key.
All supported build entrypoints should expose documented help and consistent
options; inventory existing help before adding duplicates. Never put credentials
in public artifacts or cross-user caches.

Application migration, deployment environment repairs, provider log retention and
repository branch cleanup are separate operational tasks. They do not determine
REL's grammar or bake one application's account system into the standard library.
