# Workspace examples

## Deployment package flow

A deployment workspace can express ordering without shell scripts:

```rel
:import[workspace]
:import[script]
:import[archive]

function buildDeployment() {
    const flow = workspace.construct();

    const repo = flow.step("repo", workspace.fetch({
        source: "https://github.com/example/project.git",
        to: "??/repo"
    }));

    const package = flow.step("package", script.run("$$/scripts/package.ts"));
    flow.after("package", "repo");

    const archiveStep = flow.step(
        "archive",
        archive.create("??/repo/dist", "??/deployment.zip")
    );
    flow.after("archive", "package");

    return flow.run();
}
```

The executor is expected to schedule all currently-ready operations in parallel.
The example has one serial chain (`repo -> package -> archive`), but unrelated steps
such as docs generation, checksums, or metadata generation may share the same prior
dependency and run concurrently.

## Temporary convenience form

```rel
return workspace.temp(script.run("$$/scripts/inspect.ts"));
```

`workspace.temp(...)` means: allocate an execution-scoped `??/` root, execute the
operation with that root bound, capture bounded output/result metadata, then cleanup.
It does not mean `std::env::temp_dir()` is directly exposed to REL.
