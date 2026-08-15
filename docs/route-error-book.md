# RBE Route Compiler Error Book

This is the human-readable troubleshooting guide for `.route` compiler diagnostics.

The compiler should never only say that something is wrong. It should explain **what was expected, what was found, why the mistake matters, and how to fix it**.

The detailed error output is also written to `data/admin/compiler-error.txt` during boot.

---

## How to read a compiler diagnostic

A route diagnostic is grouped by file:

```text
2 errors, 1 warning in file /api/uac/login.route

############################
|1| :import[net]
|2|
|3| cnst LMAO = net <<<<<<<<<
|4| funciton(LMAO)
############################
    ^^^^
expected `const`
line 3, column 2
```

The parts mean:

- **error/warning count** — how many diagnostics were found in this file.
- **file path** — the source file being compiled.
- **`#####` frame** — the source excerpt associated with the diagnostic.
- **`|N|`** — the source line number.
- **`<<<<`** — the compiler's visual continuation marker on the failing line; it extends toward the right edge of the frame.
- **`^` / `^^^`** — the exact source position or span involved.
- **message** — the short explanation.
- **line/column** — the source location in a stable machine-readable form.

The terminal may color these parts differently, but the underlying meaning must stay the same in plain text, PowerShell, CMD, Linux terminals, CI, and redirected output.

---

# Syntax errors

Syntax errors mean the source cannot be understood as a valid RBE route-language program yet.

## E1001 — unexpected token

### Example

```text
error[E1001]: unexpected token `}` while parsing an expression
```

### What it means

The parser reached a token that cannot legally appear where it currently is. This usually means an earlier token is missing or in the wrong place.

Common causes:

- missing expression before `}`
- missing `)`
- missing `]`
- accidental extra punctuation
- a typo in a keyword

### How to fix

Start at the reported position, then inspect the previous one or two tokens. Parser errors are frequently caused by something **before** the highlighted character.

---

## E1002 — missing `]`

### Example

```text
:import[net
```

Diagnostic:

```text
error[E1002]: expected `]` to close the import
```

### Why it happens

`[` opens a bracketed import expression and `]` closes it. Without the closing bracket, the parser cannot know where the import ends.

### Correct form

```text
:import[net]
```

For multiple imports:

```text
:import[response as resp, net]
```

Do not close the import with `)` or `}`; those belong to different grammar constructs.

---

## E1003 — expected `const`/`let`

### Example

```text
cnst LMAO = net
```

Diagnostic:

```text
error[E1003]: expected `const`
```

### Why it happens

The route language uses explicit declaration keywords. A typo such as `cnst` is not silently interpreted as JavaScript.

### Fix

```text
const LMAO = net;
```

The compiler may offer a typo suggestion when the misspelling is close enough to a known keyword.

---

## E1004 — missing `)`

### Example

```text
function test(req {
```

### Why it happens

A function parameter list starts with `(` and must close with `)` before the body begins.

### Fix

```text
function test(req) {
```

The same rule applies to calls and `if` conditions.

---

## E1005 — missing `}`

### Example

```text
class Route {
    get(req) {
        return {
            ok: true
        };
```

### Why it happens

An opening `{` creates a block/object that must eventually be closed.

### Fix

Check matching pairs from the inside outward:

```text
class Route {
    get(req) {
        return {
            ok: true
        };
    }
}
```

When possible, the compiler should point at the location where the closing token was expected rather than merely blaming end-of-file.

---

## E1006 — missing comma between imports

### Example

```text
:import[response as resp net]
```

### Why it happens

Multiple imports are separated by a comma followed by whitespace.

### Fix

```text
:import[response as resp, net]
```

The comma makes import boundaries unambiguous and keeps the one-line import form easy to scan.

---

## E1007 — trailing comma in imports

### Example

```text
:import[net, json,]
```

### Fix

```text
:import[net, json]
```

Unless the grammar is explicitly expanded to support trailing commas later, a comma must be followed by another import entry.

---

## E1008 — unknown route method

### Example

```text
class Route {
    fetch(req) {
        return {};
    }
}
```

### Why it happens

`.route` files use HTTP verbs as entrypoints. A normal route currently recognizes:

```text
get
post
put
delete
patch
head
options
```

### Fix

If `fetch` is meant to be helper logic, make it a top-level function. If it is meant to be an endpoint, use an accepted HTTP verb.

---

# Semantic errors

A semantic error means the syntax is understandable, but the program's meaning is invalid.

## E2001 — undefined variable

### Example

```text
class Route {
    get(req) {
        return usr.name;
    }
}
```

Diagnostic:

```text
error[E2001]: `usr` is not defined
```

### Why it happens

The parser can understand `usr.name`, but the compiler cannot find a declaration, parameter, or imported capability named `usr`.

### Fix

Declare it, use the correct existing name, or import the capability that supplies it.

For a typo:

```text
user
```

instead of:

```text
usr
```

The compiler should eventually provide a `did you mean ...?` suggestion when the edit distance is small.

---

## E2002 — module used as a value

### Example

```text
:import[net]

class Route {
    get(req) {
        return net;
    }
}
```

### Why it happens

`net` is a capability namespace, not a value. A module capability must be accessed through one of its exported functions or capabilities.

### Fix

```text
:import[net]

class Route {
    get(req) {
        return net.ping();
    }
}
```

---

## E2003 — symbol is not callable

### Example

```text
const value = "hello";
value();
```

### Why it happens

The name resolves to a value, not a function.

### Fix

Call the function that actually produces the value, or remove the parentheses if the value is meant to be returned directly.

---

## E2004 — duplicate declaration

### Example

```text
const value = 1;
const value = 2;
```

### Why it happens

Two local bindings use the same name in the same scope.

### Fix

Rename one binding or remove the duplicate declaration.

---

## E2005 — duplicate import

### Example

```text
:import[net, net]
```

### Why it matters

An import expresses a capability request. Importing the same capability twice is ambiguous and usually signals a mistake.

### Fix

```text
:import[net]
```

For aliases, the source capability is still considered the same capability:

```text
:import[net as network, net as httpNet]
```

should therefore be rejected once duplicate-source checking is implemented.

---

# Capability errors

Capability errors are semantic errors involving the route's allowed built-ins.

## E3001 — capability unavailable in `.route`

### Example

```text
:import[env]
```

### Why it happens

`env` is intentionally a module-only capability. Environment access can expose deployment details or secrets that ordinary routes should not have.

### Fix

Move the operation into `.module` logic and expose only the safe result to the route.

---

## E3002 — advanced capability unavailable in `.route`

A future version may report this when a route requests an operation that belongs exclusively to `.module`.

Example concept:

```text
error[E3002]: advanced request construction is not available in `.route` files
help: move this operation into a `.module` capability
```

This is a deliberate language boundary, not a missing feature by accident.

---

## E3003 — required capability not enabled

Used for capabilities such as `auth` that require a backend feature to be enabled and built before use.

Example:

```text
error[E3003]: capability `auth` is not enabled
help: enable and build the authentication subsystem before importing `auth`
```

The point is to prevent a source file from assuming a backend service exists when the deployment configuration does not provide it.

---

# Warning messages

Warnings should teach without preventing a valid backend from booting.

## W0001 — unused variable

```text
warning[W0001]: local `debug` is never used
```

### Meaning

The compiler found a local variable that contributes nothing to the reachable program.

### What happens

The generated Rust artifact may omit that value entirely when safe.

### Fix

Use it, rename it to an intentionally ignored binding if that syntax is supported, or remove it.

---

## W0002 — unused function

```text
warning[W0002]: function `legacyHelper` is never called
```

### Meaning

No route entrypoint or reachable helper calls the function.

### What happens

The compiler can leave it out of the generated Rust artifact.

### Fix

Call it if it is required, export/use it through the correct future module mechanism, or delete the dead code.

---

## W0003 — unused import

```text
warning[W0003]: import `json` is never used
```

### Fix

Remove the import or actually use the capability.

Removing unused imports also keeps the capability surface of the source file honest and easier to audit.

---

# Why the compiler collects errors

A broken route should not hide unrelated broken routes.

The boot compiler is intended to:

```text
scan every `.route`
    ↓
parse every file that can be recovered
    ↓
collect syntax errors
    ↓
analyze valid ASTs
    ↓
collect semantic/capability errors
    ↓
show every file's diagnostics
    ↓
write errors to compiler-error.txt
    ↓
abort boot only after diagnostics are collected
```

This is especially useful when a deployment has twenty broken routes. Fixing one error should reveal the next real error in the same boot rather than forcing a restart after every edit.

---

# Error recovery rules

The parser should synchronize at safe grammar boundaries rather than skipping an arbitrary amount of source.

Useful synchronization points include:

- `;` — end of a statement
- `}` — end of a block
- `class` — next top-level route class
- `function` — next helper declaration
- `:import` — next import declaration

Recovery should never invent a fake valid AST node merely to make a diagnostic disappear. When a unit cannot be reconstructed safely, it is excluded from semantic analysis and Rust artifact generation.

---

# Fixing errors: a practical workflow

1. Fix the **first syntax error** in the file. Later parser errors can sometimes be consequences of the first mistake.
2. Re-run the compiler and look at the new diagnostic list.
3. Fix semantic errors next: undefined names, duplicate bindings, wrong capability imports, and invalid calls.
4. Treat warnings as cleanup unless they reveal a real logic mistake.
5. Only after the route is clean should you investigate generated Rust or runtime behavior.

The compiler should make this workflow obvious by reporting the source path, source line, column, explanation, and suggested remedy whenever one can be known safely.

---

# Diagnostic quality rules

A good RBE diagnostic should answer four questions:

1. **Where?** — exact file, line, and column.
2. **What?** — the immediate problem in plain language.
3. **Why?** — the language rule that was violated.
4. **How do I fix it?** — a corrected example or actionable suggestion when possible.

The compiler must not claim a guessed reason as fact. When the cause is ambiguous, say that the highlighted token is unexpected and explain which forms were legal at that position.
