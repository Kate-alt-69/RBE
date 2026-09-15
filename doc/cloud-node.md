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
│   └── outbound/<peer-id>/<sync-root>/       # LOCAL durable transfer spool
└── recovery-staging/
    └── <authenticated-session>/              # REMOTE private recovery staging
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

Recovery is intentionally full-snapshot rather than merge-based. Stale objects on the REMOTE side must disappear. The receiver writes incoming resources to `.part` state inside `recovery-staging/<session>/storage`, verifies hashes and the completed staged tree against the exact root negotiated with the authenticated sender, then swaps that storage tree into the live Cloud Node store. If post-swap verification fails, the previous live storage tree is restored. Content-addressed LOCAL writes likewise use verified `.part` files before final rename, so neither side treats a partially written blob as committed data.

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
