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

Cloud Node reads `setting.node.cn.json`. Private keys never belong in this file. The node's Ed25519 private key is supplied through `RBE_CLOUD_NODE_PRIVATE_KEY`; peer public keys are configuration.

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

`requireBootRecovery` defaults to `false`. When enabled, at least one trusted replication target must be configured. `bootRecoveryTimeoutMs` defaults to 60000 and accepts 1000 through 3600000 milliseconds.

## Provider-backed mode

A Cloud Node can use a supported cloud/object provider instead of an `upstream` Cloud Node for remote persistence, snapshot synchronization, and provider history. `upstream` and `provider` are mutually exclusive: a configuration must choose peer mode or provider mode, never both.

Provider mode currently supports:

- Amazon S3 (`amazon-s3`, with `aws-s3` and `s3` aliases)
- Supabase Storage (`supabase`)
- Azure Blob Storage (`azure-blob`, with `azure` alias)
- Google Cloud Storage (`google-cloud-storage`, with `gcs` and `google-cloud` aliases)
- generic HTTPS object endpoints (`http`)

Provider credentials never belong directly in `setting.node.cn.json`. The JSON selects an authentication mode and may name environment variables; the secret value itself is read only from the process environment.

The built-in credential defaults use the RBE-owned `RBE_CN_PROV_*` namespace:

| Provider/auth | Default environment variable(s) |
| --- | --- |
| Amazon S3 SigV4 | `RBE_CN_PROV_AMAZON_ACCESS_KEY`, `RBE_CN_PROV_AMAZON_SECRET_KEY`, optional `RBE_CN_PROV_AMAZON_SESSION_TOKEN` |
| Supabase | `RBE_CN_PROV_SUPABASE_API_KEY` |
| Azure Blob SAS | `RBE_CN_PROV_AZURE_SAS_TOKEN` |
| Google Cloud Storage OAuth | `RBE_CN_PROV_GOOGLE_OAUTH_TOKEN` |
| Generic HTTP API key | `RBE_CN_PROV_HTTP_API_KEY` |
| Generic HTTP Bearer | `RBE_CN_PROV_HTTP_BEARER_TOKEN` |
| Generic HTTP Basic | `RBE_CN_PROV_HTTP_USERNAME`, `RBE_CN_PROV_HTTP_PASSWORD` |
| Generic HTTP custom header | `RBE_CN_PROV_HTTP_HEADER_VALUE` |

Supported auth modes are `auto`, `none`, `api-key`, `bearer`, `oauth-bearer`, `basic`, `header`, `aws-sig-v4`, and `azure-sas`. Not every mode is valid for every provider. For example, S3 uses SigV4 rather than pretending an AWS access key is a single generic API key.

For credentials that rotate while `cloud_node run` stays online, any provider credential environment value may point at an absolute UTF-8 secret file using the `file:` prefix instead of containing the secret directly. Cloud Node re-opens the file for every provider request, so a sidecar or platform credential agent can atomically replace the token without restarting Cloud Node. Secret files are capped at 64 KiB and trailing CR/LF is ignored. For example:

```text
RBE_CN_PROV_GOOGLE_OAUTH_TOKEN=file:/run/secrets/gcs-oauth-token
```

On Windows the same mechanism accepts an absolute Windows path, for example `file:C:\\ProgramData\\RBE\\gcs-oauth-token.txt`.

The environment variable names can be overridden without putting their values in JSON. For example, a custom provider that expects a proprietary header can use:

```json
{
  "provider": {
    "kind": "http",
    "namespace": "production",
    "bucket": "rbe-data",
    "endpoint": "https://objects.example.invalid",
    "auth": {
      "mode": "header",
      "headerName": "x-company-auth",
      "headerValueEnv": "RBE_CN_PROV_COMPANY_AUTH"
    }
  }
}
```

Provider endpoints must use HTTPS outside exact loopback development hosts. Embedded URL credentials, query strings, and fragments are rejected. Provider namespaces are also validated as safe local history components and cannot be `.` or `..`.

### Amazon S3

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "storageRoot": "Z:/"
  },
  "provider": {
    "kind": "amazon-s3",
    "namespace": "production",
    "bucket": "kastrick-rbe",
    "region": "ap-south-1",
    "auth": {
      "mode": "aws-sig-v4"
    },
    "conflictPolicy": "fail",
    "autoReconnect": true,
    "syncOnConnect": true,
    "reconnectDelayMs": 2000
  }
}
```

Set `RBE_CN_PROV_AMAZON_ACCESS_KEY` and `RBE_CN_PROV_AMAZON_SECRET_KEY`. Temporary AWS credentials may additionally set `RBE_CN_PROV_AMAZON_SESSION_TOKEN`.

### Supabase Storage

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "storageRoot": "Z:/"
  },
  "provider": {
    "kind": "supabase",
    "namespace": "production",
    "bucket": "rbe-cloud-node",
    "endpoint": "https://PROJECT.supabase.co",
    "auth": {
      "mode": "api-key"
    }
  }
}
```

Set `RBE_CN_PROV_SUPABASE_API_KEY`. Provider downloads use the authenticated object route so private buckets remain usable.

### Azure Blob Storage

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "storageRoot": "Z:/"
  },
  "provider": {
    "kind": "azure-blob",
    "namespace": "production",
    "bucket": "rbe-cloud-node",
    "account": "myaccount",
    "auth": {
      "mode": "azure-sas"
    }
  }
}
```

Set `RBE_CN_PROV_AZURE_SAS_TOKEN`. The SAS query is supplied from the environment at request time rather than being stored in the configured endpoint URL.

### Google Cloud Storage

```json
{
  "formatVersion": 1,
  "node": {
    "id": "home-nas",
    "storageRoot": "Z:/"
  },
  "provider": {
    "kind": "google-cloud-storage",
    "namespace": "production",
    "bucket": "kastrick-rbe",
    "auth": {
      "mode": "oauth-bearer"
    }
  }
}
```

Set `RBE_CN_PROV_GOOGLE_OAUTH_TOKEN` to the OAuth Bearer token used for the configured bucket.

### Provider history and synchronization

Provider mode maintains a local commit chain under `provider-history/<namespace>/`. Each history commit identifies a complete Cloud Node snapshot root and its parent commit. The provider stores the immutable snapshot data and history commits plus a mutable `HEAD.json` pointer.

The relation is deterministic:

```text
same head/root       -> no-op
remote is ancestor   -> LOCAL ahead  -> push
local is ancestor    -> REMOTE ahead -> pull
different branches   -> diverged
empty provider       -> push first local snapshot
empty/untracked local + existing provider history -> adopt/pull provider history
```

`conflictPolicy` defaults to `fail`. `prefer-local` explicitly permits a forced provider push and `prefer-remote` explicitly permits a forced provider pull. These policies are intentionally opt-in because silently selecting one side after divergence can destroy valid history.

Before replacing the mutable provider HEAD, Cloud Node re-reads it and verifies that it still matches the remote head used when synchronization was planned. If another writer moved the head, the push fails and must be retried rather than blindly publishing stale history. This is an application-level race guard; it is not advertised as a provider-native atomic compare-and-swap primitive.

Provider pulls reuse the same crash-resumable recovery machinery as authenticated peer recovery. Their staging identity is stable for the provider namespace and expected snapshot root. After process restart, already committed resources are detected and partial resources return their durable `next_offset`, allowing the provider downloader to continue from verified bytes instead of restarting the complete snapshot.

Provider mode removes the requirement for a second `cloud_node.exe` for remote object persistence, snapshot synchronization, and Cloud Node history. Object storage itself is not an arbitrary reverse network tunnel or relay. Provider-managed ingress/relay products can be integrated separately without conflating network tunneling with the persistence provider interface.

Useful provider commands are:

```text
cloud_node evaluate
cloud_node probe-provider
cloud_node provider-status
cloud_node sync-provider
cloud_node run
```

`run` automatically selects the provider loop when `provider` is configured and the peer loop when `upstream` is configured.

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
├── .cache/
│   └── outbound/<peer-id>/<sync-root>/        # frozen LOCAL upload spool; retained on failure
├── provider-history/
│   └── <namespace>/
│       ├── HEAD.json                          # current local provider-history head
│       └── commits/<commit-sha256>.json       # immutable local history commits
└── recovery-staging/
    └── <peer-or-provider-sha256>/<plan-sha256>/
        └── storage/.../*.transfer.part        # fsynced partial resource bytes
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

A sync begins by exchanging a `SyncPlanHeader` containing the canonical snapshot root plus folder/video/file counts. If the roots differ, the LOCAL node first verifies its active store and freezes every transferable manifest, file payload, and video chunk into `.cache/outbound/<peer-id>/<sync-root>/`. Large resources are copied through disk-backed `.part` files, synced, hash/size verified, and renamed into the cache; they are not accumulated in RAM. A `.ready` marker is written only after the complete snapshot has been frozen and the live LOCAL root is rechecked.

Transfers read from that immutable LOCAL cache rather than from live storage. A network or protocol failure deliberately leaves the cache in place. After reauthentication, the same trusted node and same snapshot root can reclaim its REMOTE staging state. Resume acknowledgements report the next verified byte offset, allowing the sender to skip already committed resources or jump over a verified partial prefix instead of retransmitting the entire blob. The acknowledgement extension is opt-in and remains compatible with older peers that return an empty acknowledgement payload.

Recovery is intentionally full-snapshot rather than merge-based. Stale objects on the REMOTE side must disappear. The receiver writes into a staging tree keyed by the authenticated peer plus negotiated plan rather than by the temporary authentication session. Resource bytes are first written to stable `.transfer.part` files and fsynced. On Unix-like hosts, Cloud Node also fsyncs containing directories after durable directory creation, rename, and removal operations so a successful commit covers the filesystem namespace update rather than only file contents. If the backend process restarts, the same authenticated peer negotiating the same plan reopens that staging tree, derives the already committed recovery phase, validates replayed prefix bytes, and continues from the durable partial offset instead of deleting the recovery. A changed plan for the same peer prunes the obsolete private staging tree. The completed staged tree is verified against the exact negotiated root and only then swapped into the live Cloud Node store. If post-swap verification fails, the previous live storage tree is restored.

On the LOCAL sender, `.cache/outbound/<peer>/<sync-root>/` freezes the exact snapshot before transfer. The cache is assembled through `.part` files, survives network failure, and is removed only after the REMOTE proves that exact root was activated. Large blobs stay disk-backed rather than being buffered as whole objects in memory.

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

The storage, authentication, transfer, staging, exact-root activation, and boot-admission layers remain outside REL capabilities. REL/RELC code cannot directly invoke or weaken this recovery boundary.
