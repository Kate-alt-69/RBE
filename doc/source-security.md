# REL / Runtime Image Source Security

RBE treats REL source files as **deployment inputs**, not mutable runtime authority.

## Active source boundary

After RELC links an application, the running backend is pinned to one immutable Runtime Image snapshot. The image contains exact parsed executable programs plus compiler/link metadata.

Current guarantees include:

- Route REL executes from the linked `RouteFile` snapshot rather than reopening the `.route` file per request.
- Module REL resolves from linked `ModuleFile` snapshots.
- Service source is fingerprinted during catalog compilation and checked before child activation.
- Server REL policy/ENV/middleware outputs are resolved into the image before request serving.
- native Route-WASM bytes, SHA-256 identity, ABI/compiler version inputs, and interpreter-fallback reasons are pinned to the image.
- an image source/settings change produces a different image/source identity rather than silently mutating the active program.

This closes the important source time-of-check/time-of-use class of bugs: editing a source file after RELC validation must not change the code already represented by the active image.

## Runtime ENV is not process environment

Uppercase `ENV` is typed Runtime Image configuration. It is resolved once with this precedence:

```text
built-ins
    < normal Server REL env defaults
    < settings.json runtimeEnv
    < forced Server REL env values
```

`ENV` preserves JSON types. It is readable by authorized Module/Service/Server REL and denied to Route REL by default.

The legacy lowercase operating-system-process `env` capability is rejected in RELC-linked applications. Credentials must not be placed in Runtime ENV; secrets belong in Vault.

## Service bootstrap boundary

Service Mother and Service workers do not inherit arbitrary host application environment as their authority source. Their environment is cleared/rebuilt from an RBE-owned allowlist, while runtime data such as the Runtime ENV snapshot and authentication material travels through bounded supervised bootstrap/IPC paths.

The canonical Service executable is a separately linked dependency image. Backend verifies its build-time SHA-256 and refuses an invalid Service binary; it also refuses the historical mistake of treating a byte-identical backend executable as the Service image.

Service-to-service calls use the authenticated loopback Mother Fabric rather than raw peer authority.

## Container capability boundary

Container execution is a separate process boundary. The Container Controller owns authenticated control IPC and persistent artifact/execution metadata; configured Environment identities have their own generations and execution channels, and untrusted WASM is launched in disposable worker processes.

Container capability manifests are deny-by-default and bound to the exact tuple:

```text
Runtime Image
+ SourceId
+ Environment
+ Environment generation
```

A capability grant names a logical capability kind/target/operation and request/response byte limits. It does not expose Service PIDs, raw IPC addresses, Vault credentials, or host handles to the workload. Wildcard capability targets/operations are rejected. Debug/host-file grants are rejected when the Controller is not running with debug authority.

Generation replacement invalidates the old Environment's capability manifests.

## Container sandbox status

On Linux, the execution runtime requires its configured OS isolation controls before claiming secure execution: namespace isolation, `PR_SET_NO_NEW_PRIVS`, seccomp, cgroup-v2 limits, hard wall-time enforcement, and Wasmtime fuel/memory policy are part of the boundary.

On Windows/non-Linux, the portable scheduler/control contracts can run, but the project deliberately does not claim equivalent secure OS sandbox enforcement until a native backend exists.

The Environment filesystem/root isolation layer is still being hardened; do not treat the documented Environment storage budget as a complete hostile-filesystem sandbox by itself.

## HostBootstrap and Vault

On Linux, HostBootstrap runs before normal runtime credential authority is created. It verifies/provisions the Secret Service path needed by the credential runtime and fails normal boot if that prerequisite cannot be established. Debug diagnostics are bounded and production output is intentionally less revealing.

Vault remains the secret boundary. Runtime ENV, Container grants, Service metadata, and error reports are not substitutes for Vault-backed credential storage.

## CONTROL ER recovery authority

Error Reporter has BASIC and CONTROL authority modes. CONTROL authority is issued only after the parent has the required bootstrap authority and is delivered through inherited one-shot bootstrap material rather than command-line/environment/file secrets.

CONTROL ER can return bounded restart/stop/default recovery decisions for the component being supervised. It does **not** gain arbitrary process-spawn authority: Service Mother, backend Container supervision, Vault supervision, and backend ER supervision remain owned by their respective supervisors.

Recovery reports deliberately carry bounded operational metadata rather than request bodies, credentials, Runtime ENV values, database rows, or arbitrary process memory. If CONTROL ER is absent, BASIC, stale, crashed, unauthenticated, or times out, the owning supervisor falls back to its local bounded recovery policy.

## Settings-path hardening

Normal backend boot uses an explicit `--settings <file>` path. Ambient `SETTINGS_PATH` is ignored unless the operator intentionally enables the legacy development compatibility behavior. This prevents an inherited process environment variable from silently selecting the production configuration authority.

## Why RBE does not delete source after boot

Deleting `.route`, `.module`, `.service`, or `server.server` immediately after linking is **not** the security model. It would make normal crash/restart unable to rebuild the Runtime Image, complicate controlled relinking, and provide weak protection against an attacker who already owns enough host access to inspect process memory, deployment artifacts, backups, or executable behavior.

Source deletion is not a substitute for:

- filesystem/OS access control;
- Vault-backed secrets;
- immutable Runtime Image execution;
- source/catalog fingerprints;
- authenticated child/bootstrap IPC;
- Container sandbox/capability policy;
- normal host hardening.

## Planned sealed deployment

The planned source-less production model is a persistent verified Runtime Image artifact (RBI) produced during a trusted build/deploy step. A sealed package can then contain the backend/runtime plus the RBI while omitting raw:

```text
*.route
*.module
*.service
server.server
```

Backend must verify that artifact before activation and execute only its embedded/lowered contents. Development mode can continue compiling directly from source.

Persistent RBI loading/signing is not complete yet, so raw sources must remain available for normal restart today. Do not deploy a script that deletes them after boot.

## Threat-model note

Sealed packaging reduces casual source disclosure and removes an easy post-build source-edit injection path. It cannot make application behavior unknowable to an attacker with full administrator/kernel/debug authority. RBE's real security boundary remains validation, least privilege, capabilities, process/sandbox isolation, authenticated IPC, Vault, and host hardening—not obscurity.
