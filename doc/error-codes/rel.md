# REL error codes

REL codes describe failures in the Runtime Engine Language itself: source decoding, tokenization, syntax, file-role declarations, value-shape validation, semantic language rules, and evaluator/runtime failures.

The goal is that ordinary author mistakes receive a narrow stable code plus a source frame, note, hint, and Error Code Book link. `REL1000` and `REL1100` remain migration umbrellas only for branches that have not yet been classified more narrowly.

## Ranges

| Range | Meaning |
| --- | --- |
| `REL1000-1099` | lexical/source decoding |
| `REL1100-1199` | syntax/parser |
| `REL1200-1299` | directives, imports, declarations, and file-role structure |
| `REL1300-1399` | literals, arguments, parameters, and value-shape validation |
| `REL2000-2099` | semantic language validation |
| `REL3000-3999` | evaluator/runtime execution |
| `REL9000-9099` | internal REL invariant failures / probable engine bugs |

## Migration umbrella codes

<a id="rel1000"></a>
### REL1000 — lexical error not yet classified more narrowly

**Status:** Emitted only when a lexer branch has not yet migrated to a narrower `REL10xx` code.

REL tokenization failed before a narrower stable lexer code was selected.

**Action:** use the source frame and lexer message. New releases may replace this umbrella with a more specific `REL1001+` code without changing the underlying language rule.

<a id="rel1100"></a>
### REL1100 — syntax/parser error not yet classified more narrowly

**Status:** Emitted only when a parser branch has not yet migrated to a narrower `REL11xx-13xx` code.

The REL parser rejected the source, but the exact parser branch still uses the migration umbrella.

**Action:** use the highlighted source span, `note:` and `hint:`. A later release may emit a narrower stable code for the same source mistake.

## Lexical/source decoding

<a id="rel1001"></a>
### REL1001 — invalid token or operator spelling

**Status:** Emitted by Runtime Image compiler diagnostics.

REL encountered punctuation or an operator spelling that cannot form a valid token. Examples include a lone `&` where `&&` is required, a lone `|` where `||` is required, or unsupported punctuation.

**Action:** replace the highlighted character/operator with supported REL syntax.

<a id="rel1002"></a>
### REL1002 — unterminated string or escape sequence

**Status:** Emitted by Runtime Image compiler diagnostics.

A quoted literal or escape sequence reached the end of its valid source range before being closed.

**Action:** close the string and verify the final escape sequence.

<a id="rel1003"></a>
### REL1003 — malformed project-root path literal

**Status:** Emitted by Runtime Image compiler diagnostics.

A `$$/` project-root path is malformed, does not begin with the required prefix, or does not name a relative target.

**Action:** use a non-empty path such as `$$/generated/data.json`.

<a id="rel1004"></a>
### REL1004 — malformed numeric literal

**Status:** Emitted by Runtime Image compiler diagnostics.

A sequence beginning as a number could not be parsed as one finite REL numeric token. This includes malformed decimal syntax such as multiple decimal points.

**Action:** rewrite the highlighted number using one valid numeric literal. REL must never silently coerce malformed numeric text to `0`.

## Syntax/parser

<a id="rel1101"></a>
### REL1101 — unexpected token or expression

**Status:** Emitted by Runtime Image compiler diagnostics.

The parser received a valid token that is not legal in the current grammar position.

**Action:** inspect the highlighted token and the operator/delimiter immediately before it.

<a id="rel1102"></a>
### REL1102 — missing delimiter or statement terminator

**Status:** Emitted by Runtime Image compiler diagnostics.

A construct was opened but not closed correctly, or a required `;` terminator is missing.

**Action:** verify matching `()`, `[]`, `{}` and the preceding statement terminator.

<a id="rel1103"></a>
### REL1103 — identifier expected

**Status:** Emitted by Runtime Image compiler diagnostics.

The current grammar position requires a name but received another token.

**Action:** provide a valid REL identifier and check whether an earlier missing delimiter shifted the parser into the wrong position.

<a id="rel1104"></a>
### REL1104 — invalid operator/operand sequence

**Status:** Emitted by Runtime Image compiler diagnostics.

A unary or binary operator does not have the operand shape required by REL, for example a dangling `&&`, `||`, comparison, or arithmetic operator.

**Action:** add the missing expression or remove the stray operator.

<a id="rel1105"></a>
### REL1105 — invalid statement or trailing source content

**Status:** Emitted by Runtime Image compiler diagnostics.

The parser completed a declaration/block but found source text that cannot begin another legal statement or declaration.

**Action:** inspect the highlighted trailing token and the statement immediately before it.

<a id="rel1106"></a>
### REL1106 — malformed parameter or argument list

**Status:** Emitted by Runtime Image compiler diagnostics.

A function parameter list or call argument list has invalid separators, delimiters, or item syntax.

**Action:** verify commas, names/expressions, and the closing `)`.

<a id="rel1107"></a>
### REL1107 — malformed block or function body

**Status:** Emitted by Runtime Image compiler diagnostics.

A function/class/control-flow body cannot be reconstructed because its block syntax is incomplete or malformed.

**Action:** verify the opening/closing braces and the statements inside the highlighted block.

## Directives, imports, declarations, and source roles

<a id="rel1201"></a>
### REL1201 — invalid REL directive

**Status:** Emitted by Runtime Image compiler diagnostics.

A `:<directive>[...]` declaration is malformed or contains syntax that is not valid for that directive.

**Action:** check the directive spelling, brackets, fields, separators, and supported options.

<a id="rel1202"></a>
### REL1202 — invalid source-role declaration

**Status:** Emitted by Runtime Image compiler diagnostics.

The file is valid REL-shaped text but does not satisfy the required top-level structure for its role (`.route`, `.module`, `.service`, `.field`, or `server.server`).

**Action:** use the declaration structure required by that source role.

<a id="rel1203"></a>
### REL1203 — malformed import declaration

**Status:** Emitted by Runtime Image compiler diagnostics.

A `:import[...]` entry is incomplete, has invalid target/alias syntax, or uses malformed separators.

**Action:** correct the highlighted import entry. Import target existence is a RELC linking concern, not this code.

<a id="rel1204"></a>
### REL1204 — duplicate declaration, export, member, or binding

**Status:** Emitted by Runtime Image compiler diagnostics.

The same declaration identity appears more than once where REL requires uniqueness.

**Action:** rename or remove the duplicate declaration.

<a id="rel1205"></a>
### REL1205 — invalid route method or Service lifecycle member

**Status:** Emitted by Runtime Image compiler diagnostics.

A class member name is not legal for the active source role, such as a non-HTTP method in `class Route` or an unsupported Service lifecycle member.

**Action:** use a supported HTTP verb or Service lifecycle member.

<a id="rel1206"></a>
### REL1206 — required source-role declaration missing

**Status:** Emitted by Runtime Image compiler diagnostics.

A source role requires metadata or a structural declaration that is absent, for example missing FieldManager metadata.

**Action:** add the required declaration shown by the diagnostic.

## Literal, argument, parameter, and value-shape validation

<a id="rel1301"></a>
### REL1301 — invalid literal or value type

**Status:** Emitted by Runtime Image compiler diagnostics.

A literal/value exists syntactically but has the wrong type or shape for the current language construct.

**Action:** use one of the value types listed by the diagnostic.

<a id="rel1302"></a>
### REL1302 — invalid constant expression

**Status:** Emitted by Runtime Image compiler diagnostics.

A location restricted to compile-time literal values received a dynamic expression.

**Action:** replace it with a supported literal/array/object constant, or move dynamic work into executable REL.

<a id="rel1303"></a>
### REL1303 — invalid argument or parameter count

**Status:** Emitted by Runtime Image compiler diagnostics.

A declaration or language-owned call has too many/few parameters for that construct.

**Action:** match the signature shown in the diagnostic.

<a id="rel1304"></a>
### REL1304 — invalid FieldManager binding configuration

**Status:** Emitted by Runtime Image compiler diagnostics.

A FieldManager binding combines options that cannot be used together, uses an unsupported resolver mode, or violates a binding-specific rule.

**Action:** use the supported `required`, `optional`, or `dynamic` shape and only the options permitted for that mode.

## Semantic language validation

<a id="rel2001"></a>
### REL2001 — unsupported language operation in this source role

**Status:** Reserved for direct language semantic validation.

The syntax is valid REL, but the operation is not valid for the current source role at the language layer.

Do not confuse this with RELC capability errors: REL describes language/source-role rules; RELC describes whole-application linking, authority, and Runtime Image construction.

<a id="rel2002"></a>
### REL2002 — invalid declaration relationship

**Status:** Reserved.

Individually valid declarations conflict semantically in a way the language does not permit.

**Action:** follow the relationship described in the diagnostic rather than rewriting unrelated syntax.

<a id="rel2003"></a>
### REL2003 — reserved language name used illegally

**Status:** Reserved.

A declaration uses a name reserved by REL or by the active source-role grammar.

**Action:** rename the declaration; do not shadow the reserved language/runtime name.

<a id="rel2004"></a>
### REL2004 — invalid Server REL semantic configuration

**Status:** Emitted by Runtime Image compiler diagnostics.

`server.server` parsed successfully but a Server REL setting/section violates a semantic rule, such as an invalid status, duplicate reserved section, or incorrect section body shape.

**Action:** correct the highlighted Server REL setting according to the accompanying note/hint.

## Evaluator/runtime execution

<a id="rel3001"></a>
### REL3001 — unknown identifier at evaluation time

**Status:** Reserved.

The evaluator attempted to resolve a name that does not exist in the active lexical/runtime scope.

**Action:** verify spelling, imports, parameters, and local declarations.

<a id="rel3002"></a>
### REL3002 — function call argument mismatch

**Status:** Reserved.

A callable was found, but the invocation does not satisfy its expected parameters.

**Action:** compare the call with the exported/local function signature.

<a id="rel3003"></a>
### REL3003 — value cannot be used by this runtime operation

**Status:** Reserved.

Evaluation reached an operation whose runtime operand/value type is unsupported even though parsing succeeded.

**Action:** validate the value before the operation or convert it using a supported REL helper.

## Internal invariants

<a id="rel9001"></a>
### REL9001 — REL runtime invariant violated

**Status:** Reserved.

RBE reached a state that should be impossible after successful parsing/validation. This is likely an RBE defect rather than a normal user-source error.

**Action:** do not repeatedly rewrite valid REL to work around it. Capture the full diagnostic, source role/path, RBE build ID, Runtime Image ID when available, and the smallest reproducer; then report the issue.
