# Built-in inventory and proposed security work

Status: source review and implementation plan, 2026-10-17, at the
[roadmap baseline](../rel-roadmap.md). Findings below are not a penetration-test
report or proof that an application can exploit each path. No security fix is
implemented by this document.

## Current built-in surfaces

This is a grouped inventory, not an exhaustive alias/signature reference. Import
recognition is distinct from permitted authority and working execution. See
[compatibility](../compatibility.md) for role restrictions.

| Surface | Current behavior / boundary | Useful next work |
| --- | --- | --- |
| `http` | Async request/get/post host operations; body/header limits, public destination checks, pinned DNS result and redirects disabled | Complete deadline, sanitized errors, cancellation and carefully scoped pooling |
| `crypto` | SHA-256/hash aliases, HMAC-SHA256 and OS-backed random bytes/tokens with input limits | Clear hash-vs-password guidance; aggregate work budgets; keep maintained implementations |
| `argon` under `crypto` | Argon2id hash/verify; encoded string/password length and algorithm checks | Parameter cost ceilings and bounded blocking admission |
| `security` | Constant-time comparison helper, secure-request and client-IP helpers | Test trusted-proxy handling end to end; preserve constant-time implementation |
| `log` | `info` and `warn`; first value formatted with debug representation | Structured fields, error/debug levels, bounded encoding, redaction and rate limits |
| `request` | Header/query/parameter/cookie/method/path/body snapshot access | Typed access and deadline/request-ID extensions within existing role authority |
| `response` | JSON/text/HTML/status/redirect/headers/cookies | Session-cookie safe defaults; prefix/SameSite validation; header/redirect regression checks |
| `json` | Parse/stringify via existing value conversions | Explicit byte/depth/output budgets and optional future incremental processing |
| `regx` | Test/raw regex paths; 4 KiB pattern, 1 MiB input, raw backtracking budget | Bounded compiled-pattern cache and per-request aggregate CPU budgets |
| `time` | Wall-clock UNIX time | Monotonic deadlines, duration units and test clocks |
| `ENV` | Typed immutable Runtime Image configuration; not OS environment | Keep non-secret configuration separate from Vault and secret capability access |
| Legacy `env` | Compatibility implementation can read process environment; rejected by RELC-linked applications | Keep linked applications restricted; audit compatibility entrypoints |
| Service `memory` | Process-local service state operations | Entry/total-byte accounting and explicit overflow/eviction policy |
| Service `quickDB` | Probabilistic membership/indexing filters, with configuration checks and fallible allocation | Aggregate/layer/filter-count quotas; false-positive guidance; never use as an authorization database |
| `field` / `.field` | Host-owned typed request validation with bounded dynamic families and deterministic helpers | Better schemas/errors and strict opt-in modes without changing existing coercion silently |
| `storage` | Module-owned Environment Storage reads/transactions/writes, plus contained ProjectRoot write contract | Audit provider-signing and conditional-write API gaps against current storage guarantees |
| `workspace`, `script`, `archive` | Host contracts with partial execution; archive executor still unwired at this baseline | Complete trusted execution with path containment, expansion/work limits and fail-closed behavior |
| `math`, `net`, `private` | Numeric/text helpers, ping and health helpers under role admission | Specify numeric edge cases and safe diagnostic output |
| `vm` / Video Manager | Privileged module-host media operations | Maintain admission, input/work budgets and cancellation alongside media changes |

Vault and host admission are security architecture, not permission for arbitrary
REL code to read secrets. Preserve [source/security boundaries](../source-security.md)
and the separate [storage contract](../storage.md).

## Concrete first fixes

### SEC-01 — Argon2 work bounds

Evidence: [`call_crypto_argon`](../../engine/crates/route-engine/src/modules.rs)
parses a bounded PHC string, checks `argon2id`, then delegates verification. The
wrapper does not impose explicit ceilings on encoded memory/time/parallelism
parameters before expensive verification. Encoded length limits do not bound those
costs. Risk depends on whether an application lets untrusted hashes reach this API.

Inspect the maintained Argon2 parameter parser and enforce documented maximum
costs before allocation. Select ceilings from realistic deployment budgets; do
not weaken normal password hashing to fit a low default or silently lower encoded
costs. Old valid hashes within policy must still verify, and over-budget hashes
need a distinguishable policy error rather than an ordinary password mismatch.

Move expensive work through bounded blocking admission with finite queueing.
Keep the permit until underlying work finishes even if the caller is cancelled.
Test malformed/unsupported/over-budget parameters by proving rejection before
work starts; never run an attacker-sized allocation as a regression test.

### SEC-02 — HTTP deadline and error hygiene

Evidence: [`call_public_http`](../../engine/crates/core/src/network_broker.rs)
awaits explicit DNS resolution before constructing the reqwest client whose
timeouts apply to the later request. Give the entire operation one deadline,
including admission/DNS/connect/send/body. This is a timeout coverage gap, not a
claim that existing SSRF protections are absent.

Retain scheme/userinfo checks, rejection of non-public addresses, validation of
all resolved addresses, selected-address pinning, redirect policy and payload
limits. Review error conversion so URLs containing signed queries or credentials
do not appear in logs/client errors. Pooling is a separate optimization requiring
the same connection/destination policy; it must not reintroduce DNS rebinding.

Test a stalled resolver with an injected transport/clock, response/body stalls,
private/mapped addresses, mixed public/private results, oversized input/output,
redirects and cancellation. Existing negative network tests must continue passing.

### SEC-03 — bounded structured logs

Evidence: [`call_log`](../../engine/crates/route-engine/src/modules.rs) formats
the first value using `Debug`, supports only info/warn, and has no local structured
field/redaction/truncation budget. Applications can accidentally emit sensitive or
very large values. This does not establish that a specific deployed log leaked.

Proposed API, not current behavior:

```text
:import[log]

log.info("order accepted", {orderId: order.id});
log.warn("upstream unavailable", {requestId: request.id()});
log.error("operation failed", {code: error.code});
log.debug("cache lookup", {hit: true});
```

Use structured fields and host-attached request metadata without granting new
request authority. Bound message bytes, field count, nested depth, string sizes,
stack/cause chains and total encoded event bytes. Filter disabled levels before
serialization. Use safe JSON/console encoding to prevent control-character log
forgery. Reserve host fields so application fields cannot forge severity/request ID.

Redact known secret fields and respect opaque secret handles. Redaction cannot
recognize every secret inside arbitrary prose: document safe logging, omit raw
headers/bodies/tokens and signed URLs by default, and avoid claiming perfect
automatic detection. Bound queue size and rate; report dropped/suppressed counts
without logging dropped content. Logging an error must not itself send an HTTP
response or expose internal causes to the client.

Test disabled-level work, deeply nested/large values, secret keys, control bytes,
reserved-field collisions, quotas and logger failure. Keep simple existing
`log.info("message")` calls working.

### SEC-04 — aggregate service allocations

Evidence: service memory stores JSON values, and quickDB can grow filters despite
per-configuration validation and fallible allocation. Add explicit total-byte,
entry/filter/layer and admission limits. A process memory limit is not a substitute
for an actionable per-operation rejection before allocation.

Replacement/delete/clear must release accounting; a failed mutation must not leave
partial state or incorrect counters. Reject arithmetic overflow. Define whether
eviction is supported and explicit; never silently evict application state.
Test repeated insert/remove, expansion at limits, replacement and parallel calls
under whatever service scheduling model is actually admitted.

## Follow-up hardening

Response cookies need a policy review of SameSite=None/Secure and `__Host-` /
`__Secure-` invariants. Prefer an explicit session-cookie helper with safe defaults
over breaking generic cookie behavior without migration guidance. Proxy/IP trust,
JSON budgets, regex CPU/cache quotas, monotonic time and typed field semantics
need focused follow-up tests rather than speculative rewrites.

Archive declarations still fail closed at the baseline. Recheck the separate
executor change before marking execution complete; require traversal, symlink,
duplicate-name, decompression, CPU/concurrency and atomic-write tests. External
script execution must retain trusted runtime/container admission and must never
fall back to ambient PATH commands when the managed runtime is missing.

## Completion standard

For each item, trace Route/Module/Service and native/interpreter host paths so an
alternate entrypoint cannot bypass a new check. Use existing crypto/network
libraries, preserve useful errors without secret values, and add focused tests
before changing current-reference documentation. Record passed/failed/unrun
checks and measured limits; do not label the whole runtime secure from this list.
