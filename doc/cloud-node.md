# RBE Cloud Node

Cloud Node is low-level RBE persistence and disaster-recovery infrastructure. It is deliberately below REL and RELC: language programs cannot read node private keys, choose peers, open node tunnels, mutate topology, or bypass the authenticated recovery protocol.

## Build contract

Every normal packaged RBE build includes `cloud_node` / `cloud_node.exe` beside the backend binary. A node-only refresh is also supported without requiring the backend Control Room password or Container signing key:

```powershell
.\build.ps1 --cloud-node-only --build-win --arch-x64
.\build.ps1 --cloud-node-only --build-linux --arch-x64
```

```bash
./build.sh --cloud-node-only --build-linux --arch-x64
```

A Cloud Node-only build overwrites only the Cloud Node artifact inside `dist/<target>`; it does not delete an existing backend package.

## Node settings

Cloud Node reads `setting.node.cn.json`. Private keys and provider credentials never belong directly in this file. The node's Ed25519 private key is supplied through `RBE_CLOUD_NODE_PRIVATE_KEY`; peer public keys are configuration. Provider secrets are read from environment variables.

A node chooses either an authenticated RBE peer (`upstream`) or a managed/object-storage provider (`provider`). They are mutually exclusive.

### Peer mode

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "mode": "primary",
    "storageRoot": "Z:/",
    "backupVersions": 5,
    "preserveOriginal": true,
    "videoChunkBytes": 4194304
  },
  "upstream": {
    "url": "https://example-backend.invalid",
    "nodeId": "render-main",
    "publicKey": "<64 hex Ed25519 public key>",
    "autoReconnect": true,
    "syncOnConnect": true,
    "reconnectDelayMs": 2000
  },
  "replication": {
    "requireBootRecovery": true,
    "bootRecoveryTimeoutMs": 60000,
    "targets": []
  }
}
```

### Provider mode

Provider mode removes the requirement for a second `cloud_node.exe`. The configured provider stores Cloud Node snapshots, history commits, and the provider HEAD object.

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "mode": "primary",
    "storageRoot": "Z:/"
  },
  "provider": {
    "kind": "amazon-s3",
    "namespace": "production",
    "bucket": "kastrick-rbe-backups",
    "region": "ap-south-1",
    "prefix": "server-a",
    "conflictPolicy": "fail",
    "syncOnConnect": true,
    "autoReconnect": true,
    "reconnectDelayMs": 2000,
    "auth": {
      "mode": "aws-sig-v4"
    }
  }
}
```

Supported provider kinds are currently:

- `amazon-s3` (`aws-s3` / `s3` aliases)
- `supabase`
- `azure-blob` (`azure` alias)
- `google-cloud-storage` (`gcs` / `google-cloud` aliases)
- `http` for an HTTPS object endpoint using the Cloud Node object-key contract

The default credential environment names are intentionally RBE-owned and predictable:

| Provider | Default environment variables |
| --- | --- |
| Amazon S3 | `RBE_CN_PROV_AMAZON_ACCESS_KEY`, `RBE_CN_PROV_AMAZON_SECRET_KEY`, optional `RBE_CN_PROV_AMAZON_SESSION_TOKEN` |
| Supabase | `RBE_CN_PROV_SUPABASE_API_KEY` |
| Azure Blob | `RBE_CN_PROV_AZURE_SAS_TOKEN` |
| Google Cloud Storage | `RBE_CN_PROV_GOOGLE_OAUTH_TOKEN` |
| Generic HTTP | `RBE_CN_PROV_HTTP_BEARER_TOKEN`, or auth-mode-specific variables such as `RBE_CN_PROV_HTTP_API_KEY`, `RBE_CN_PROV_HTTP_USERNAME`, `RBE_CN_PROV_HTTP_PASSWORD`, and `RBE_CN_PROV_HTTP_HEADER_VALUE` |

Environment variable names can be overridden without moving the secret into JSON. For example:

```json
{
  "provider": {
    "kind": "supabase",
    "namespace": "production",
    "bucket": "rbe-backups",
    "endpoint": "https://example.supabase.co",
    "auth": {
      "mode": "api-key",
      "apiKeyEnv": "RBE_CN_PROV_SUPABASE_API_KEY"
    }
  }
}
```

Provider auth modes are `auto`, `none`, `api-key`, `bearer`, `basic`, `header`, `aws-sig-v4`, `azure-sas`, and `oauth-bearer`. Fixed cloud providers accept only the authentication modes that match their actual API. Generic `http` mode can use no auth, API-key headers, Bearer/OAuth tokens, Basic auth, or an arbitrary configured header.

A custom-header example:

```json
{
  "provider": {
    "kind": "http",
    "namespace": "production",
    "bucket": "rbe",
    "endpoint": "https://storage.example.com",
    "auth": {
      "mode": "header",
      "headerName": "x-storage-token",
      "headerValueEnv": "RBE_CN_PROV_MY_STORAGE_TOKEN"
    }
  }
}
```

The JSON contains only the name of the environment variable. The secret value remains outside `setting.node.cn.json`.

`requireBootRecovery` defaults to `false`. When enabled, at least one trusted replication target must be configured. `bootRecoveryTimeoutMs` defaults to 60000 and accepts 1000 through 3600000 milliseconds.

## Provider history and sync direction

Provider mode maintains a local Git-like commit DAG without invoking or depending on `git.exe`. Using an internal history is important because Cloud Node stores large binary/video objects that should not be shoved through Git object storage.

For each provider namespace Cloud Node stores local history under:

```text
<ROOT>/rbe/provider-history/<namespace>/
├── HEAD.json
└── commits/
    └── <commit-sha256>.json
```

The provider stores the corresponding remote history beneath its Cloud Node namespace:

```text
rbe-cn/<namespace>/
├── history/
│   ├── HEAD.json
│   └── commits/<commit-sha256>.json
└── snapshots/<snapshot-root>/...
```

Each history commit points to one canonical Cloud Node snapshot root and an optional parent commit. On connection Cloud Node compares local and provider HEAD ancestry:

```text
same commit          -> no transfer
remote is ancestor   -> LOCAL is ahead -> push
local is ancestor    -> PROVIDER is ahead -> pull
no ancestry          -> diverged
no provider HEAD     -> initial push
```

Divergence fails closed by default. `conflictPolicy` may be set explicitly to `prefer-local` or `prefer-remote` when an operator intentionally wants one side to win. The default `fail` policy avoids silently destroying a valid branch of history.

The history tracks synchronization state and ancestry. Snapshot payloads remain content-addressed Cloud Node objects with SHA-256 validation and recovery staging; the history layer does not weaken payload verification.

Useful provider commands are:

```text
cloud_node evaluate
cloud_node probe-provider
cloud_node provider-status
cloud_node sync-provider
cloud_node run
```

`run` automatically selects provider mode when `provider` is configured, otherwise it uses the existing authenticated peer mode.

## NAS layout

For a configured storage root `<ROOT>` Cloud Node owns:

```text
<ROOT>/rbe/
├── storage/
│   └── <object-sha256>/
│       ├── file.blob.cn | video.blob.cn | folder.blob.cn
│       ├── versions/<content-sha256>/...
│       └── chunks/<chunk-sha256>.chunk       # video objects
├── backup/
│   └── <same-object-sha256>/
│       ├── original/                         # first exact object, preserved
│       ├── latest/                           # latest exact object
│       ├── versions/<content-sha256>/...     # actual rolling revision payloads
│       └── history.blob.cn                   # binary rolling history, default 5
├── provider-history/
│   └── <namespace>/                          # provider sync DAG + local HEAD
└── recovery-staging/
    └── <authenticated-session>/              # private full-snapshot recovery staging
```

The directory SHA is a stable object identity derived from blob kind + normalized logical path. Every exact content revision has its own SHA-256 inside that object. This gives backup and active storage the same stable lookup key while still keeping immutable content generations.

## Binary CN blob formats

`file.blob.cn`, `video.blob.cn`, `folder.blob.cn`, and `history.blob.cn` are binary formats. They are not JSON, UTF-8 documents, or UTF-16 documents. String fields inside a binary record are length-prefixed bytes for cross-platform path identity.

`file.blob.cn` records exact byte-range replacements from the previous content generation. The immutable full payload remains under `versions/<content-sha256>/payload`, so history can reconstruct or verify any retained generation without pretending a database needs database-specific semantics.

`video.blob.cn` uses large SHA-256-addressed chunks (4 MiB by default) so large media revisions can reuse unchanged chunks rather than duplicating a whole video in the active object store. Recovery reconstructs the immutable video generation only after every declared chunk is present and hash-valid.

`folder.blob.cn` is the filesystem topology manifest. During LOCAL -> REMOTE recovery the transfer order is fixed:

```text
folder.blob.cn / folder structure
        ↓
video.blob.cn + requested video chunks
        ↓
file.blob.cn + requested file generations
```

The receiver rejects phase regression, interleaved resource streams, non-contiguous chunk ranges, payloads arriving before their manifest, undeclared video chunks, malformed manifests, and hash/size mismatches.

## Authenticated recovery transport

Cloud Node uses domain-separated Ed25519 challenge signing. The private key is never sent in a ping, challenge, response, sync frame, or configuration file. The binary `RBE-CN/1` frame envelope has distinct message types for authentication, sync negotiation, object transfer, completion, and ping/pong.

Cloud Node authentication additionally has a compact binary `RBECNAU1` proof. A node signs its node id, timestamp, fresh session id, and nonce. The accepting RBE node returns a separately signed proof bound to that exact session and client nonce. Follow-up requests use fresh signed session proofs, so a captured request proof cannot simply be replayed during the session lifetime.

A sync begins by exchanging a `SyncPlanHeader` containing the canonical snapshot root plus folder/video/file counts. If the roots differ, the LOCAL node sends the complete ordered snapshot through bounded object-transfer frames. Every transfer chunk carries its own SHA-256, and the complete resource is verified against its declared resource hash before it can be committed to staging.

Recovery is intentionally full-snapshot rather than merge-based. Stale objects on the REMOTE side must disappear. The receiver writes into `recovery-staging/<session>/storage`, verifies the completed staged tree against the exact root negotiated with the authenticated sender, then swaps that storage tree into the live Cloud Node store. If post-swap verification fails, the previous live storage tree is restored.

## Evaluating-phase boot admission

When `replication.requireBootRecovery` is enabled, the backend's temporary maintenance responder becomes the recovery endpoint while the main RBE process remains in `Evaluating..`.

The main process and maintenance responder use a private parent/child control pipe for boot policy and completion. The responder reports whether Cloud Node recovery is disabled, optional, or required. For required recovery, normal runtime admission waits for a valid completion control message until `bootRecoveryTimeoutMs` expires.

This gate occurs before RBE proceeds into normal service startup. Vault, Container-managed application execution, Service runtime admission, and the normal HTTP router are therefore not treated as ready merely because the maintenance listener is reachable.

A required boot is admitted only when either:

1. the authenticated LOCAL recovery peer proves that the negotiated snapshot already matches the REMOTE snapshot, or
2. a differing snapshot is transferred, fully verified, atomically activated, and the activated root exactly equals the negotiated LOCAL root.

Malformed control messages, an exited maintenance responder, timeout, authentication failure, transfer corruption, or a final root mismatch fail closed instead of silently admitting the normal runtime.

## Current boundary

Cloud Node recovery currently restores the Cloud Node-owned `storage/` snapshot. That is intentionally different from blindly writing recovered logical objects into arbitrary application/runtime filesystem paths. A separate ownership/materialization contract is required before recovered Cloud Node objects can be projected into normal application paths; recovery must not gain an implicit ability to overwrite RBE runtime files.

The storage, authentication, transfer, staging, exact-root activation, provider synchronization, and boot-admission layers remain outside REL capabilities. REL/RELC code cannot directly invoke or weaken this recovery boundary.
