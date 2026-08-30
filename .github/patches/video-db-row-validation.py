from pathlib import Path

path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()

# Validate jobs when they cross the SQLite read boundary.
old = '''fn read_video_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<VideoJob> {
    Ok(VideoJob {
        id: row.get(0)?,
        asset_id: row.get(1)?,
        job_type: row.get(2)?,
        state: row.get(3)?,
        progress: row.get(4)?,
        attempts: row.get(5)?,
        error: row.get(6)?,
        created_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

fn validate_job_state(state: &str) -> anyhow::Result<()> {
'''
new = '''fn read_video_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<VideoJob> {
    Ok(VideoJob {
        id: row.get(0)?,
        asset_id: row.get(1)?,
        job_type: row.get(2)?,
        state: row.get(3)?,
        progress: row.get(4)?,
        attempts: row.get(5)?,
        error: row.get(6)?,
        created_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

fn validate_stored_job(job: VideoJob) -> anyhow::Result<VideoJob> {
    validate_generated_uuid("job id", &job.id)?;
    validate_generated_uuid("job asset id", &job.asset_id)?;
    validate_job_state(&job.job_type)?;
    validate_job_state(&job.state)?;
    if !job.progress.is_finite() || !(0.0..=1.0).contains(&job.progress) {
        anyhow::bail!("Video Manager stored job progress must be finite and between 0 and 1");
    }
    if job.created_at_ms < 0 || job.updated_at_ms < job.created_at_ms {
        anyhow::bail!("Video Manager stored job timestamps are invalid");
    }
    if job.job_type == "download"
        && !matches!(
            job.state.as_str(),
            "queued"
                | "downloading"
                | "downloaded"
                | "inspecting"
                | "container_checked"
                | "probing"
                | "probed"
                | "normalizing"
                | "ready"
                | "failed"
        )
    {
        anyhow::bail!("Video Manager stored download job has invalid state {:?}", job.state);
    }
    Ok(job)
}

fn validate_variant_row(variant: &VideoVariant) -> anyhow::Result<()> {
    validate_generated_uuid("variant id", &variant.id)?;
    validate_generated_uuid("variant asset id", &variant.asset_id)?;
    validate_segment("variant profile", &variant.profile)?;
    if let Some(codec) = &variant.codec {
        validate_segment("variant codec", codec)?;
    }
    if variant.width.is_some_and(|value| value == 0)
        || variant.height.is_some_and(|value| value == 0)
    {
        anyhow::bail!("Video Manager stored variant dimensions must be positive");
    }
    if variant
        .fps
        .is_some_and(|fps| !fps.is_finite() || fps <= 0.0 || fps > 1_000.0)
    {
        anyhow::bail!("Video Manager stored variant fps is invalid");
    }
    if variant.size_bytes == 0 {
        anyhow::bail!("Video Manager stored variant size must be positive");
    }
    validate_relative_media_path(&variant.path)?;
    if variant.state != "ready" {
        anyhow::bail!("Video Manager stored variant has invalid state {:?}", variant.state);
    }
    if variant.created_at_ms < 0 || variant.updated_at_ms < variant.created_at_ms {
        anyhow::bail!("Video Manager stored variant timestamps are invalid");
    }
    Ok(())
}

fn validate_job_state(state: &str) -> anyhow::Result<()> {
'''
if old not in source:
    raise SystemExit("read_video_job anchor missing")
source = source.replace(old, new, 1)

# Ensure write path enforces the same basic invariants.
old = '''    fn insert_job(&self, job: &VideoJob) -> anyhow::Result<()> {
        validate_job_state(&job.state)?;
        let connection = self
'''
new = '''    fn insert_job(&self, job: &VideoJob) -> anyhow::Result<()> {
        validate_stored_job(job.clone())?;
        let connection = self
'''
if old not in source:
    raise SystemExit("insert_job anchor missing")
source = source.replace(old, new, 1)

# Validate optional jobs from transaction queries before committing.
for label, old in [
    ("claim_job", '''        let job = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        transaction.commit()?;
        Ok(job)
'''),
    ("transition_job", '''        let job = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        transaction.commit()?;
        Ok(job)
'''),
]:
    new = old.replace('            .optional()?;\n        transaction.commit()?;\n        Ok(job)', '            .optional()?\n            .map(validate_stored_job)\n            .transpose()?;\n        transaction.commit()?;\n        Ok(job)')
    if old not in source:
        raise SystemExit(f"{label} validation anchor missing")
    source = source.replace(old, new, 1)

old = '''        Ok(connection
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?)
'''
new = '''        connection
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?
            .map(validate_stored_job)
            .transpose()
'''
if old not in source:
    raise SystemExit("get_job validation anchor missing")
source = source.replace(old, new, 1)

# commit_ready_variant also reads the current job twice.
needle = '''            .optional()?;
        let Some(job) = job else {
'''
replacement = '''            .optional()?
            .map(validate_stored_job)
            .transpose()?;
        let Some(job) = job else {
'''
if needle not in source:
    raise SystemExit("commit_ready_variant precondition job anchor missing")
source = source.replace(needle, replacement, 1)
needle = '''            .optional()?;
        transaction.commit()?;
        Ok(committed)
'''
replacement = '''            .optional()?
            .map(validate_stored_job)
            .transpose()?;
        transaction.commit()?;
        Ok(committed)
'''
if needle not in source:
    raise SystemExit("commit_ready_variant committed job anchor missing")
source = source.replace(needle, replacement, 1)

# Validate reconstructed variants before returning them.
old = '''                    Ok(VideoVariant {
                        id,
                        asset_id,
                        profile,
                        codec,
                        width: width
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant width is outside u32 range")?,
                        height: height
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant height is outside u32 range")?,
                        fps,
                        bitrate: bitrate
                            .map(u64::try_from)
                            .transpose()
                            .context("Video Manager variant bitrate is negative")?,
                        size_bytes: u64::try_from(size_bytes)
                            .context("Video Manager variant size is negative")?,
                        path,
                        state,
                        created_at_ms,
                        updated_at_ms,
                    })
'''
new = '''                    let variant = VideoVariant {
                        id,
                        asset_id,
                        profile,
                        codec,
                        width: width
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant width is outside u32 range")?,
                        height: height
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant height is outside u32 range")?,
                        fps,
                        bitrate: bitrate
                            .map(u64::try_from)
                            .transpose()
                            .context("Video Manager variant bitrate is negative")?,
                        size_bytes: u64::try_from(size_bytes)
                            .context("Video Manager variant size is negative")?,
                        path,
                        state,
                        created_at_ms,
                        updated_at_ms,
                    };
                    validate_variant_row(&variant)?;
                    Ok(variant)
'''
if old not in source:
    raise SystemExit("list_variants reconstruction anchor missing")
source = source.replace(old, new, 1)

insert = r'''

    #[test]
    fn corrupted_stored_job_and_variant_rows_fail_closed() {
        let path = temp_db("corrupted-media-rows");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "corruption-test".into(),
                group: "rows".into(),
                title: "Corrupt rows".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let connection = Connection::open(&path).unwrap();

        connection
            .execute(
                "UPDATE video_jobs SET progress = 2.0 WHERE id = ?1",
                params![queued.job.id],
            )
            .unwrap();
        let error = manager.get_job(None, &queued.job.id).unwrap_err();
        assert!(error.to_string().contains("progress"));

        connection
            .execute(
                "UPDATE video_jobs SET progress = 0.0, state = 'mystery' WHERE id = ?1",
                params![queued.job.id],
            )
            .unwrap();
        let error = manager.get_job(None, &queued.job.id).unwrap_err();
        assert!(error.to_string().contains("invalid state"));

        let variant_id = Uuid::new_v4().to_string();
        connection
            .execute(
                "INSERT INTO video_variants (id, asset_id, profile, codec, width, height, fps, bitrate, size_bytes, path, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, 'standard', 'h264', 1920, 1080, 30.0, 4000000, 10, '../escape.mp4', 'ready', 1, 1)",
                params![variant_id, queued.asset.id],
            )
            .unwrap();
        let error = manager.list_variants(None, &queued.asset.id).unwrap_err();
        assert!(error.to_string().contains("media path"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
'''
end = source.rfind("\n}")
if end < 0:
    raise SystemExit("video-manager tests tail missing")
source = source[:end] + insert + source[end:]

path.write_text(source)
