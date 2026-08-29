# /module

`/module` is the default recursive source tree for executable `.module` files. It is a sibling of `/api/` under the backend binary/runtime root.

`.module` files are compiled and validated during backend boot. A module compiler error aborts route/API boot rather than waiting for the first request to discover a broken module.

## Imports

Custom module imports support the normal module path forms, including the short binary-root form:

```text
:import[module&storage]
```

and the explicit form:

```text
:import["./module/storage"]
```

Namespace and direct-function imports are supported. Aliases are supported as well.

The module compiler validates missing dependencies, duplicate bindings/sources/functions/exports, export bodies, and circular module dependency graphs.

## Execution

`.module` functions execute through the async `ModuleExecutor` and support local helpers, nested module calls, built-in capabilities, service calls, conditionals, objects/arrays, member access, comparisons/arithmetic, and a bounded call depth.

Only exported functions can be invoked from outside their module. Private helper functions remain callable from functions within the same module.

`.route -> .module` is the normal privileged-boundary pattern. Route execution uses the same async module executor, so a route may call a module which then calls a service or another host capability.

## Services

Modules may explicitly import `.service` interfaces:

```text
:import[service:uac-cache as cache]
:import[service:search.find as lookup]
```

The backend validates service names and direct service exports while loading modules. Unknown services fail boot with `MOD2008`; unknown direct exports fail with `MOD2009`.

Direct `.route -> .service` imports are rejected. Service access is intentionally mediated by `.module`.

## Video Manager

Video Manager is a privileged module capability. It is **not global** and must be explicitly imported.

Supported import names are:

```text
vm
video-manager
```

Examples:

```text
:import[vm]
```

```text
:import[video-manager as media]
```

```text
:import[video-manager.status as videoStatus]
```

There is deliberately no `video` alias. `:import[video]` fails module boot with `MOD2010`.

Video Manager calls are scoped to the canonical module identity. For example, `module/learning/catalog.module` receives the owner `learning.catalog`; asset mutations are pinned to namespace `module:learning.catalog` instead of trusting a caller-supplied owner string.

Direct routes cannot import `vm` or `video-manager` because these names are not route capabilities.

See `docs/video-manager.md` for the media pipeline and available language operations.

## Service-host boundary

The service host also uses module-language execution internally for `.service` bodies, but mother-side privileged host capabilities are not automatically injected into a service worker. In particular, `.service` does not currently receive the mother process's Video Manager capability.

That separation is intentional: service-to-mother privileged capabilities need an explicit authenticated channel rather than an accidental in-process escape hatch.
