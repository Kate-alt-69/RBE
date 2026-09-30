# Cloud Node: Kastrick RPX registry on Supabase Storage

This profile mirrors a trusted Kastrick/RPX registry export through the existing Cloud Node provider-history engine and stores the Cloud Node copy in Supabase Storage using Supabase's S3-compatible endpoint.

The live Kastrick publisher remains the registry authority. Cloud Node does not discover or scrape the publisher's private S3 namespace by itself: a trusted registry export must first be materialized and ingested into the Cloud Node store. Once ingested, the registry objects participate in normal Cloud Node provider history and synchronization.

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

`ingest_registry_export()` and the `cloud_node ingest-registry` command accept a **complete frozen registry snapshot**, not an incremental patch. The export may contain these top-level collections:

```text
index/
packages/
ownership/
releases/
history/
analytics/
artifacts/
```

`index/snapshot.json` is mandatory. Cloud Node validates the complete export before changing active registry state. Metadata collections contain JSON only. `artifacts/` may contain JSON metadata and published `.rbe.zip` payloads.

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

Ingest a materialized export without contacting the provider with:

```text
cloud_node --config=setting.node.cn.json ingest-registry <export-root>
```

The command reports `files`, `metadataFiles`, `artifactFiles`, and `removedFiles`, followed by the resulting Cloud Node logical path, object key, and content SHA-256 for each stored object.

Ingestion rejects missing `index/snapshot.json`, symlinks, unsafe paths, duplicate logical paths, unknown collections, non-JSON metadata, non-`.rbe.zip` artifact payloads, and `package.rbe.json` before snapshot replacement begins.

Cloud Node stores accepted files under the logical `registry/` prefix in its normal content-addressed store. After every file in the new export has been stored successfully, active `registry/` file objects absent from the validated snapshot are removed from the active storage tree. Non-registry Cloud Node objects are never touched by this replacement step.

Removal affects only the active registry view used to calculate the next sync root. Cloud Node's `backup/` history for a removed registry object is deliberately retained, so accepting a newer complete registry snapshot does not destroy the node's local historical copies.

That replacement behavior is important: registry ingestion is not an overlay. If a path existed in the previous complete export but is absent from the next one, the next Cloud Node provider snapshot must not continue advertising that stale path.

`package.rbe.json` is deliberately not accepted as registry archive metadata. Published package archives are opaque to Cloud Node. The trusted Kastrick publisher must verify the `.rbe.zip` and its internal `package.rbe.yaml` before exporting a release artifact to Cloud Node.

## One-shot registry provider synchronization

Provider-backed nodes can ingest a trusted export and immediately run normal provider-history synchronization in one command:

```text
cloud_node --config=setting.node.cn.json sync-registry <export-root>
```

`sync-registry` is intentionally a composition of existing Cloud Node behavior, not a second registry protocol. It performs these steps in order:

1. verify that provider mode is configured before changing local registry state;
2. validate the complete frozen export through the same path used by `ingest-registry`;
3. store the new snapshot and remove stale active `registry/` paths that are absent from it;
4. run the normal `synchronize_provider()` transaction, including provider-history locking, ancestry checks, conflict policy, immutable object uploads, and provider HEAD compare-and-swap behavior;
5. report `ingestFiles`, `ingestMetadataFiles`, `ingestArtifactFiles`, `ingestRemovedFiles`, then the normal provider sync relation, action, final head, and final root.

Package publishing must not depend synchronously on this command. A publisher can materialize a frozen export and schedule `sync-registry` independently so a temporary object-provider outage does not make package publication itself unavailable.

If provider synchronization fails after ingestion, the accepted registry snapshot remains in the local Cloud Node store and the command returns an error that states the local ingest completed. Re-running the same frozen export is safe with respect to snapshot identity: Cloud Node's sync root is derived from object kind, logical path, content SHA-256, and logical size rather than manifest timestamps. Replaying byte-identical registry files therefore keeps the same sync root; changing object content or the active path set changes the root and produces the expected new provider snapshot.

Because `sync-registry` uses the configured provider conflict policy, a diverged remote does not get silently overwritten. The default `fail` policy still fails. `prefer-local` and `prefer-remote` retain their existing explicit meanings.

## Revision behavior

Registry data does not get a second synchronization algorithm. It uses Cloud Node's existing provider history after ingestion:

- local and provider heads equal: no-op
- provider head is an ancestor of local: local is ahead, push
- local head is an ancestor of provider: provider is ahead, pull
- unrelated heads: divergence; apply the configured conflict policy

Immutable release artifacts remain content-addressed in Cloud Node and are uploaded through provider synchronization. The registry's own release immutability/yank semantics remain enforced by the Kastrick registry backend; Cloud Node is the durable synchronization/storage layer.

The trusted publisher-to-export materialization step is intentionally separate from Cloud Node's storage engine. That boundary prevents Cloud Node from becoming a second registry authority or requiring access to client-facing RPX credentials.
