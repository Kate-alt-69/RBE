# REL host capability status

The public contracts for `workspace`, `script`, and `archive` are now part of Route Engine.

Current implementation boundary:

- symbolic `$$/` ProjectRoot path contract: existing RBE behavior;
- symbolic `??/` temporary Workspace path contract: defined;
- workspace DAG planning and parallel-ready batch calculation: defined and tested;
- script extension/runtime inference: defined and tested;
- `rbe.sys.pypy`: reserved as a first-class managed runtime identity by the script contract;
- archive format/entry/update/extract planning: defined and traversal-safe;
- host request/response envelope: defined;
- trusted runtime hydration through RPX/install-runtime: integration pending;
- REL evaluator dispatch for the three host builtins: integration pending;
- Container one-shot script executor binding: integration pending;
- ZIP execution engine and Route-WASM archive warming: integration pending;
- Server REL helper execution bridge: integration pending;

The pending items must fail closed. No implementation may substitute ambient PATH tools,
`std::process::Command` from REL-facing code, the process current directory, or an arbitrary
OS temporary directory for the missing trusted host authority.
