# Runtime subsystem error codes

This page covers backend/runtime (`RBE`), Error Reporter (`ER`), Vault (`VLT`) and Video Manager (`VID`) diagnostic namespaces.

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

**Status:** Reserved.

Backend requires a packaged runtime dependency such as Container or Service, but the expected artifact is missing.

**Action:** rebuild/distribute the complete RBE package instead of copying only `backend(.exe)`.

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
