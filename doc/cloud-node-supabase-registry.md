# Cloud Node: Kastrick RPX registry on Supabase Storage

This profile mirrors Kastrick/RPX registry state through the existing Cloud Node provider-history engine and stores the provider copy in Supabase Storage using Supabase's S3-compatible endpoint.

## Why the S3 transport is used

Supabase Storage exposes an AWS Signature V4-compatible S3 endpoint. Cloud Node already has a bounded, streaming S3 transport with immutable-object creation and provider revision history, so the registry integration reuses that transport instead of introducing a separate upload implementation.

Use the example at:

`engine/crates/cloud-node/examples/supabase-registry.setting.node.cn.example.json`

Replace only the non-secret project values:

- `endpoint`: the direct Supabase Storage S3 endpoint, normally `https://<project-ref>.storage.supabase.co/storage/v1/s3`
- `region`: the project region shown in Storage S3 configuration
- `bucket`: the Storage bucket used for the registry; the example uses `rbe-registry`

The provider is intentionally configured as `kind: "s3"`. Supabase is the backing service, while the wire protocol Cloud Node speaks is S3/SigV4.

## Credentials

Do not put live credentials in `setting.node.cn.json` or commit them to Git.

The Supabase S3 profile reads:

```text
RBE_CN_PROV_SUPABASE_ACCESS_KEY
RBE_CN_PROV_SUPABASE_SECRET_KEY
```

The Kastrick server may separately use these environment names for Supabase REST/database operations when configured:

```text
RBE_CN_PROV_SUPABASE_URL
RBE_CN_PROV_SUPABASE_API_KEY
RBE_CN_PROV_SUPABASE_SERVICE_KEY
```

Those are not substitutes for the S3 access-key pair. S3 access keys are server-side credentials with broad Storage access and must stay on trusted backend surfaces.

## Registry export contract

`ingest_registry_export()` accepts a trusted registry export directory containing these top-level collections:

```text
index/
packages/
ownership/
releases/
history/
analytics/
artifacts/
```

Metadata collections contain JSON only. `artifacts/` may contain JSON metadata and published `.rbe.zip` payloads.

A typical export is:

```text
index/
  snapshot.json
packages/
  demo.json
ownership/
  demo.json
releases/
  demo/
    1.0.0.json
history/
  demo/
    publish-<event>.json
analytics/
  demo/
    snapshot-<revision>.json
artifacts/
  demo/
    1.0.0/
      metadata.json
      demo.rbe.zip
```

Cloud Node stores these under the logical `registry/` prefix in its normal content-addressed store. That means package metadata, index snapshots/revisions, ownership records, release history, analytics, artifact metadata, and `.rbe.zip` artifacts all participate in the same provider snapshot.

`package.rbe.json` is deliberately not accepted as registry archive metadata. Published package archives are opaque to Cloud Node. The trusted Kastrick publisher must verify the `.rbe.zip` and its internal `package.rbe.yaml` before exporting a release artifact to Cloud Node.

## Revision behavior

Registry data does not get a second synchronization algorithm. It uses Cloud Node's existing provider history:

- local and provider heads equal: no-op
- provider head is an ancestor of local: local is ahead, push
- local head is an ancestor of provider: provider is ahead, pull
- unrelated heads: divergence; apply the configured conflict policy

Immutable release artifacts remain content-addressed in Cloud Node and are uploaded through provider synchronization. The registry's own release immutability/yank semantics remain enforced by the Kastrick registry backend; Cloud Node is the durable synchronization/storage layer.
