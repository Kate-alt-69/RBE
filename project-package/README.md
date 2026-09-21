# RBE project package state

`package.rbe.yaml` is the human-edited project dependency manifest.
`package.lock.rbe.yaml` is the exact resolver output used for deterministic restores.

Canonical project cache paths:

```text
project/
├── package.rbe.yaml
├── package.lock.rbe.yaml
└── .cache/
    ├── library/<artifact-sha256>/
    └── rbe/
        └── sys/{python,nodejs,bunjs,rust}/...
```

The lockfile stores exact artifact URLs and SHA-256 values so a deleted
`.cache/library` can be rehydrated without re-resolving versions. Deleted bytes
still have to be downloaded again.

Cache presence is never proof of integrity. RBE must verify the pinned hashes
before activating a cached artifact.
