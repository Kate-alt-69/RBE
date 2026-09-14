# RBE Error Code Book

This directory is the authoritative long-form reference for RBE diagnostics.

Short terminal/compiler errors should stay compact. When a code has a deeper explanation, use the stable code plus this book instead of stuffing every possible cause into one log line.

Example:

```text
RELC3001 Route requires native Container execution but native lowering failed.
help: doc/error-codes/relc.md#relc3001
```

## Code format

RBE diagnostic codes use an uppercase subsystem prefix followed by four decimal digits.

```text
PREFIX1234
```

The prefix identifies the subsystem. The numeric range identifies the class of failure.

| Prefix | Area | Reference |
| --- | --- | --- |
| `REL` | Runtime Engine Language lexer/parser/runtime | [REL](rel.md) |
| `RELC` | REL compiler/linker/Runtime Image generation | [RELC](relc.md) |
| `SVC` | Service compiler/runtime/Mother boot | [Service](service.md) |
| `CTR` | Container Controller/Environment lifecycle | [Container](container.md) |
| `CAP` | Capability broker/gateway/authority | [Container](container.md#capability-codes) |
| `RBE` | backend boot/control-plane/runtime | [Runtime](runtime.md) |
| `ER` | Error Reporter/recovery authority | [Runtime](runtime.md#error-reporter-codes) |
| `VLT` | Vault/secret-runtime failures | [Runtime](runtime.md#vault-codes) |
| `VID` | Video Manager | [Runtime](runtime.md#video-manager-codes) |

## Numeric convention

Unless an older subsystem already owns a range, new codes should use:

- `1xxx` — source/input/discovery/parse problems;
- `2xxx` — semantic/link/authority/configuration problems;
- `3xxx` — executable lowering/runtime preparation problems;
- `4xxx` — runtime operation/lifecycle failures;
- `5xxx` — process/package/compatibility/bootstrap failures;
- `8xxx` — external dependency/platform failures;
- `9xxx` — internal invariant failures: likely RBE bugs rather than user source mistakes.

`9xxx` is intentionally special. A user who sees `RELC9001`, for example, should preserve the diagnostic context and report it instead of blindly rewriting valid REL.

## Status labels

The book distinguishes two states:

- **Emitted** — current `main` can produce this code.
- **Reserved** — the code and meaning are assigned, but a legacy unnumbered diagnostic may still need to be migrated to it.

Reserved codes must not be reused for another meaning.

## Diagnostic UX contract

New user-facing diagnostics should provide, where practical:

```text
CODE concise summary

  source/path/component:
    ...

  reason:
    ...

  action:
    ...

  help:
    doc/error-codes/<book>.md#<lowercase-code>
```

Machine-readable/JSON logging should keep `code`, `module`, severity, and structured context as separate fields rather than parsing the pretty string.

## Maintenance rule

A new stable error code should be added to this book in the same change that introduces it. Codes are never recycled after release, even if the implementation that emitted them is removed.

For tooling and future `--explain <CODE>` support, see [`catalog.json`](catalog.json).
