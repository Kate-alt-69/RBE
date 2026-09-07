use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    FfmpegPolicy, MediaProbe, QueuedDownload, VideoManager, VideoVariant, PROGRESS_NORMALIZING,
    PROGRESS_PROBED,
};

impl VideoManager {
    /// Normalize a probed quarantined download into the fixed standard profile,
    /// atomically publish its variant metadata, and only then remove quarantine.
    pub async fn normalize_download_media(
        &self,
        queued: &QueuedDownload,
        probe: &MediaProbe,
        policy: &FfmpegPolicy,
    ) -> anyhow::Result<VideoVariant> {
        if queued.asset.id != queued.job.asset_id || queued.job.job_type != "download" {
            anyhow::bail!("Video Manager normalization identity/type mismatch");
        }
        let (width, height, fps) = normalization_video_metadata(probe)?;
        let (_, database) = self.resolve_database(Some(&queued.asset.database))?;
        let transitioned = database
            .transition_job(&queued.job.id, "probed", "normalizing")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Video Manager download job {:?} has not passed FFprobe validation",
                    queued.job.id
                )
            })?;
        if transitioned.asset_id != queued.asset.id || transitioned.job_type != "download" {
            let detail = "Video Manager normalization job does not match its download asset/type";
            let _ = database.update_job(&transitioned.id, "failed", PROGRESS_PROBED, Some(detail));
            anyhow::bail!("{detail}");
        }

        database.update_job(&transitioned.id, "normalizing", PROGRESS_NORMALIZING, None)?;
        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
        let (staging, final_path, stored_path) =
            self.normalized_paths(&transitioned.asset_id, &transitioned.id)?;

        let normalized = match crate::ffmpeg::run_ffmpeg_normalize(&quarantine, &staging, policy)
            .await
        {
            Ok(normalized) => normalized,
            Err(error) => {
                let detail = error.to_string();
                let _ = tokio::fs::remove_file(&staging).await;
                if let Err(state_error) = database.update_job(
                    &transitioned.id,
                    "failed",
                    PROGRESS_NORMALIZING,
                    Some(&detail),
                ) {
                    return Err(anyhow::anyhow!(
                        "Video Manager normalization failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                return Err(error);
            }
        };

        if let Err(error) = promote_normalized_output(&staging, &final_path).await {
            let detail = format!("promote normalized Video Manager output: {error}");
            let _ = tokio::fs::remove_file(&staging).await;
            if let Err(state_error) = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_NORMALIZING,
                Some(&detail),
            ) {
                return Err(anyhow::anyhow!(
                    "{detail}; additionally failed to persist job failure: {state_error}"
                ));
            }
            anyhow::bail!("{detail}");
        }

        let output_bitrate = probe.duration_secs.and_then(|duration| {
            if duration.is_finite() && duration > 0.0 {
                let bits_per_second = (normalized.size_bytes as f64 * 8.0) / duration;
                (bits_per_second.is_finite() && bits_per_second > 0.0)
                    .then(|| bits_per_second.round() as u64)
            } else {
                None
            }
        });
        let now = crate::now_ms();
        let variant = VideoVariant {
            id: Uuid::new_v4().to_string(),
            asset_id: transitioned.asset_id.clone(),
            profile: normalized.profile.to_string(),
            codec: Some(normalized.video_codec.to_string()),
            width: Some(width),
            height: Some(height),
            fps,
            bitrate: output_bitrate,
            size_bytes: normalized.size_bytes,
            path: stored_path,
            state: "ready".into(),
            created_at_ms: now,
            updated_at_ms: now,
        };

        match database.commit_ready_variant(&transitioned.id, &variant) {
            Ok(Some(_)) => {
                let _ = tokio::fs::remove_file(&quarantine).await;
                if let Some(parent) = quarantine.parent() {
                    let _ = tokio::fs::remove_dir(parent).await;
                }
                Ok(variant)
            }
            Ok(None) => {
                let _ = tokio::fs::remove_file(&final_path).await;
                anyhow::bail!(
                    "Video Manager normalization job {:?} lost its normalizing state before commit",
                    transitioned.id
                );
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&final_path).await;
                let detail = error.to_string();
                if let Err(state_error) = database.update_job(
                    &transitioned.id,
                    "failed",
                    PROGRESS_NORMALIZING,
                    Some(&detail),
                ) {
                    return Err(anyhow::anyhow!(
                        "Video Manager normalization commit failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                Err(error)
            }
        }
    }

    fn normalized_paths(
        &self,
        asset_id: &str,
        job_id: &str,
    ) -> anyhow::Result<(PathBuf, PathBuf, String)> {
        crate::validate_generated_uuid("asset id", asset_id)?;
        crate::validate_generated_uuid("job id", job_id)?;
        let asset_dir = self.media_root.join(asset_id);
        std::fs::create_dir_all(&asset_dir).with_context(|| {
            format!(
                "create Video Manager normalized asset directory {}",
                asset_dir.display()
            )
        })?;
        let asset_dir = std::fs::canonicalize(&asset_dir).with_context(|| {
            format!(
                "canonicalize Video Manager normalized asset directory {}",
                asset_dir.display()
            )
        })?;
        if !asset_dir.starts_with(&self.media_root) {
            anyhow::bail!("Video Manager normalized asset directory escaped its storage root");
        }
        let staging = asset_dir.join(format!(".{job_id}.normalizing.mp4"));
        let final_path = asset_dir.join("primary.mp4");
        if staging.exists() || final_path.exists() {
            anyhow::bail!("Video Manager normalized output path already exists");
        }
        Ok((staging, final_path, format!("{asset_id}/primary.mp4")))
    }
}

async fn promote_normalized_output(staging: &Path, final_path: &Path) -> anyhow::Result<()> {
    match tokio::fs::hard_link(staging, final_path).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::remove_file(staging).await {
                let _ = tokio::fs::remove_file(final_path).await;
                return Err(error).context("remove Video Manager staging link after promotion");
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(error).context(
                "Video Manager normalized output already exists; refusing to replace it",
            );
        }
        Err(_) => {
            // Some otherwise valid media roots (for example FAT/exFAT or
            // restricted mounts) do not support hard links. Fall back to an
            // exclusive destination create so publication still cannot clobber
            // a winner from another normalization attempt.
        }
    }

    let source_metadata = tokio::fs::symlink_metadata(staging)
        .await
        .context("inspect Video Manager normalized staging file before promotion")?;
    if !source_metadata.file_type().is_file() || source_metadata.len() == 0 {
        anyhow::bail!("Video Manager normalized staging output is not a non-empty regular file");
    }

    let mut source = tokio::fs::File::open(staging)
        .await
        .context("open Video Manager normalized staging file for promotion")?;
    let mut destination = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(final_path)
        .await
        .context("create exclusive Video Manager normalized output")?;

    let copied = match tokio::io::copy(&mut source, &mut destination).await {
        Ok(copied) => copied,
        Err(error) => {
            drop(destination);
            let _ = tokio::fs::remove_file(final_path).await;
            return Err(error).context("copy Video Manager normalized output during promotion");
        }
    };
    if copied != source_metadata.len() {
        drop(destination);
        let _ = tokio::fs::remove_file(final_path).await;
        anyhow::bail!(
            "Video Manager normalized promotion copied {copied} bytes but staging contained {} bytes",
            source_metadata.len()
        );
    }
    if let Err(error) = destination.flush().await {
        drop(destination);
        let _ = tokio::fs::remove_file(final_path).await;
        return Err(error).context("flush Video Manager normalized output during promotion");
    }
    if let Err(error) = destination.sync_all().await {
        drop(destination);
        let _ = tokio::fs::remove_file(final_path).await;
        return Err(error).context("sync Video Manager normalized output during promotion");
    }
    drop(destination);

    if let Err(error) = tokio::fs::remove_file(staging).await {
        let _ = tokio::fs::remove_file(final_path).await;
        return Err(error).context("remove Video Manager staging file after copied promotion");
    }
    Ok(())
}

fn normalization_video_metadata(probe: &MediaProbe) -> anyhow::Result<(u32, u32, Option<f64>)> {
    let stream = probe.video_streams.first().ok_or_else(|| {
        anyhow::anyhow!("Video Manager normalization probe contains no video stream")
    })?;
    Ok((stream.width, stream.height, stream.frame_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rbe-video-normalization-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn invalid_probe_is_rejected_before_normalization_can_claim_a_job() {
        let probe = MediaProbe {
            format_names: vec!["mp4".into()],
            duration_secs: Some(1.0),
            bit_rate: None,
            video_streams: Vec::new(),
            audio_streams: 0,
        };
        let error = normalization_video_metadata(&probe)
            .expect_err("empty probe must be rejected before state transition");
        assert!(error.to_string().contains("no video stream"));
    }

    #[tokio::test]
    async fn promotion_never_clobbers_an_existing_winner() {
        let root = temp_root("no-clobber");
        let staging = root.join("staging.mp4");
        let final_path = root.join("primary.mp4");
        std::fs::write(&staging, b"candidate").unwrap();
        std::fs::write(&final_path, b"winner").unwrap();

        let error = promote_normalized_output(&staging, &final_path)
            .await
            .expect_err("promotion must refuse an existing final output");
        assert!(error.to_string().contains("refusing to replace"));
        assert_eq!(std::fs::read(&final_path).unwrap(), b"winner");
        assert_eq!(std::fs::read(&staging).unwrap(), b"candidate");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promotion_moves_staging_without_changing_contents() {
        let root = temp_root("publish");
        let staging = root.join("staging.mp4");
        let final_path = root.join("primary.mp4");
        std::fs::write(&staging, b"normalized-video").unwrap();

        promote_normalized_output(&staging, &final_path)
            .await
            .expect("promotion must succeed");
        assert!(!staging.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"normalized-video");
        let _ = std::fs::remove_dir_all(root);
    }
}
