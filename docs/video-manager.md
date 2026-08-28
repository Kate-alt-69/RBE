# RBE Video Manager

Video Manager is RBE's global media identity, metadata, database, and job-control subsystem. The current implementation establishes the durable control/data model first; heavyweight media workers are intentionally not embedded in this crate yet.

## Current responsibilities

The implemented Video Manager currently owns:

- stable video asset identity
- namespaces and groups
- the built-in SQLite database
- pluggable database adapter registration
- asset metadata and state
- variant table/schema
- media job records
- live-session table/schema
- database health/status
- creation of quarantined download jobs without performing network I/O

It does **not** currently execute FFmpeg, FFprobe, RTMP, HLS, WHIP, or remote downloads.

## Stable identity

Assets receive a UUID-backed stable URI:

```text
vm://<namespace-kind>:<namespace-owner>/<group>/<asset-id>
```

Example:

```text
vm://service:kastrick-learning/tutorials/550e8400-e29b-41d4-a716-446655440000
```

The URI is an RBE Video Manager identity, not a filesystem path or network URL.

Namespace kind, owner, group, and custom database names currently accept ASCII letters, digits, `-`, `_`, and `.`.

## Asset model

Implemented source types are:

- `upload`
- `download`
- `local`
- `generated`
- `live`
- `recorded_live`

Implemented asset states are:

- `reserved`
- `quarantined`
- `processing`
- `ready`
- `failed`
- `deleted`

An asset stores its ID, stable URI, selected database, namespace, group, title, source type, optional source URI, JSON metadata, and timestamps.

## Default database

`VideoManager::open_default` registers the built-in SQLite adapter under the name `default`.

SQLite is opened with WAL journaling and foreign-key enforcement. The schema currently contains:

- `video_namespaces`
- `video_groups`
- `video_assets`
- `video_variants`
- `video_jobs`
- `video_live_sessions`

The default adapter supports asset creation, asset lookup, job insertion, and a simple database health probe.

## Database adapters

`VideoDatabase` is the storage adapter contract. Additional adapters can be registered in-process under explicit names.

Database selection is fail-closed: an explicit unknown database override returns an error. Video Manager does not silently fall back to `default` when a caller requested another database.

The language-level/custom adapter surface is not wired yet; the Rust adapter registry is the implemented foundation.

## Download queue

`queue_download` currently accepts only an `https://` source string. It does **not** fetch that URL.

Calling it creates:

1. a `download` asset in `quarantined` state, and
2. a `download` job in `queued` state with zero progress and zero attempts.

This separation is intentional. The future Download Worker is expected to consume queued jobs and perform security-sensitive network/media work outside the database/control-plane call.

The current `https://` prefix check is only a queue-entry constraint, **not** the final download security policy.

## Planned secure Download Worker

The actual worker is still pending. Its responsibilities are expected to include, at minimum:

- URL parsing and scheme policy
- DNS resolution and re-resolution checks
- SSRF protection against loopback, private, link-local, metadata, multicast, and otherwise disallowed destinations
- redirect policy with destination revalidation
- response/body byte limits
- configured maximum download size
- content/magic-byte inspection instead of trusting extensions or `Content-Type`
- quarantine storage
- FFprobe validation
- isolated/sandboxed normalization where required
- explicit job progress/failure transitions

Until that worker exists, a queued download must not be described as downloaded, validated, normalized, or safe for playback.

## FFmpeg / FFprobe

FFmpeg and FFprobe execution are not implemented in Video Manager yet.

The intended architecture is for trusted RBE-owned worker code to launch and supervise these binaries. `.service`, `.module`, and `.route` code should not receive a generic `process.spawn`, `exec`, or shell escape merely to reach FFmpeg.

Hardware-acceleration probing and codec/profile selection are also pending.

## Live media

The schema contains `video_live_sessions`, and status exposes the configured live idle duration, but there is no live media runtime yet.

Current status deliberately reports:

```text
live_runtime = "sleeping"
```

This does not mean an RTMP/HLS server exists in a suspended state. It means heavyweight live workers have not been activated/implemented by this control-plane slice.

Planned live work includes RTMP/HLS and likely WHIP-style ingest where appropriate, with heavyweight workers started lazily and stopped after the configured idle period. The current default configuration target is 7,200 seconds (2 hours).

Node Media Server is not part of the intended architecture; trusted native/RBE-owned workers should own media subprocess and protocol lifecycle.

## Backend integration

When Video Manager is enabled, the backend opens its configured data directory and the built-in `video-manager.db`, then exposes Video Manager status as part of backend `/health`.

The backend currently expects the configured default database name to be the built-in `default`. Runtime registration of additional Rust adapters exists, but selecting a custom configured default through the full backend/language configuration path is not complete yet.

Video Manager health is currently based on the default database health. A failed default database makes the Video Manager status unhealthy and therefore contributes to backend `/health` failure.

## Language integration

A first-class `video` language capability is still pending. Do not assume `.module` or `.service` code can call Video Manager merely because the Rust control plane exists.

The intended language layer should expose capability-scoped operations rather than raw database handles, filesystem paths, or FFmpeg process access. It should preserve namespace ownership and allow an explicit database selection only when that database is registered and authorized.

## Security boundary

Video Manager is designed as the authority that separates untrusted/request-driven media intent from privileged media execution.

Today the implemented boundary is the durable identity/database/job layer. Future network download, probing, transcoding, and live workers must remain behind trusted RBE-owned APIs and must not broaden the route/module/service languages into arbitrary OS process execution.

## Not implemented yet

The following are explicitly pending:

- network download execution
- complete SSRF/DNS/redirect policy
- quarantine file pipeline
- magic-byte/media validation
- FFprobe worker
- FFmpeg transcode/normalization worker
- hardware acceleration detection
- RTMP ingest
- HLS generation/serving
- WHIP/WebRTC ingest
- lazy live-worker activation and the actual 2-hour idle shutdown timer
- language-level `video` capability
- backend-configured custom default database adapters
- upload/local/generated media ingestion workers beyond asset metadata creation

The current implementation should therefore be described as the Video Manager **control/data-plane foundation**, not a finished media server.
