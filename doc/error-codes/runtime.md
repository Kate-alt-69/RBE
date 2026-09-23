# Runtime subsystem error codes

This page covers backend/runtime (`RBE`), Error Reporter (`ER`), Vault (`VLT`), Video Manager (`VID`) and REL cryptography (`CRY`) diagnostic namespaces.

Most of these subsystems still contain legacy unnumbered diagnostics. The ranges below reserve stable code space for migration; existing numbered Service/Container codes remain documented in their own books.

## Backend (`RBE`)

| Range | Meaning |
| --- | --- |
| `RBE1000-1999` | configuration/input/bootstrap discovery |
| `RBE2000-2999` | control-plane/runtime policy |
| `RBE4000-4999` | runtime lifecycle/supervision |
| `RBE5000-5999` | packaged dependency/build compatibility |
| `RBE9000-9099` | backend invariants / probable RBE bugs |

<a id="rbe5001"></a>
### RBE5001 — required packaged dependency missing

**Status:** Emitted.

A required packaged runtime dependency is absent from the expected application-relative path. The normal backend boot path currently emits this code when the packaged Container runtime is missing.

**Action:** rebuild/reinstall the complete RBE package for the same target. Do not satisfy this error by copying an unrelated Container binary into place.

<a id="rbe5002"></a>
### RBE5002 — backend Container binding metadata is invalid

**Status:** Emitted.

The backend build does not contain a complete, valid integrity binding for its packaged Container runtime. This is a package/build compatibility problem, not an instruction to disable verification.

**Action:** rebuild the complete RBE package for the same target so backend and Container metadata are generated together.

<a id="rbe5003"></a>
### RBE5003 — packaged Container failed backend integrity verification

**Status:** Emitted.

The packaged Container could not be read/hashed or its hash/signature does not match the artifact cryptographically bound to this backend build.

**Action:** replace the complete package with a coherent build for the same target. Do not copy `container`/`container.exe` between backend builds.

<a id="rbe5099"></a>
### RBE5099 — backend startup failed with an unclassified boot error

**Status:** Emitted.

Backend startup returned an error that does not yet own a narrower stable `RBExxxx` diagnostic code. If terminal logging was already initialized, RBE reports this through `FATAL [BACKEND:BOOT]`; otherwise it uses the same structured text through the early stderr fallback.

**Action:** follow the nested `reason` first. If the same immutable configuration/build repeatedly fails without a more specific code, preserve the startup logs and report it so the originating branch can receive a narrower code.

<a id="rbe5100"></a>
### RBE5100 — Runtime Image compilation failed during backend startup

**Status:** Emitted.

Backend startup reached Runtime Image compilation, but REL/RELC rejected one of the application sources or linking rules before the API listener was bound. `RBE5100` is the boot-level classification; the nested compiler diagnostic remains authoritative for the actual source problem, for example `REL1100` for parser syntax or a `RELCxxxx` capability/link error.

For physical REL parse failures, RBE renders a RustC-style source frame with the source path, exact line/column, the offending source line, a caret, a human-readable `note:`, a likely `hint:`, and the nested REL/RELC Error Code Book link. Parser token debug names such as `RParen` are converted to source spelling such as `)` in the user-facing diagnostic.

**Action:** fix the nested compiler diagnostic first, then restart/redeploy. Do not treat `RBE5100` as an instruction to bypass RELC validation; compilation intentionally stops before RBE binds the public API.

<a id="rbe9001"></a>
### RBE9001 — backend control-plane invariant violated

**Status:** Reserved.

Backend reached a state that should be impossible after successful startup validation.

Capture build ID, active Runtime Image ID, process/component and the full diagnostic before reporting it.

<a id="error-reporter-codes"></a>
## Error Reporter codes

| Range | Meaning |
| --- | --- |
| `ER1000-1999` | issue input/queue validation |
| `ER2000-2999` | authority/recovery request validation |
| `ER4000-4999` | daemon processing/persistence |
| `ER5000-5999` | daemon bootstrap/process compatibility |
| `ER9000-9099` | Error Reporter invariants |

<a id="er4001"></a>
### ER4001 — durable issue processing failed

**Status:** Reserved.

The Error Reporter accepted or observed an issue but could not complete its durable processing path.

**Action:** inspect Error Reporter status, queue state and `last_error_message`; preserve the original issue if possible.

<a id="er9001"></a>
### ER9001 — recovery-authority invariant violated

**Status:** Reserved.

CONTROL ER produced or observed a recovery state outside its bounded authority model.

This should be treated as an RBE bug, not as permission to bypass the recovery boundary.

<a id="vault-codes"></a>
## Vault codes

| Range | Meaning |
| --- | --- |
| `VLT1000-1999` | credential name/ACL/input validation |
| `VLT2000-2999` | authorization/policy |
| `VLT4000-4999` | keyring/file-store operations |
| `VLT5000-5999` | secret backend/bootstrap availability |
| `VLT9000-9099` | Vault invariants |

<a id="vlt5001"></a>
### VLT5001 — no usable secret backend

**Status:** Reserved.

Vault could not initialize the required platform keyring/Secret Service or its permitted fallback backend.

**Action:** fix the platform secret service or configured fallback storage; do not replace this with plaintext application secrets.

<a id="vlt9001"></a>
### VLT9001 — Vault authorization invariant violated

**Status:** Reserved.

A credential operation escaped the expected caller/ACL/secret-backend model.

Treat this as a security bug and preserve the caller identity plus operation metadata.

<a id="crypto-codes"></a>
## REL cryptography codes

| Range | Meaning |
| --- | --- |
| `CRY1000-1999` | crypto API input/operation validation |
| `CRY3000-3999` | secure entropy/runtime preparation failures |
| `CRY4000-4999` | password/authentication crypto execution |
| `CRY9000-9099` | crypto invariants / probable RBE bugs |

<a id="cry1001"></a>
### CRY1001 — invalid cryptography argument

**Status:** Emitted.

A REL cryptography helper received the wrong number/type of arguments or a value outside its bounded input/length policy.

**Action:** use the function signature and limits shown in the diagnostic. Do not remove the limits to accept attacker-controlled unbounded crypto work.

<a id="cry1002"></a>
### CRY1002 — unknown cryptography operation

**Status:** Emitted.

REL attempted to call a function that the `crypto` builtin does not export.

**Action:** use an explicitly supported crypto operation; do not infer undocumented host cryptography APIs.

<a id="cry3001"></a>
### CRY3001 — secure random generation failed

**Status:** Emitted.

The operating-system cryptographically secure random generator could not supply the requested entropy. RBE fails closed rather than substituting a predictable PRNG.

**Action:** inspect the host entropy/platform failure. Do not replace this failure with timestamps, counters or non-cryptographic randomness.

<a id="cry4001"></a>
### CRY4001 — Argon2id password hashing failed

**Status:** Emitted.

The `argon from crypto` sub-library could not complete an Argon2id password-hash operation. RBE owns the Argon2id parameters and random salt generation; REL callers cannot weaken or reuse them manually.

**Action:** preserve the diagnostic and inspect resource/runtime failure. Do not fall back to a fast general-purpose hash for passwords.

<a id="video-manager-codes"></a>
## Video Manager codes

| Range | Meaning |
| --- | --- |
| `VID1000-1999` | media/probe/input validation |
| `VID2000-2999` | policy/job/asset state validation |
| `VID4000-4999` | worker/live-session execution |
| `VID5000-5999` | FFmpeg/platform dependency compatibility |
| `VID9000-9099` | Video Manager invariants |

<a id="vid5001"></a>
### VID5001 — required media tool/capability unavailable

**Status:** Reserved.

The configured FFmpeg/FFprobe runtime cannot provide a capability required by the active Video Manager policy.

**Action:** verify the configured executable and supported codecs/encoders. Hardware encoder failure may legitimately fall back to software only where the policy explicitly allows it.

<a id="vid9001"></a>
### VID9001 — Video Manager state-machine invariant violated

**Status:** Reserved.

A persisted/in-memory asset, job or live-session state transition violated the allowed state machine.

Capture the asset/job/session ID and current/attempted states and report it.
