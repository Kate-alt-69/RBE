# Service error codes

Service codes cover `.service` catalog parsing/validation, Service Mother/worker execution, and standalone `service(.exe)` package compatibility.

## Ranges

| Range | Meaning |
| --- | --- |
| `SVC1000-1099` | `.service` catalog/source declaration problems |
| `SVC2000-2099` | executable REL validation/compilation |
| `SVC4000-4099` | Service runtime/lifecycle/fabric operations |
| `SVC5000-5099` | standalone Service binary/Mother bootstrap/compatibility |
| `SVC5100-5199` | Service control and worker bootstrap |
| `SVC9000-9099` | Service runtime internal invariants |

## Emitted catalog/compiler codes

<a id="svc1000"></a>
### SVC1000 — service directory scan failed

**Status:** Emitted.

RBE could not enumerate the configured Service directory.

**Common causes:** permission failure, invalid path, transient filesystem error.

**Action:** verify the Service directory exists/is readable and that `services.directory` resolves under the intended application root.

<a id="svc1001"></a>
### SVC1001 — service source could not be read

**Status:** Emitted.

A discovered `.service` file could not be read.

**Action:** inspect filesystem permissions, encoding/read errors and the path shown in the diagnostic.

<a id="svc1002"></a>
### SVC1002 — missing `:service[...]` declaration

**Status:** Emitted.

A `.service` file does not contain the required Service declaration.

**Action:** add a valid `:service[...]` declaration near the top of the file.

<a id="svc1003"></a>
### SVC1003 — malformed Service declaration fields

**Status:** Emitted.

The key/value body inside `:service[...]` could not be parsed.

**Action:** check separators, quoting and field spelling.

<a id="svc1004"></a>
### SVC1004 — service name is required

**Status:** Emitted.

The declaration does not provide a non-empty `name`.

<a id="svc1005"></a>
### SVC1005 — unsupported characters in service name

**Status:** Emitted.

Service logical names currently allow ASCII letters/digits plus `-`, `_`, and `.`.

**Action:** rename the service; do not use paths, sockets, spaces or wildcard syntax as a Service name.

<a id="svc1006"></a>
### SVC1006 — duplicate service name

**Status:** Emitted.

Two discovered files declare the same logical Service name.

**Action:** choose one authoritative owner or rename one declaration. Service Fabric routing requires unique logical names.

<a id="svc1007"></a>
### SVC1007 — invalid restart policy

**Status:** Emitted.

The `restart` value is not one of the supported policies. Supported values are currently `always`, `on-failure`, and `never`.

<a id="svc1008"></a>
### SVC1008 — numeric Service policy value is invalid

**Status:** Emitted.

A numeric declaration field could not be parsed as an unsigned integer.

**Action:** check fields such as memory/timeouts/instances for units, signs or non-numeric text.

<a id="svc1009"></a>
### SVC1009 — unsupported Service instance count

**Status:** Emitted.

`instances` is not `1`.

Current Service runtime deliberately requires one instance per declared Service. Multi-instance Service Fabric routing is future work; do not assume changing this validation alone makes multi-instance execution safe.

<a id="svc1010"></a>
### SVC1010 — invalid Service mode

**Status:** Emitted.

The declared mode is not a supported Service mode. Supported values are currently `resident`, `on-demand` and `hybrid`.

<a id="svc1011"></a>
### SVC1011 — invalid idle timeout for lazy Service mode

**Status:** Emitted.

An on-demand/hybrid Service has `idleTimeoutMs = 0`.

**Action:** configure a positive idle timeout, or use `resident` if the Service must remain continuously alive.

<a id="svc2000"></a>
### SVC2000 — executable Service REL validation failed

**Status:** Emitted.

The catalog declaration was accepted, but executable REL inside the Service failed validation/compilation.

The diagnostic includes the file and source location from the underlying REL parser/compiler.

**Action:** fix the reported REL problem. If the underlying error becomes a numbered `RELxxxx`/`RELCxxxx` diagnostic, follow that code for the detailed explanation.

## Standalone Service compatibility

<a id="svc5001"></a>
### SVC5001 — Service binary is incompatible with this backend

**Status:** Emitted.

The standalone Service binary is missing, fails its build-time integrity binding, or successfully answered the compatibility probe but proved it was built for a different runtime/build/target/ABI.

Expected UX:

```text
SVC5001 Service binary is not compatible with the current backend.

  expected_path:
    <application>/dep/service(.exe)

  reason:
    <integrity/build/target/runtime ABI mismatch>

  action:
    Rebuild the complete RBE package and use the Service binary generated
    alongside this backend.

  help:
    doc/error-codes/service.md#svc5001
```

**Do not** diagnose a catalog fingerprint mismatch as SVC5001 if the compatibility handshake already proved the executable is compatible.

<a id="svc5002"></a>
### SVC5002 — Service catalog changed after backend validation

**Status:** Emitted.

Backend and a compatible Service Mother compiled different deterministic Service catalog fingerprints.

The catalog fingerprint covers Service policy/defaults plus each Service's logical identity, mode/restart/resource policy and source digest. The filesystem directory path itself is not part of the fingerprint.

**Common causes:** `.service` files changed during startup, Service settings changed between parent validation and child compilation, parent/child are reading different application roots, or a deterministic compiler/fingerprint defect.

**Action:** stop concurrent source/config mutation and retry. If the same immutable tree repeatedly produces different fingerprints, preserve both hashes/build ID and report it as an RBE bug.

<a id="svc5003"></a>
### SVC5003 — Service compatibility probe failed

**Status:** Emitted.

The configured `service(.exe)` could not execute the bounded compatibility probe, timed out, returned malformed/oversized output, or does not understand the protocol.

This is distinct from SVC5001: SVC5003 means Backend could not obtain a trustworthy compatibility statement at all.

**Action:** replace the binary with one produced by the same RBE build and verify it can execute on the host platform.

<a id="svc5099"></a>
### SVC5099 — Service Mother startup failed with an unclassified error

**Status:** Emitted.

The Service Mother failed during startup, but the underlying error has not yet been migrated to a more specific `SVCxxxx` diagnostic.

This is a fallback envelope, not a replacement for specific diagnostics. If the error chain already contains a specific Service code such as `SVC5001`, that code is reported instead.

**Action:** follow the nested `reason`. If the failure is stable and has no more specific code, preserve the complete error chain so the diagnostic can be classified in a future release.

<a id="svc5100"></a>
### SVC5100 — invalid Service control command

**Status:** Emitted.

`service(.exe)` received a malformed/unsupported operator control command.

**Action:** verify the restart/control command syntax. Internal Mother/worker command-line contracts are not public control commands.

<a id="svc5101"></a>
### SVC5101 — Service restart request could not be queued

**Status:** Emitted.

The control command was valid, but RBE could not persist/queue the restart request for the Service Mother.

**Common causes:** runtime data directory is unavailable/read-only, atomic write failure, or a host filesystem problem.

**Action:** verify the RBE runtime data directory is writable and retry. Preserve the underlying filesystem error if it persists.

<a id="svc5102"></a>
### SVC5102 — invalid internal Service executable mode

**Status:** Emitted.

`service(.exe)` was started without exactly one internal Mother/worker role.

**Action:** launch the Service runtime through `backend(.exe)` rather than manually reproducing internal process arguments.

<a id="svc5199"></a>
### SVC5199 — Service worker startup failed with an unclassified error

**Status:** Emitted.

A Service worker failed during startup, but the underlying error has not yet been migrated to a more specific Service diagnostic.

Like `SVC5099`, this is only a fallback envelope. The underlying error chain should be retained so a more precise code can be assigned later.
