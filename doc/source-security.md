# REL / Runtime Image Source Security

RBE treats REL source files as **deployment inputs**, not mutable runtime authority.

## Implemented runtime boundary

After RELC successfully links an application, execution is pinned to one immutable Runtime Image snapshot.

- Route REL executes the `RouteFile` AST stored in the active Runtime Image.
- Module REL resolves from `ModuleFile` ASTs stored in that same image.
- Service REL source is fingerprinted during catalog compilation and checked again before every child activation; a changed `.service` file is refused.
- Runtime ENV is resolved once and copied into supervised service processes over the bounded inherited parent-bootstrap pipe.
- Uppercase `ENV` is typed Runtime Image configuration. The legacy lowercase process-environment `env` capability is rejected in RELC-linked applications.
- Service child and Service Mother environments are cleared before launch and rebuilt from a small RBE-owned allowlist.
- Normal backend boot uses `--settings <file>`. Ambient `SETTINGS_PATH` is ignored unless the operator explicitly opts into the legacy development compatibility flag `--allow-settings-env`.

These rules close the important time-of-check/time-of-use class of bugs: changing a source file after RELC validates it must not cause different code to execute inside the already-active image.

## Why RBE does not delete source files after normal boot

Deleting `.route`, `.module`, `.service`, or `server.server` immediately after linking is **not** the default security boundary.

It would make a normal crash/restart unable to rebuild the Runtime Image, make controlled reloads impossible, and provide weak protection against an attacker who already has sufficient host access to inspect the process, deployment directory, backups, build artifacts, or executable behavior.

Source deletion is therefore never used as a substitute for:

- filesystem/OS access control;
- Vault-backed secrets;
- immutable Runtime Image execution;
- source/catalog fingerprints;
- authenticated child-process bootstrap;
- normal host hardening.

## Planned sealed deployment mode

For deployments where raw REL source disclosure itself is a concern, the target is **sealed deployment**, not post-boot deletion.

A sealed build will produce a verified Runtime Image artifact (RBI) during the trusted build/deploy step. The production package will contain the backend plus that artifact and will omit raw:

- `*.route`
- `*.module`
- `*.service`
- `server.server`

The backend will verify the artifact before activation and execute only its embedded/lowered Runtime Image data. Development mode will continue to compile directly from source files.

Until persistent RBI loading is implemented, raw source files must remain available for restart. Do not deploy a script that deletes them after boot.

## Threat model note

Sealed packaging reduces casual source disclosure and removes an easy source-edit injection path. It cannot make application behavior unknowable to an attacker with full administrator/kernel/process-debug access to the host. RBE should keep the security boundary in validation, capabilities, process isolation, authenticated IPC, Vault, and least-privilege deployment rather than relying on obscurity alone.
