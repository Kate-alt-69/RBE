# RBE Video Manager

Video Manager is RBE's global media identity, metadata, quarantine, validation, normalization, job-control, and live-session control subsystem. Privileged network/media/process work stays in trusted Rust; REL receives a narrow ownership-scoped control surface.

## Module REL capability

Video Manager is currently a privileged **Module REL** capability. Import it explicitly:

```text
:import[vm]
:import[video-manager as media]
:import[video-manager.status as videoStatus]
```

The legacy name `video` is intentionally not an alias. Route REL cannot import Video Manager directly; put privileged media operations behind a Module REL interface.

Language code does not receive raw database handles, filesystem paths, FFmpeg command lines, or process-spawn primitives.

## Stable asset identity

Assets use generated IDs and an RBE-owned URI shape such as:

```text
vm://module:learning.catalog/tutorials/<asset-id>
```

The URI is a Video Manager identity, not an exposed local path. Module ownership is part of the namespace, and lookups/jobs/variants/live sessions are checked against the calling module's ownership before data is returned.

## Current language API

The current Module facade includes:

```text
status()
databaseHealth([database])
get(assetId[, database])
job(jobId[, database])
variants(assetId[, database])
create(group, title, sourceType[, sourceUri[, metadata[, database]]])
queueDownload(group, title, url[, metadata[, database]])
reserveLive(assetId[, database])
liveSession(sessionId[, database])
endLive(sessionId[, database])
```

Snake-case aliases exist for several multi-word functions.

Public job/variant/session views deliberately omit privileged worker diagnostics, storage paths, and trusted live transport endpoints.

## Download pipeline

Remote downloads enter quarantine rather than going directly to a player/FFmpeg shell path.

The implemented high-level flow is:

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

A failed trusted stage records failure instead of promoting unvalidated bytes as ready media.

The pipeline includes:

- strict URL parsing/normalization;
- DNS resolution and rejection of private/loopback/link-local/special targets;
- redirect revalidation and HTTPS downgrade protection;
- bounded streaming download into quarantine;
- container-signature preflight;
- trusted FFprobe validation;
- trusted FFmpeg normalization;
- atomic metadata promotion of the normalized variant/asset/job;
- quarantine cleanup after successful promotion;
- lazy mother-owned queued-job scheduling and startup recovery.

`create(..., "download")` is rejected; remote downloads must use `queueDownload()` so they cannot bypass quarantine.

## FFprobe/FFmpeg boundary

Configured FFprobe/FFmpeg executables are trusted runtime inputs. Relative configured paths are resolved by RBE; Module REL does not supply executable names or arbitrary switches.

Normalization produces the controlled standard MP4 profile with H.264 video, AAC audio, `yuv420p`, first video/optional first audio stream, subtitles/data excluded, local/data protocol restrictions, stdin disabled, bounded logs/timeouts, and no overwrite of an existing output.

## Hardware video encoding — implemented with fallback

At worker startup, RBE probes FFmpeg capabilities and performs bounded smoke verification for supported hardware H.264 candidates. The backend chooses the first verified typed hardware encoder when available; otherwise it uses `libx264`.

Typed hardware candidates currently include:

- NVIDIA NVENC (`h264_nvenc`);
- Intel Quick Sync (`h264_qsv`);
- AMD AMF (`h264_amf`);
- Apple VideoToolbox (`h264_videotoolbox`);
- Windows Media Foundation (`h264_mf`).

The selected encoder becomes part of trusted `FfmpegPolicy`, not a language-provided switch. If a selected hardware encode fails during normalization, RBE removes the failed output and retries **once** with software `libx264`. If that fallback also fails, the normalization fails normally.

Normalized media metadata records which typed encoder actually succeeded.

## Probe metadata

Validated media probing carries useful stream metadata such as width, height, frame rate, and estimated bitrate into the ready variant metadata where available. Values are bounded/validated before the asset becomes ready.

## Live session control — implemented control plane

Video Manager now has a persisted live-session state machine:

```text
reserved -> starting -> live -> stopping -> ended
        \       \        \
         +------ failed ---+
```

Current manager/language operations can reserve a session, query it, and request its end. Duplicate/invalid transitions are rejected by the state machine rather than silently rewriting state.

Trusted Rust owns transport binding. A trusted runtime can bind:

- RTMP/RTMPS ingest;
- WHIP over HTTPS;
- optional HTTPS playback endpoint.

Endpoint URLs are validated, and credentials/fragments are rejected from these bindings. Module REL **does not receive the ingest/playback endpoint strings** through the public live-session view and cannot self-promote a reservation into a trusted active transport.

### Important live-media limit

The live **control plane/state/binding contract** is implemented. Do not infer from that that RBE already contains a complete production RTMP/HLS/WHIP media server, transcoder, CDN, or WebRTC stack. Actual live transport/runtime workers and serving behavior remain separate implementation work where not already wired by trusted callers.

## Database model

The built-in default adapter uses SQLite with WAL/foreign-key enforcement. Video Manager stores namespace/group/asset/variant/job/live-session metadata and resolves database adapters by explicit name.

Unknown database adapters fail closed instead of silently falling back to `default`.

Ready promotion is transactional at the metadata layer: the normalized variant, asset-ready transition, and job-ready transition are committed together. If promotion fails, the staged/promoted output is cleaned rather than leaving metadata that claims a broken asset is ready.

## Lazy worker behavior

The download worker is event-driven rather than a hot polling loop:

1. `queueDownload()` persists the quarantined asset/job;
2. the manager wakes the worker;
3. the worker claims queued work and runs the trusted pipeline;
4. it sleeps again when the queue is empty;
5. bounded recovery scans catch interrupted/restored work.

Worker health/degraded state contributes to backend health reporting when the Video Manager worker is enabled.

## Security model

Video Manager's security boundary is based on ownership and trusted Rust mediation:

- Module identity scopes mutable/read operations;
- remote input begins quarantined;
- URL/DNS/redirect targets are revalidated;
- media bytes must pass signature + FFprobe checks before normalization;
- REL cannot supply shell strings or arbitrary FFmpeg flags;
- internal error strings/path details are not returned in public job views;
- storage paths are stripped from public variant views;
- live transport binding is trusted-Rust-only.

## Still incomplete

Video Manager should not yet be described as a complete media platform. Remaining work includes parts of:

- production live transport/runtime execution and serving;
- HLS/output serving/generation;
- broader upload/local/generated ingestion workers;
- richer normalization/profile/variant policy;
- complete backend-configured custom default adapter behavior;
- a Service-to-Mother Video Manager capability channel if Service REL is later allowed to use VM directly.
