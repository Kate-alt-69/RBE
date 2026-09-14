# RELC error codes

RELC codes describe whole-application compilation/linking rather than basic REL syntax. RELC discovers sources, resolves identities/imports/capabilities, builds immutable Runtime Images, and prepares native executable artifacts.

## Ranges

| Range | Meaning |
| --- | --- |
| `RELC1000-1099` | source discovery/registration/identity |
| `RELC2000-2099` | imports/symbol linking/dependencies |
| `RELC2100-2199` | capability/authority validation |
| `RELC3000-3099` | native lowering/artifact preparation |
| `RELC3100-3199` | Runtime Image construction/identity |
| `RELC4000-4099` | activation/transition failures |
| `RELC9000-9099` | compiler invariant failures / probable compiler bugs |

## Emitted migration umbrella codes

<a id="relc1000"></a>
### RELC1000 — source discovery/registration error not yet classified more narrowly

**Status:** Emitted.

RELC failed while discovering, extracting, registering, or initially compiling a source, but the originating branch does not yet own a narrower `RELC10xx` code.

<a id="relc2000"></a>
### RELC2000 — link/dependency error not yet classified more narrowly

**Status:** Emitted.

Whole-application linking failed before that branch had a narrower stable `RELC20xx` code. The original link reason remains in the diagnostic.

<a id="relc2200"></a>
### RELC2200 — Runtime ENV/policy/middleware lowering error

**Status:** Emitted.

RELC failed while resolving typed Runtime ENV, ServerPolicy, or MiddlewarePlan state used by the immutable Runtime Image.

## Specific migration codes

<a id="relc1001"></a>
### RELC1001 — duplicate source identity

**Status:** Reserved.

Two physical or embedded REL sources resolved to the same logical source identity.

**Why RELC stops:** Runtime Images require one deterministic owner for every SourceId. Allowing two definitions would make dependency/capability binding ambiguous.

**Action:** rename or move one source so each logical source has a unique identity.

<a id="relc1002"></a>
### RELC1002 — unsupported or inconsistent source role

**Status:** Reserved.

A source was discovered with a role that conflicts with its registration/embedding context.

**Action:** verify that embedded and physical files retain the intended `.route`, `.module`, `.service`, or Server REL role.

<a id="relc2001"></a>
### RELC2001 — unresolved import or symbol

**Status:** Reserved.

A source references a Module/Service/export that is not present in the linked application image.

**Action:** verify logical import names, file names and exported symbol spelling.

<a id="relc2002"></a>
### RELC2002 — dependency cycle is not supported

**Status:** Reserved.

RELC detected a dependency cycle that the current execution model cannot safely satisfy. Service-to-Service synchronous cycles are a common example because a single-request worker model can deadlock.

**Action:** break the synchronous cycle by restructuring ownership, introducing an asynchronous boundary, or moving shared logic to a Module when appropriate.

<a id="relc2101"></a>
### RELC2101 — capability not permitted for source role

**Status:** Emitted.

The REL syntax/import exists, but the source role does not own that capability.

**Action:** move the privileged operation behind the correct owning source and import its exported interface instead of widening the caller's authority.

<a id="relc2102"></a>
### RELC2102 — capability target or operation is invalid

**Status:** Emitted.

A capability request does not map to an exact supported target/operation pair. RELC deliberately rejects wildcard or dynamically widened authority.

<a id="relc3001"></a>
### RELC3001 — native Container execution required but lowering failed

**Status:** Emitted.

The linked Runtime Image requires a capability that is only safe through the Container Controller, but Route-WASM lowering could not produce an executable artifact for that route.

This is intentionally fail-closed. RELC must not silently fall back to an in-process interpreter when doing so would bypass Container capability mediation.

**Action:** read the lowering reason included in the diagnostic. Rewrite only unsupported syntax that genuinely blocks native lowering. If a supported construct triggers this code, treat it as a compiler defect and report it.

<a id="relc3101"></a>
### RELC3101 — Runtime Image identity could not be constructed

**Status:** Reserved.

RELC failed while canonicalizing or hashing the immutable application state used for Runtime Image identity. Because Runtime Image IDs participate in capability authority, RELC must not invent or partially recover an identity.

<a id="relc4001"></a>
### RELC4001 — Runtime Image activation rejected

**Status:** Reserved.

A compiled Runtime Image could not become the active image because activation invariants failed.

**Action:** preserve both the old and candidate image IDs plus the full activation diagnostic.

<a id="relc9001"></a>
### RELC9001 — compiler invariant violated

**Status:** Reserved.

RELC reached a state that should be impossible if earlier compiler passes behaved correctly. This is the canonical "likely RELC bug" code.

**Action:** capture and report the complete diagnostic/context, RBE build ID, source path/role, Runtime Image/source hashes if available, and the smallest reproducible REL project. Do not treat `RELC9001` as a normal syntax mistake.

<a id="relc9002"></a>
### RELC9002 — deterministic compiler output diverged

**Status:** Reserved.

Two compiler paths/runs that should produce identical canonical output produced different identities/artifacts.

This is a correctness bug because Runtime Image identity and capability provenance rely on deterministic compilation.

**Action:** preserve both outputs/hashes and report the bug; avoid deploying the divergent build until understood.
