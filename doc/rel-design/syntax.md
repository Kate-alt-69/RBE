# Proposed REL syntax and types

Status: design proposal from the [roadmap](../rel-roadmap.md), not the current
grammar reference. All examples below are proposed syntax. Existing grammar is
documented in [REL](../rel.md) and the source-role guides.

## Function declarations

The requested declaration spellings are `function` and `funct`, with `async`
before either one for asynchronous functions. No `*` is required. In JavaScript,
`*` denotes a generator, not an ordinary async function; REL should not conflate
those two concepts.

```text
function [total(items: Item[])], num() {
	return items.reduce((sum, item) => sum + item.price, 0);
}

async funct [loadUser(id: string)], User() {
	return await users.find(id);
}

funct [health()] {
	return {ok: true};
}
```

Proposed interpretation: brackets enclose the function name and parameter list.
The optional comma clause is a return-type marker; `num()` and `User()` do not
invoke constructors or arbitrary code. An async annotation describes the resolved
value, not a second wrapper the author has to write.

`Item`, `User`, `users`, `reduce`, arrows and `await` in these examples illustrate
the design together; they do not establish that the current evaluator implements
each construct. `funct` is a proposed alias, not confirmed current syntax.

Keep existing `function total(items) { ... }` programs valid. Both declaration
forms should lower to the same function representation and share validation.
Do not interpret existing declarations differently when a language level changes.

Recommendation: keep `async` a declaration prefix. Use `=` for assignment and
`=>` for arrow expressions. The discussed `async => function` and
`async = function` alternatives would need separate grammar and diagnostics; they
are not selected by this proposal.

## Decisions needed before a parser change

| Question | Proposed direction / unresolved detail |
| --- | --- |
| What does `num` mean? | Preserve current numeric behavior initially; specify integer/float precision and safe wire conversion before adding separate numeric types |
| Can a type have arguments? | Begin with explicit type markers; define generic/union/nullable notation separately rather than treating type markers as executable calls |
| Are annotations optional? | Infer ordinary locals and return values; validate annotations when present; define exported API checks explicitly |
| Are `number`, `num` and other spellings aliases? | Decide one documented canonical name and intentional aliases; do not silently create several incompatible numeric types |
| What if a value and type share a name? | Type-position lookup must be deterministic and may never execute the value |
| What is an async call's runtime value? | Settle with the task design; preserve existing async declarations while introducing explicit wait semantics deliberately |
| How do formatter/editor tools understand this? | Publish grammar fixtures, source spans and formatting rules in the same change |

## Collection foundation

Add collection operations as one coherent model before more surface shorthand:

- Indexing and mutation: define missing index behavior, bounds, key types and
  whether strings index bytes, code points or another unit.
- `for...of`: deterministic order, operation charging, and limits even for empty
  or nested loops; decide whether iteration observes mutation or a snapshot.
- Closures and arrows: explicit capture/lifetime rules, bounded retained state,
  and no capability escalation through a captured function reference.
- `map`, `filter`, `reduce`: document that eager results allocate; offer lazy
  iterators for large inputs and bounded parallel mapping for async work.
- Destructuring/spread: evaluate each source once; specify missing values,
  duplicate keys, shallow versus deep copies and allocation costs.
- Optional access and null defaults: `??` must not replace `0`, `false` or `""`.
- Python-style slices: specify negative indexes, omitted ends, clipping, step
  handling and Unicode behavior; avoid an accidental full copy for every view.

## Static facilities and errors

Optional types, interfaces and readonly data should catch errors without adding a
runtime object for every annotation. Readonly must state whether it is shallow or
deep. Compile-time constants must be pure: no environment secrets, network calls
or mutable filesystem reads embedded during compilation.

Static methods and associated helpers should work without constructing instances.
Exhaustive matching can make state machines safer. Typed result/error values can
support predictable failure handling, but must fit REL's existing diagnostic model
rather than introduce a parallel incompatible exception system.

Avoid choosing a large class/inheritance framework as a prerequisite for a small
function-based backend. These features can be delivered independently.

## Implementation and acceptance

SYN-01 inventories the lexer/parser, module/service evaluators, FieldManager,
RELC and native subset before claiming anything is missing. SYN-02 implements the
chosen aliases/annotations. SYN-03 implements collections and closures. SYN-04
adds convenience syntax after the base semantics are stable.

Acceptance checks include old program fixtures, both function spellings,
diagnostics for malformed brackets/types, annotation mismatches, evaluation-order
tests, recursion/iteration budgets and capability rejection. New syntax must either
lower natively with equivalent behavior or report an explicit interpreter fallback.
Pure `.field` helpers receive no new I/O or nondeterministic powers.
