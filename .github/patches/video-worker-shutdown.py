from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# Replace the small worker module wholesale: it owns the worker lifecycle API.
worker = Path("engine/crates/video-manager/src/worker.rs")
worker.write_text(r'''use std::sync::Arc;
use std::time::Duration;

use crate::{DownloadPolicy, FfmpegPolicy, FfprobePolicy, VideoManager, VideoWorkerState};

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
        self.download.validate()?;
        self.ffprobe.validate()?;
        self.ffmpeg.validate()?;
        Ok(())
    }
}

pub struct VideoWorkerHandle {
    manager: Arc<VideoManager>,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl VideoWorkerHandle {
    /// Stop accepting new work and allow the current pipeline item to finish.
    /// If it does not finish within `timeout`, the task is aborted; startup
    /// recovery will safely re-queue any interrupted job on the next boot.
    pub async fn shutdown(mut self, timeout: Duration) {
        let _ = self.shutdown.send(true);
        match tokio::time::timeout(timeout, &mut self.task).await {
            Ok(Ok(())) => {
                tracing::info!("Video Manager download worker stopped gracefully");
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "Video Manager download worker task ended unexpectedly during shutdown"
                );
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis(),
                    "Video Manager download worker exceeded graceful shutdown budget; aborting task"
                );
                self.task.abort();
                let _ = self.task.await;
            }
        }
        if let Err(error) = self.manager.set_worker_state(VideoWorkerState::Disabled) {
            tracing::error!(error = %error, "Video Manager worker shutdown telemetry failed");
        }
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
    ) -> anyhow::Result<VideoWorkerHandle> {
        policy.validate()?;
        {
            let mut state = self
                .worker_state
                .lock()
                .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))?;
            if *state != VideoWorkerState::Disabled {
                anyhow::bail!("Video Manager download worker is already active");
            }
            *state = VideoWorkerState::Sleeping;
        }

        let manager = self.clone();
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            match self.recover_incomplete_downloads() {
                Ok(0) => {}
                Ok(count) => tracing::warn!(
                    count,
                    "Video Manager re-queued interrupted download job(s) after restart"
                ),
                Err(error) => {
                    let _ = self.set_worker_state(VideoWorkerState::Degraded);
                    tracing::error!(
                        error = %error,
                        "Video Manager failed to complete startup download recovery"
                    );
                }
            }

            loop {
                if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
                    break;
                }

                match self.next_queued_download(None) {
                    Ok(Some(queued)) => {
                        if let Err(error) = self.set_worker_state(VideoWorkerState::Processing) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
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
                        if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
                            break;
                        }
                        if let Err(error) = self.set_worker_state(VideoWorkerState::Sleeping) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
                        continue;
                    }
                    Ok(None) => {
                        if let Err(error) = self.set_worker_state(VideoWorkerState::Sleeping) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
                    }
                    Err(error) => {
                        let _ = self.set_worker_state(VideoWorkerState::Degraded);
                        tracing::error!(
                            error = %error,
                            "Video Manager failed to discover queued download work"
                        );
                    }
                }

                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = self.work_notify.notified() => {}
                    _ = tokio::time::sleep(policy.recovery_scan) => {}
                }
            }

            if let Err(error) = self.set_worker_state(VideoWorkerState::Disabled) {
                tracing::error!(error = %error, "Video Manager worker exit telemetry failed");
            }
        });

        Ok(VideoWorkerHandle {
            manager,
            shutdown,
            task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rbe-video-worker-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn policy(root: &std::path::Path) -> VideoWorkerPolicy {
        let ffprobe = root.join("ffprobe-test");
        let ffmpeg = root.join("ffmpeg-test");
        std::fs::write(&ffprobe, b"test").unwrap();
        std::fs::write(&ffmpeg, b"test").unwrap();
        VideoWorkerPolicy {
            download: DownloadPolicy::default(),
            ffprobe: FfprobePolicy::new(ffprobe),
            ffmpeg: FfmpegPolicy::new(ffmpeg),
            recovery_scan: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn idle_worker_stops_gracefully_and_prevents_duplicate_spawn() {
        let root = temp_root("shutdown");
        let manager = Arc::new(
            VideoManager::open_default(&root.join("video-manager.db"), 7200).unwrap(),
        );
        let worker_policy = policy(&root);
        let handle = manager
            .clone()
            .spawn_download_worker(worker_policy.clone())
            .unwrap();
        let duplicate = manager.clone().spawn_download_worker(worker_policy);
        assert!(duplicate.is_err());
        handle.shutdown(Duration::from_secs(1)).await;
        assert_eq!(
            manager.status().unwrap().download_worker.state,
            VideoWorkerState::Disabled
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
''')

# Re-export the lifecycle handle.
path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "pub use worker::VideoWorkerPolicy;\n",
    "pub use worker::{VideoWorkerHandle, VideoWorkerPolicy};\n",
    "Video worker export",
)
path.write_text(source)

# Backend uses its configured graceful-shutdown budget before forced abort.
path = Path("engine/crates/backend/src/main.rs")
source = path.read_text()
source = replace_once(
    source,
    '''    if let Some(task) = video_worker_task {
        task.abort();
        let _ = task.await;
    }
''',
    '''    if let Some(worker) = video_worker_task {
        worker
            .shutdown(Duration::from_millis(
                config.runtime.graceful_shutdown_timeout_ms.max(1),
            ))
            .await;
    }
''',
    "backend Video Manager worker shutdown",
)
path.write_text(source)
