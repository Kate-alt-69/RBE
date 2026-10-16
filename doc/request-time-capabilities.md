# Request-time capabilities in the main backend

REL can perform bounded native archive work within the main Backend process.
It does not send an archive request to Cloud Node, the publisher HTTP server, a
shell tool, or a JavaScript runtime. ZIP parsing uses the existing Rust `zip`
dependency through `install-runtime::workspace_archive`. The maintained
`unicode-normalization` crate supplies canonical collision checks across platforms.

## Execute a local operation

Archive functions produce deferred plans. Use `workspace.run(operation)` to
execute against the frozen ProjectRoot, or `workspace.temp(operation)` to give
one operation an isolated `??/` directory that is removed on completion.

```rel
:import[workspace, archive]

export function inspectCandidate() {
	const index = workspace.run(archive.list("$$/data/uploads/candidate.zip"));
	const manifest = workspace.run(archive.read(
		"$$/data/uploads/candidate.zip", "package.rbe.yaml"
	));
	return {
		entries: index.entries,
		manifest: manifest.text,
		manifestSha256: manifest.sha256
	};
}
```

Use an application-owned upload identifier to select a path. Do not concatenate
an unrestricted client path or expose an unrestricted filesystem browser. The
existing REL import/capability rules still apply; pure `.field` code cannot
acquire archive, workspace, script or storage authority.

Supported ZIP operations are `list`, `read`, `extract`, `create`, `replace`, and
`remove`. TAR/TGZ/TXZ plans remain recognized but execution rejects them. This is
an archive engine, not the publisher's package schema/version/ownership validator.
An application still needs to validate the manifest and apply its account rules.

Results:

| Operation | Result fields |
| --- | --- |
| `list` | `ok`, `entries`: array of `{path, directory, size, compressedSize}` |
| `read` | `ok`, `path`, `size`, `sha256`, `text`, `dataHex` |
| `extract/create/replace/remove` | `ok`, `bytesWritten`, `entries` (count) |

`read.text` is a UTF-8 string or null for binary content; `dataHex` is available
for both. A read is limited to 1 MiB. `extract` takes exact file names; an empty
array selects all entries. Creation includes a directory's contents, not its
outer folder. Replace/remove require an existing entry.

Persistent output uses an explicit `$$/` destination. Output under `??/` disappears
when the temporary call ends, including after request cancellation once its
blocking worker finishes. Constructed multi-step workspace graphs are still
unimplemented; `workspace.run` materializes a single deferred operation only.

## Limits and failure behavior

| Bound | Limit |
| --- | --- |
| Compressed ZIP or newly encoded ZIP | 16 MiB |
| Individual expanded entry | 16 MiB |
| Aggregate declared expanded data | 64 MiB |
| Entries | 4,096 |
| One returned entry | 1 MiB |
| Compression ratio per entry | 200:1 |
| Normalized entry path | 1,024 UTF-8 bytes, 64 segments |
| Concurrent native archive operations | 2 per Backend process |

Admission occurs before spawning blocking work. Excess concurrency fails with a
retryable “workers are busy” diagnostic rather than growing a hidden job queue.
Compression and filesystem work does not block Tokio's request workers. A cancelled
request does not immediately cancel native I/O; its permit and temporary workspace
remain owned by the bounded blocking task until it completes.

The engine rejects traversal, absolute/drive/ADS paths, NUL, invalid UTF-8 entry
names, Windows device/trailing-dot/short-name aliases, duplicate/case/canonical-Unicode-colliding paths, files
used as ancestor directories, ZIP symlinks/special entries, encrypted ZIPs and
unsupported compression. Only Stored and Deflated entries are enabled. Root paths
and output components reject filesystem symlinks. Filesystem roots must be managed
by the trusted backend OS identity; these checks do not isolate against another
process with the same identity deliberately racing filesystem changes.

Metadata inspection checks the whole ZIP before exposing names or extracting.
Entry reads check decoded sizes and CRCs; listing does not inflate every entry or
claim to verify every payload CRC. Actual reads, encoder writes and encoder seeks
are bounded, so declared lengths alone do not authorize unlimited allocation.
ZIP rewrites commit through AtomicIo only after successful encoding and validation.
Backend serializes local archive mutations to prevent two local rewrite operations
from losing each other's edits. This is not a cross-process or provider CAS API.

Extraction uses atomic replacement per file, not one atomic directory transaction.
A later corrupt payload or I/O failure can leave previously extracted files.
Prefer a temporary workspace for untrusted extraction. Mutating a ZIP does not
execute its contents or carry over executable permission bits.

The 16 MiB in-process limit is deliberately lower than the publisher's 128 MiB
package-upload limit. This change does not remove the publisher or migrate its
larger upload paths. Bulk archives/builds need a separately bounded worker path.

## Remaining publisher migration boundary

The focused storage audit found reusable provider implementations, but they are
not yet equivalent to an authorized REL application API:

| Requirement | Current implementation and migration work |
| --- | --- |
| Password hashing, tokens, login pages, sessions | Existing REL primitives can express these; the application and data migration still need implementing. |
| ZIP inspection/read | Local executor is now wired, subject to the limits above. Package manifest schema and publish rules remain application validation. |
| Signed S3 GET/PUT URLs | Publisher `trusted/rpx-publisher/src/s3.rs` delegates `presign_get`/`presign_put` to its native signer. REL needs an admitted signer capability bounded to provider, method, key prefix, TTL and upload policy. Provider credentials must stay in the trusted host. |
| Unique identity claims | Publisher uses `put_bytes_if_absent`/`put_json_if_absent`, with an explicit already-exists result. A REL equivalent must expose atomic create-if-absent, not an `exists` followed by a write. |
| Concurrent account/session updates | Publisher reads an ETag and uses `put_json_if_match`. A REL equivalent needs an opaque version and explicit conflict handling; an unconditional overwrite loses this guarantee. |
| Provider snapshot concurrency | Cloud Node `provider.rs` already performs conditional provider HEAD writes for synchronization. That sync protocol is not a general application key/value CAS endpoint. |
| Local/environment storage | REL Storage handles environment transactions and authorized project-root writes. Local file atomic replacement does not establish multi-instance provider uniqueness. |

Before moving identity/session ownership off the publisher: implement the scoped
signer/CAS host interfaces; exercise create races and stale-version conflicts;
preserve the existing object-key layout and account/session data; then port and
verify application behavior against those interfaces. There is no need for a
separate login HTTP server solely to perform a small native operation, but its
storage guarantees must survive the migration.
