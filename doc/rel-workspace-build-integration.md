# RPX hydration boundary

`workspace`, `script`, and `archive` never download language runtimes themselves.

RPX/install-runtime owns managed runtime acquisition and promotion. A future `rpx runtime hydrate`/build integration should resolve the requested `rbe.sys.*` identity through the active registry/custom-domain configuration, fetch over trusted HTTPS, verify the manifest-bound SHA-256 and size, safely extract the archive, and atomically promote the host/version cache entry.

The REL host executor consumes only an already-admitted managed runtime record. Missing runtime state is an installation/hydration error, not permission to search PATH.
