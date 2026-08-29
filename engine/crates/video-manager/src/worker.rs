use std::sync::Arc;
use std::time::Duration;

use crate::{DownloadPolicy, FfmpegPolicy, FfprobePolicy, VideoManager};

const MAX_RECOVERY_SCAN: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
pub struct VideoWorkerPolicy {
    pub download: DownloadPolicy,
    pub ffprobe: FfprobePolicy,
    pub ffmpeg: FfmpegPolicy,
    pub recovery_scan: Duration,
}

impl VideoWorkerPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.recovery_scan.is_zero() || self.recovery_scan > MAX_RECOVERY_SCAN {
            anyhow::bail!(
                "Video Manager worker recovery scan must be greater than zero and at most {:?}",
                MAX_RECOVERY_SCAN
            );
        }
        self.ffprobe.validate()?;
        self.ffmpeg.validate()?;
        Ok(())
    }
}

impl VideoManager {
    /// Start the mother-owned download worker. The task sleeps when there is no
    /// work and wakes immediately when `queue_download` notifies it. A bounded
    /// recovery scan also catches queued jobs restored after process restart or
    /// inserted through another registered database adapter.
    pub fn spawn_download_worker(
        self: Arc<Self>,
        policy: VideoWorkerPolicy,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        policy.validate()?;
        Ok(tokio::spawn(async move {
            loop {
                match self.next_queued_download(None) {
                    Ok(Some(queued)) => {
                        let asset_id = queued.asset.id.clone();
                        let job_id = queued.job.id.clone();
                        match self
                            .process_queued_download(
                                &queued,
                                policy.download.clone(),
                                &policy.ffprobe,
                                &policy.ffmpeg,
                            )
                            .await
                        {
                            Ok(variant) => tracing::info!(
                                asset_id = %asset_id,
                                job_id = %job_id,
                                variant_id = %variant.id,
                                "Video Manager download pipeline completed"
                            ),
                            Err(error) => tracing::warn!(
                                asset_id = %asset_id,
                                job_id = %job_id,
                                error = %error,
                                "Video Manager download pipeline failed"
                            ),
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::error!(
                        error = %error,
                        "Video Manager failed to discover queued download work"
                    ),
                }

                tokio::select! {
                    _ = self.work_notify.notified() => {}
                    _ = tokio::time::sleep(policy.recovery_scan) => {}
                }
            }
        }))
    }
}
