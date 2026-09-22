# RBE project package state

`package.rbe.yaml` is the human-edited project dependency manifest.
`package.lock.rbe.yaml` is the exact resolver output and the activation boundary used for deterministic restores.

Canonical project package/cache state:

```text
project/
├── package.rbe.yaml
├── package.lock.rbe.yaml
└── .cache/
    ├── library/
    │   ├── <artifact-sha256>/
    │   └── .staging/
    └── rbe/
        ├── sys/
        │   └── {python,nodejs,bunjs,rust}/<version>/<host>/...
        ├── build-deps/
        │   └── <ecosystem>/<lock-sha256>/
        │       └── hydration.rbe.json
        └── install/
            ├── journal.rbe.json
            └── install.lease
```

The lockfile stores the exact resolution required to restore the graph, including pinned artifact URLs and SHA-256 values. If `.cache/library` is deleted, RBE can redownload the locked artifacts without resolving new versions. Deleted bytes still have to be downloaded again.

Cache presence is never proof of integrity. RBE verifies pinned hashes before using cached artifacts, and system-runtime cache entries are also treated as untrusted bytes until their manifest/hash contract is revalidated.

## Managed system runtimes

RBE-owned portable tools use the identities:

```text
rbe.sys.python
rbe.sys.nodejs
rbe.sys.bunjs
rbe.sys.rust
```

They live under `.cache/rbe/sys/...`, do not mutate the machine PATH, and are not automatically exposed as user packages.

## Build dependency hydration

The actual package build remains network-dead. Locked ecosystem dependencies are hydrated first into:

```text
.cache/rbe/build-deps/<ecosystem>/<lock-sha256>/
```

Supported lock contracts currently include:

- Cargo → `Cargo.lock`
- npm → `package-lock.json`
- Bun → `bun.lock`
- Python → hash-locked `requirements.lock`

Hydration uses managed tools only, an explicit HTTPS registry-origin allowlist, a cleared environment, no shell, and scripts disabled where the ecosystem supports that control. A successful hydration records `hydration.rbe.json`; the subsequent build receives the offline cache environment and still has RBE build networking disabled.

## Durable activation

Installation is project-transactional rather than package-by-package activation. The install journal and lease live under `.cache/rbe/install/`.

A candidate graph is prepared and verified first. Only after every required graph member is ready does RBE write `package.lock.rbe.yaml.next` and atomically replace `package.lock.rbe.yaml`. A crash before that swap leaves the previously active graph authoritative while allowing verified cache work to be reused.

See [`../doc/library-system.md`](../doc/library-system.md) for the full package/install architecture and [`../install-executor/README.md`](../install-executor/README.md) for the execution/hydration security contract.
