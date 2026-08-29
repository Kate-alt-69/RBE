use std::path::PathBuf;

use anyhow::Context;
use uuid::Uuid;

use crate::{FfmpegPolicy, QueuedDownload, VideoManager, VideoVariant};

impl VideoManager {
    /// Normalize a probed quarantined download into the fixed standard profile,
    /// atomically publish its variant metadata, and only then remove quarantine.
    pub async fn normalize_download_media(
        &self,
        queued: &QueuedDownload,
        policy: &FfmpegPolicy,
    ) -> anyhow::Result<VideoVariant> {
        if queued.asset.id != queued.job.asset_id || queued.job.job_type != "download" {
            anyhow::bail!("Video Manager normalization identity/type mismatch");
        }
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
            let _ = database.update_job(&transitioned.id, "failed", 1.0, Some(detail));
            anyhow::bail!("{detail}");
        }

        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
        let (staging, final_path, stored_path) =
            self.normalized_paths(&transitioned.asset_id, &transitioned.id)?;

        let normalized = match crate::ffmpeg::run_ffmpeg_normalize(&quarantine, &staging, policy).await
        {
            Ok(normalized) => normalized,
            Err(error) => {
                let detail = error.to_string();
                let _ = tokio::fs::remove_file(&staging).await;
                if let Err(state_error) =
                    database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))
                {
                    return Err(anyhow::anyhow!(
                        "Video Manager normalization failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                return Err(error);
            }
        };

        if let Err(error) = tokio::fs::rename(&staging, &final_path).await {
            let detail = format!("promote normalized Video Manager output: {error}");
            let _ = tokio::fs::remove_file(&staging).await;
            let _ = tokio::fs::remove_file(&final_path).await;
            if let Err(state_error) =
                database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))
            {
                return Err(anyhow::anyhow!(
                    "{detail}; additionally failed to persist job failure: {state_error}"
                ));
            }
            anyhow::bail!("{detail}");
        }

        let now = crate::now_ms();
        let variant = VideoVariant {
            id: Uuid::new_v4().to_string(),
            asset_id: transitioned.asset_id.clone(),
            profile: normalized.profile.to_string(),
            codec: Some(normalized.video_codec.to_string()),
            width: None,
            height: None,
            fps: None,
            bitrate: None,
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
                if let Err(state_error) =
                    database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))
                {
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
        Ok((
            staging,
            final_path,
            format!("{asset_id}/primary.mp4"),
        ))
    }
}
