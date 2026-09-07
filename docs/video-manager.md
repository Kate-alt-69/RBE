# RBE Video Manager

Video Manager is RBE's global media identity, storage metadata, job-control, quarantine, validation, normalization, and lazy-worker subsystem. Trusted Rust owns privileged network/media workers; the RBE language surface only receives capability-scoped control-plane operations.

## Current pipeline

The download path implements this state flow:

```text
queued
  -> downloading
  -> downloaded
  -> inspecting
  -> container_checked
  -> probing
  -> probed
  -> normalizing
  -> ready
```

A failure at a privileged worker stage records the job as `failed`. Downloaded bytes remain quarantined until all validation stages pass and the normalized variant is committed.

The implemented pipeline includes:

- strict URL parsing and normalization
- DNS resolution with special/private-address rejection
- redirect revalidation
- bounded HTTPS download execution
- quarantine storage under generated UUID paths
- cheap fail-closed container signature inspection
- trusted FFprobe execution with bounded output and timeout
- trusted FFmpeg normalization with a fixed local-only profile
- atomic database promotion of the normalized variant, asset, and job
- cleanup of quarantine after successful promotion
- mother-owned lazy background scheduling for queued downloads
- startup recovery for interrupted download/validation/normalization jobs
- worker state and aggregate queue-depth reporting
- bounded graceful worker shutdown with safe restart recovery fallback

RTMP, HLS, WHIP/WebRTC ingest, hardware-acceleration selection, and additional ingestion/normalization profiles are still pending.

## Stable identity

Assets receive a UUID-backed stable URI:

```text
vm://<namespace-kind>:<namespace-owner>/<group>/<asset-id>
```

Example:

```text
vm://module:learning.catalog/tutorials/550e8400-e29b-41d4-a716-446655440000
```

The URI is a Video Manager identity, not a filesystem path or network URL. Absolute media storage paths are not exposed through the language API. Normalized variants store controlled relative media paths internally such as:

```text
<asset-id>/primary.mp4
```

That relative storage path is also stripped from the module-facing variant view.

## Asset, job, and variant model

Implemented source types:

- `upload`
- `download`
- `local`
- `generated`
- `live`
- `recorded_live`

Implemented asset states:

- `reserved`
- `quarantined`
- `processing`
- `ready`
- `failed`
- `deleted`

Download jobs expose stage progress. Current aggregate milestones are approximately:

- `queued`: `0.00`
- `downloading`: `0.05`
- `downloaded`: `0.45`
- `container_checked`: `0.55`
- `probed`: `0.70`
- `normalizing`: `0.75`
- `ready`: `1.00`

These are pipeline-stage progress values, not byte-perfect FFmpeg progress percentages.

The normalized download path currently produces a `standard` MP4 variant using H.264 video and AAC audio. The FFmpeg profile is owned by trusted Rust and is not supplied as arbitrary language/process arguments.

## Default database

`VideoManager::open_default` registers the built-in SQLite adapter as `default` and creates both quarantine and normalized-media roots adjacent to the database data directory.

SQLite uses WAL journaling and foreign-key enforcement. The schema contains:

- `video_namespaces`
- `video_groups`
- `video_assets`
- `video_variants`
- `video_jobs`
- `video_live_sessions`

The adapter supports asset creation/lookup, job lookup, queued-job discovery, aggregate queue depth, startup recovery, atomic job claiming/transitions, variant listing, and atomic ready-variant commit.

Final normalized promotion is transactional at the metadata layer: the ready variant is inserted, the asset changes from `quarantined` to `ready`, and the job changes from `normalizing` to `ready` in one SQLite transaction. If that commit fails, the newly promoted media file is removed instead of leaving a falsely-ready asset.

## Database adapters

`VideoDatabase` is the in-process storage adapter contract. Additional adapters can be registered under explicit names.

Database selection is fail-closed. Requesting an unknown adapter returns an error; Video Manager does not silently fall back to `default`.

Worker-oriented adapter hooks have safe no-work defaults, so a custom adapter must explicitly implement queued-job discovery/recovery if it wants the mother-owned download worker to consume its jobs.

The full backend-configured custom-default-adapter path is not complete yet.

## Lazy download worker

The backend can start one mother-owned download worker. It is deliberately event-driven rather than a hot polling loop:

1. `queueDownload()` persists the quarantined asset/job.
2. Video Manager calls its internal `Notify` wakeup.
3. the sleeping worker discovers the oldest queued download and runs the trusted pipeline.
4. when the queue is empty it returns to `sleeping`.
5. a bounded periodic recovery scan catches work restored after restart or inserted through another adapter/process.

A second worker spawn is rejected so two consumers cannot accidentally race the same queue through one `VideoManager` instance.

Worker state is reported as:

- `disabled`
- `sleeping`
- `processing`
- `degraded`

`degraded` contributes to Video Manager `/health` failure. Status also reports aggregate queued-download depth.

### Worker configuration

The relevant `videoManager` settings are:

```json
{
  "downloadWorkerEnabled": false,
  "ffprobeExecutable": "",
  "ffmpegExecutable": "",
  "workerRecoveryScanSecs": 30,
  "downloadMaxBytes": 8589934592
}
```

The worker is disabled by default. When enabled, FFprobe and FFmpeg must be explicitly configured. Relative executable paths are resolved against the backend binary/runtime root; Video Manager does not search the shell `PATH` and does not accept executable paths from module/route input.

At backend shutdown the worker first receives a graceful stop signal. An in-flight pipeline item may finish within `runtime.gracefulShutdownTimeoutMs`. If it exceeds that budget the task is aborted; the next startup's recovery pass re-queues the interrupted job. Retried downloads identity-check and truncate their reserved quarantine file before writing, so stale partial bytes do not contaminate the retry.

## Download security boundary

`queue_download` does not hand arbitrary URLs directly to FFmpeg or a shell. It creates a quarantined asset/job and normalizes the target first.

The trusted download worker then applies network policy including:

- HTTP target normalization
- credentials/fragment rejection
- DNS answer vetting
- rejection of loopback, private, link-local, multicast, metadata/special-use, and mapped disallowed addresses
- mixed public/private DNS answer rejection
- redirect destination revalidation
- HTTPS downgrade prevention
- response size limits and fail-closed `Content-Length` handling
- bounded body streaming into the reserved quarantine file
- reserved-file identity checks before truncation/write

A completed network fetch is only `downloaded`; it is not yet a playable/ready asset.

## Container inspection

After download, `inspect_download_container` claims `downloaded -> inspecting` and runs a cheap signature gate before expensive probing.

The gate recognizes supported video-container families such as ISO-BMFF/MP4 and MPEG transport streams while rejecting obvious image/audio/FLV/random payloads. Failure deletes the quarantined bytes and records the job failure.

This signature gate is intentionally not treated as authoritative media decoding. FFprobe is the next required stage.

## FFprobe

`probe_download_media` claims `container_checked -> probing` and launches only the configured trusted FFprobe executable.

The FFprobe policy requires an absolute regular-file executable path after runtime-path resolution and bounds both timeout and captured output. The invocation:

- reads only the quarantined local file
- restricts allowed protocols to local/data-oriented protocols
- captures bounded JSON output
- rejects malformed output
- requires at least one valid video stream
- validates dimensions and frame-rate ranges

Success records `probed`. Failure deletes quarantine and records the job as failed.

## FFmpeg normalization

`normalize_download_media` claims `probed -> normalizing` and invokes the configured trusted FFmpeg executable through `FfmpegPolicy`.

The current standard profile is deliberately fixed by RBE:

- MP4 output
- H.264 via `libx264`
- AAC audio when present
- `yuv420p`
- CRF 23
- medium preset
- fast-start MP4 metadata
- first video stream and optional first audio stream
- subtitles/data streams excluded
- local/data protocol whitelist only
- stdin disabled
- output overwrite disabled
- bounded FFmpeg error output
- bounded execution timeout

Module/service code does **not** receive arbitrary FFmpeg switches, shell strings, process spawning, input paths, or output paths.

FFmpeg writes into a private staging file under the generated asset directory. Only after FFmpeg succeeds is that staging file renamed to the final `primary.mp4`; the DB ready-variant transaction then publishes it. Quarantine is removed after that transaction succeeds.

At worker boot, RBE parses the configured FFmpeg encoder table and requires the software `libx264` and AAC encoders used by the standard profile. Advertised H.264 hardware candidates are recorded from a strict RBE allowlist. Directly testable candidates (currently NVENC, Quick Sync, AMF, VideoToolbox, and Media Foundation) then receive a bounded one-frame synthetic smoke test; only encoders that actually initialize and encode successfully are reported as verified. Device-bound candidates such as VAAPI, V4L2 M2M, and RKMPP remain advertised-only until explicit device handling lands.

Hardware acceleration is still not selected for real normalization yet; the current profile intentionally uses deterministic `libx264`. The verified candidate set is the safety foundation for automatic selection rather than trusting FFmpeg's advertised encoder list.

## Language API

Video Manager is a privileged **module-only** capability and must be imported explicitly. There is no global `video` object and `video` is not a compatibility alias.

Supported import names are:

```text
vm
video-manager
```

Examples:

```text
:import[vm]
```

or:

```text
:import[video-manager as media]
```

Direct function imports are also supported, for example:

```text
:import[video-manager.status as videoStatus]
```

Current language functions:

- `status()`
- `databaseHealth([database])` / `database_health([database])`
- `get(assetId[, database])`
- `job(jobId[, database])`
- `variants(assetId[, database])`
- `create(group, title, sourceType[, sourceUri[, metadata[, database]]])`
- `queueDownload(group, title, url[, metadata[, database]])`
- `queue_download(...)`

`job()` returns ownership-scoped job state/progress/attempt metadata but deliberately omits the internal error string, which may contain privileged worker or filesystem diagnostics. `variants()` also strips internal storage paths before returning variant information to module code.

Using `:import[video]` fails module boot with `MOD2010` and directs the developer to `vm` or `video-manager`.

`.route` files cannot directly import `vm` or `video-manager`; privileged Video Manager access must go through a `.module`. Host capabilities are import-gated, so merely having the backend capability object present does not create a global language binding.

Mutating calls are namespace-scoped to the resolved module identity. A module such as `module/learning/catalog.module` operates in namespace `module:learning.catalog` and cannot claim another module/service namespace. `get`, `job`, and `variants` enforce ownership before returning asset-derived data.

The generic `create(..., "download")` path is rejected; modules must use `queueDownload()` so remote content enters the quarantine pipeline.

## Backend integration

When enabled, the backend owns one `VideoManager` in shared application state. `/health` includes database health plus download-worker state/queue depth. An unhealthy default database or a degraded download worker contributes to backend health failure.

The language layer receives a narrow `VideoLanguage` facade rather than raw manager/database/filesystem objects.

The worker is optional. With `downloadWorkerEnabled = false`, queueing remains available but jobs stay quarantined/queued until a trusted worker is enabled or another trusted Rust caller processes them.

## Live media

The schema contains `video_live_sessions`, and status includes the configured live idle duration, but a live protocol runtime has not landed yet. Current `live_runtime = "sleeping"` means no heavyweight live worker is active; it should not be interpreted as an implemented RTMP/HLS server merely suspended in memory.

Planned live work includes lazy worker activation, RTMP/HLS and potentially WHIP/WebRTC ingest, with worker shutdown after the configured idle period.

## Still pending

The following remain intentionally unfinished:

- automatic selection/use of verified FFmpeg hardware encoders
- additional normalization profiles/variants
- upload/local/generated media ingestion workers beyond metadata creation
- RTMP ingest
- HLS generation/serving
- WHIP/WebRTC ingest
- lazy live-worker activation and actual idle shutdown
- backend-configured custom default database adapters
- service-to-mother Video Manager capability channel

The download path now includes both the secure media pipeline and its lazy mother-owned scheduler, but Video Manager is not yet a complete live/media server.
