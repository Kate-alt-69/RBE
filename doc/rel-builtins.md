# Bounded REL built-ins

The REL built-in registry is shared by application evaluation paths. Import
authority remains defined by the [source-role matrix](compatibility.md); these
APIs do not grant `.field` sources I/O, mutable service memory or randomness.

## Password hashing

Import the Argon2id sub-library and use its existing names/aliases:

```text
:import[argon from crypto]

export async function makeVerifier(password) {
	return argon.hashPassword(password);
}
```

New hashes retain the maintained Argon2 library's defaults: 19 MiB memory, two
passes, one lane, random salt, and a 32-byte output. A general SHA-256/HMAC helper
is not a replacement for password hashing.

Verification validates the encoded work parameters before admitting any work:

| Limit | Maximum |
| --- | --- |
| Encoded verifier | 1024 UTF-8 bytes |
| Password | 4096 UTF-8 bytes |
| Memory cost | 65,536 KiB (64 MiB) |
| Passes | 6 |
| Lanes | 4 |
| Memory cost multiplied by passes | 262,144 KiB-passes |
| Hash output | 64 bytes |
| Active password workers | 2 per process; no waiting queue |

All individual and combined limits must pass. An over-budget or malformed hash
returns `CRY1001`; a valid hash with the wrong password returns `false`. Invalid
verifiers no longer silently look like an ordinary password mismatch. Costs are
never lowered to make verification succeed.

The async VM offloads hashing/verification to admitted blocking workers. A full
pool returns `CRY4001`; retry later with bounded backoff. Cancelling the waiting
request does not stop a running Argon2 operation, so its worker permit stays held
until the operation actually ends. Two maximum-cost operations may use up to
128 MiB of Argon2 working memory, plus runtime overhead; limits are per process,
not a promise of whole-server peak RSS.

Synchronous compatibility/transpiled callers share the same cost and admission
checks. They may run outside a Tokio runtime; within Tokio they fail explicitly
instead of blocking an executor thread. Use the async evaluator there. Calling
the async evaluator without Tokio also returns an explicit error. Source-role
permissions, native lowering and explicit interpreter fallback rules still apply.

## Structured logging

```text
:import[log]

export function recordFailure(code) {
	log.error("operation failed", {code: code});
}
```

`log.info`, `log.warn`, `log.error` and `log.debug` accept a message and an optional
fields object. Existing single-message calls remain valid. They return `null`;
logging an error does not send a response or throw an application exception.

Enabled logs are emitted through tracing with nested `fields` encoded as a JSON
string, plus `truncated` and `suppressed` metadata. Application fields do not
replace the host's severity/timestamp envelope. Existing tracing subscribers own
console versus JSON output and level filtering. Request correlation fields can
be supplied explicitly; this API does not invent an ambient request capability.

Encoding caps message/value text at 1024 bytes, field names at 128 bytes, nesting
at four levels, total visited values at 128, and retained text at 4096 bytes.
Oversized field names are omitted. A final 8192-byte message-plus-fields check
also accounts for JSON escaping; oversized output falls back to a small valid
truncation record. These caps apply before recursively formatting the original
object. Disabled levels skip encoding.

Control characters in text are replaced by spaces. Common password, token,
secret, credential, authorization, cookie, API-key, private-key and signature
field names are redacted, including nested fields. URL values starting with
`http://` or `https://` omit their query and fragment. Do not log raw credentials,
headers, request bodies or signed URLs inside prose: redaction cannot recognize
every secret or application-specific naming convention.

The REL logger admits 100 events per process per one-second monotonic window,
across levels. Excess events are discarded without a queue; the next admitted
event reports their count. This cap does not govern unrelated native runtime
logs or the downstream subscriber's own storage/queue policy.

## Outbound HTTP

`http.request` retains its 5-second default timeout and 10-second maximum;
`timeoutMs` now bounds DNS lookup, connection, sending and response-body reading
together. A resolver wait or slow body cannot start a fresh full timeout budget.
Dropping a request cancels its async wait; an OS-level DNS resolver operation
already started by Tokio may still finish in the background.

Transport errors expose a category rather than a URL/error chain that could
contain signed query parameters. Existing HTTP(S)-only policy, forbidden address
checks, validation of resolved addresses, selected-address pinning, disabled
redirects and body/header/envelope limits remain enforced. Resolved-address
collection is bounded to 16 distinct addresses before further retention.

## Service memory

`memory.set(key, value)` returns `true` after successful admission. Limit failure
returns `SVC4202` through REL and both interpreted/native Service IPC dispatch;
it never returns success after dropping a write. The old value is preserved on a
rejected replacement. Reads, delete, clear, length and isEmpty keep their behavior.

Default limits are 4096 entries, an 8 MiB charged payload total, a 1 MiB charged
entry, 1024-byte keys and nesting through depth 32. Charging includes key/text
bytes and per-value/string metadata. This is deterministic payload accounting,
not allocator/RSS measurement. Deletion, successful replacement and clear release
the corresponding budget. Clones share the same admission/accounting lock.

Rust embedders can construct `ServiceMemory::with_limits(ServiceMemoryLimits)`;
`ServiceMemory::set` now returns a `Result` which must be handled. Standard REL
services use the defaults. These bounds supplement process/service policies;
they do not override a stricter host memory limit or evict application state.

## quickDB allocations

Each Service quickDB registry admits at most 64 filters and 8 MiB of allocated
filter payload in aggregate. Names remain bounded to 128 bytes. Scalable filters
have at most 16 layers. Creation and growth check their allocation before
allocating, under the registry write lock; dropping a filter releases its budget.
Clear releases extra scalable layers while retaining the first layer.

An unsuccessful mutation keeps the existing fail-closed readiness rules: the
filter becomes unready/poisoned and must be cleared or rebuilt from authoritative
data before sealing. Bulk mutation may have applied earlier items before a later
failure; it is not a transaction. Membership bits are hints with possible false
positives, not durable rows or an authorization decision. See [quickDB](quickdb.md).
