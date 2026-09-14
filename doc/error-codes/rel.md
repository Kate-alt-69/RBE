# REL error codes

REL codes describe problems in the Runtime Engine Language itself: tokenization, syntax, source-role rules that belong to the language layer, and evaluation/runtime failures.

Current REL parser/evaluator diagnostics are not all numbered yet. The ranges below are the stable migration target and must not be reused for unrelated meanings.

## Ranges

| Range | Meaning |
| --- | --- |
| `REL1000-1099` | lexical/source decoding |
| `REL1100-1199` | syntax/parser |
| `REL1200-1299` | directives and file-role declarations |
| `REL1300-1399` | literal/argument/value-shape validation |
| `REL2000-2099` | semantic language validation |
| `REL3000-3999` | evaluator/runtime execution |
| `REL9000-9099` | internal REL invariant failures / probable engine bugs |

## Reserved migration codes

<a id="rel1001"></a>
### REL1001 — invalid token

**Status:** Reserved.

REL encountered a byte/character sequence that does not form a valid token.

Common causes: unsupported punctuation, an accidental character copied from another language, malformed operator spelling, or source encoding damage.

**Action:** inspect the reported source location and replace the token with supported REL syntax. If ordinary valid syntax triggers this code, preserve the source snippet and report it.

<a id="rel1002"></a>
### REL1002 — unterminated string or literal

**Status:** Reserved.

A quoted literal reached the end of the line/source before its closing delimiter.

**Action:** close the literal and verify escaping around the reported location.

<a id="rel1101"></a>
### REL1101 — unexpected token

**Status:** Reserved.

The parser received a valid token that is not legal in the current grammar position. This is a syntax error, not a RELC linking error.

**Action:** inspect the token immediately before and at the reported location. Missing `)`, `]`, `}`, commas, or malformed declarations commonly shift the parser into the wrong state.

<a id="rel1102"></a>
### REL1102 — missing delimiter or block terminator

**Status:** Reserved.

A syntactic construct started but was not closed correctly.

**Action:** verify matching parentheses/brackets/braces around the reported construct.

<a id="rel1201"></a>
### REL1201 — invalid REL directive

**Status:** Reserved.

A `:<directive>[...]` declaration is malformed or uses fields that are not valid for that directive.

**Action:** check the documentation for the relevant REL file type and the directive field names.

<a id="rel2001"></a>
### REL2001 — unsupported language operation in this source role

**Status:** Reserved.

The syntax is valid REL, but the operation is not valid for the source role at the language layer.

Do not confuse this with RELC capability errors: REL errors describe language/source-role rules; RELC errors describe whole-application linking, authority and Runtime Image construction.

<a id="rel3001"></a>
### REL3001 — unknown identifier at evaluation time

**Status:** Reserved.

The evaluator attempted to resolve a name that does not exist in the active lexical/runtime scope.

**Action:** verify spelling, imports, parameters and local declarations.

<a id="rel3002"></a>
### REL3002 — function call argument mismatch

**Status:** Reserved.

A callable was found, but the invocation does not satisfy its expected parameters.

**Action:** compare the call with the exported/local function signature.

<a id="rel9001"></a>
### REL9001 — REL runtime invariant violated

**Status:** Reserved.

RBE reached a state that should be impossible after successful parsing/validation. This is likely an RBE defect rather than a normal user-source error.

**Action:** do not repeatedly rewrite valid REL to work around it. Capture the full code/message, source role/path, RBE build ID, Runtime Image ID when available, and the smallest reproducer; then report the issue.
