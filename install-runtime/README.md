# RBE install runtime

`rbe-install-runtime` is the trusted host-side I/O implementation for the source-only package install contracts.

It deliberately sits between registry/network I/O and the deterministic install stack:

```text
registry HTTPS
    ↓
rbe-install-runtime
    ↓ validated RegistryPackageIndex
rbe-library-registry
    ↓
rbe-library-resolver

pinned artifact URL + SHA-256 + size
    ↓
rbe-install-runtime
    ↓ StreamingVerifier / PromotionPlan
rbe-install-executor
```

## Registry transport

Registry package indexes are fetched only from credential-free HTTPS URLs. DNS is resolved before dispatch and private, loopback, link-local, multicast, documentation, carrier-grade NAT, unspecified, and other non-public destinations are rejected. Redirects are handled manually and every redirect target is revalidated and re-resolved.

Registry bodies are bounded independently from dependency solving. The default per-index limit is 2 MiB, the hard maximum is 8 MiB, and recursive metadata hydration is capped at 1024 package indexes by default.

The runtime feeds validated indexes into `rbe-library-registry`; it does not duplicate semver/dependency solving.

## Artifact staging

`stage_artifact()` executes an existing `ArtifactDownloadPlan` from `rbe-install-executor`.

It preserves the plan's contract:

- HTTPS only;
- SSRF-resistant DNS checks;
- bounded redirects;
- disk-space preflight;
- `.part` resume support;
- re-hash an existing partial prefix before reuse;
- require HTTP 206 plus a matching numeric `Content-Range` for resume;
- restart from byte zero when a server ignores Range and policy allows it;
- stream every received chunk through `StreamingVerifier`;
- enforce the pinned artifact size and SHA-256;
- `sync_all()` the verified staging file before returning;
- reject symlinked staging/cache path components.

A successful call returns an `ArtifactStage` containing the `VerifiedDownload` and the executor-owned `PromotionPlan`. **It does not publish the artifact into the final cache itself.** Atomic durable promotion and install-session state transitions remain separate trust boundaries.

This crate does not execute package build commands, hydrate build dependencies, extract archives, or mutate `package.lock.rbe.yaml`.
