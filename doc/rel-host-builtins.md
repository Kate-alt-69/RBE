# Host-backed REL builtins

The global names are:

```rel
:import[workspace]
:import[script]
:import[archive]
```

They are host capabilities rather than pure evaluator functions.

## workspace

- `workspace.temp(operation)`
- `workspace.construct()`
- `workspace.fetch(options)`
- `workspace.copy(source, destination)`
- constructed-plan `step`, `after`, and `run`

## script

- `script.run(path)`
- `script.runPyPy(path)` / `script.run_pypy(path)`
- `script.runRust(path)` / `script.run_rust(path)`

## archive

- `archive.list(path)`
- `archive.read(path, entry)`
- `archive.extract(path, entries, destination)`
- `archive.create(source, destination)`
- `archive.replace(path, entry, source)`
- `archive.remove(path, entry)`

Host-backed builtins are unavailable to pure `.field` sources. Module, Service and Route
execution may use them after RELC and the host authority grant them. Server REL may declare
imports through the shared grammar; actual helper execution remains gated by the Server host bridge.
