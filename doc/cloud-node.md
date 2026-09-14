# RBE Cloud Node

Cloud Node is low-level RBE persistence infrastructure. It is deliberately below REL and RELC: language programs cannot read node private keys, choose peers, open node tunnels, or mutate topology.

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
    "publicKey": "<64 hex Ed25519 public key>",
    "autoReconnect": true,
    "syncOnConnect": true
  },
  "replication": {
    "targets": []
  }
}
```

## NAS layout

For a configured storage root `<ROOT>` Cloud Node owns:

```text
<ROOT>/rbe/
├── storage/
│   └── <object-sha256>/
│       ├── file.blob.cn | video.blob.cn | folder.blob.cn
│       ├── versions/<content-sha256>/...
│       └── chunks/<chunk-sha256>.chunk       # video objects
└── backup/
    └── <same-object-sha256>/
        ├── original/                         # first exact object, preserved
        ├── latest/                           # latest exact object
        └── history.blob.cn                   # binary rolling history, default 5
```

The directory SHA is a stable object identity derived from blob kind + normalized logical path. Every exact content revision has its own SHA-256 inside that object. This gives backup and active storage the same stable lookup key while still keeping immutable content generations.

## Binary CN blob formats

`file.blob.cn`, `video.blob.cn`, `folder.blob.cn`, and `history.blob.cn` are binary formats. They are not JSON, UTF-8 documents, or UTF-16 documents. String fields inside a binary record are length-prefixed bytes for cross-platform path identity.

`file.blob.cn` records exact byte-range replacements from the previous content generation. The immutable full payload remains under `versions/<content-sha256>/payload`, so history can reconstruct or verify any retained generation without pretending a database needs database-specific semantics.

`video.blob.cn` uses large SHA-256-addressed chunks (4 MiB by default) so large media revisions can reuse unchanged chunks rather than duplicating a whole video in the active object store.

`folder.blob.cn` is the filesystem topology manifest. During LOCAL -> REMOTE recovery the sync planner must send/validate data in this order:

```text
folder.blob.cn / folder structure
        ↓
video.blob.cn + requested video chunks
        ↓
file.blob.cn + requested file generations
```

That ordering is part of the Cloud Node recovery contract and is intended to run while the RBE main node is in its `Evaluating..` phase before normal runtime admission.

## Authentication foundation

Cloud Node uses domain-separated Ed25519 challenge signing. The private key is never sent in a ping, challenge, response, sync frame, or configuration file. The binary `RBE-CN/1` frame envelope already reserves distinct message types for Hello, challenge/response, sync negotiation, folder manifests, object requests/chunks, completion, and ping/pong.

The storage/format/identity foundation in the Cloud Node crate intentionally does not expose any REL capability. The authenticated remote tunnel and backend Evaluating-phase sync admission are the next transport layer built on this contract.
