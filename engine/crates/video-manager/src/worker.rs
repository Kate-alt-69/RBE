use std::sync::Arc;
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
        if let Err(error) = self.manager.set_worker_encoder(None) {
            tracing::error!(error = %error, "Video Manager worker encoder cleanup failed");
        }
    }
}

impl VideoManager {
    fn recover_worker_downloads(&self) -> bool {
        match self.recover_incomplete_downloads() {
            Ok(0) => true,
            Ok(count) => {
                tracing::warn!(count, "Video Manager re-queued interrupted download job(s)");
                true
            }
            Err(error) => {
                let _ = self.set_worker_state(VideoWorkerState::Degraded);
                tracing::error!(
                    error = %error,
                    "Video Manager download recovery failed; worker will not process new jobs until recovery succeeds"
                );
                false
            }
        }
    }

    /// Start the mother-owned download worker. The task sleeps when there is no
    /// work and wakes immediately when `queue_download` notifies it. A bounded
    /// recovery scan retries interrupted-job recovery while the worker is idle.
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
        self.set_worker_encoder(Some(policy.ffmpeg.video_encoder))?;

        let manager = self.clone();
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut recovery_required = true;
            loop {
                if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
                    break;
                }

                if recovery_required {
                    if self.recover_worker_downloads() {
                        recovery_required = false;
                    } else {
                        tokio::select! {
                            changed = shutdown_rx.changed() => {
                                if changed.is_err() || *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                            _ = self.work_notify.notified() => {}
                            _ = tokio::time::sleep(policy.recovery_scan) => {}
                        }
                        continue;
                    }
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
                    _ = tokio::time::sleep(policy.recovery_scan) => {
                        recovery_required = true;
                    }
                }
            }

            if let Err(error) = self.set_worker_state(VideoWorkerState::Disabled) {
                tracing::error!(error = %error, "Video Manager worker exit telemetry failed");
            }
            if let Err(error) = self.set_worker_encoder(None) {
                tracing::error!(error = %error, "Video Manager worker encoder exit cleanup failed");
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
    use crate::{
        CreateAssetRequest, DatabaseHealth, QueuedDownload, VideoAsset, VideoDatabase, VideoJob,
        VideoLiveRuntimeState, VideoVariant, DEFAULT_DATABASE_NAME,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, RwLock};
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
        let manager =
            Arc::new(VideoManager::open_default(root.join("video-manager.db"), 7200).unwrap());
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

    struct FlakyRecoveryDatabase {
        recovery_attempts: AtomicUsize,
        recovery_succeeded: AtomicBool,
        discovery_before_recovery: AtomicBool,
        discoveries: AtomicUsize,
    }

    impl FlakyRecoveryDatabase {
        fn new() -> Self {
            Self {
                recovery_attempts: AtomicUsize::new(0),
                recovery_succeeded: AtomicBool::new(false),
                discovery_before_recovery: AtomicBool::new(false),
                discoveries: AtomicUsize::new(0),
            }
        }
    }

    impl VideoDatabase for FlakyRecoveryDatabase {
        fn kind(&self) -> &'static str {
            "flaky-test"
        }

        fn health(&self) -> DatabaseHealth {
            DatabaseHealth {
                ok: true,
                kind: self.kind().into(),
                detail: None,
            }
        }

        fn create_asset(
            &self,
            _database: &str,
            _request: &CreateAssetRequest,
        ) -> anyhow::Result<VideoAsset> {
            anyhow::bail!("unused test operation")
        }

        fn insert_job(&self, _job: &VideoJob) -> anyhow::Result<()> {
            anyhow::bail!("unused test operation")
        }

        fn claim_job(
            &self,
            _job_id: &str,
            _expected_state: &str,
            _claimed_state: &str,
        ) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unused test operation")
        }

        fn update_job(
            &self,
            _job_id: &str,
            _state: &str,
            _progress: f64,
            _error: Option<&str>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unused test operation")
        }

        fn transition_job(
            &self,
            _job_id: &str,
            _expected_state: &str,
            _next_state: &str,
        ) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unused test operation")
        }

        fn get_job(&self, _job_id: &str) -> anyhow::Result<Option<VideoJob>> {
            Ok(None)
        }

        fn queued_download_count(&self) -> anyhow::Result<u64> {
            Ok(0)
        }

        fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
            if !self.recovery_succeeded.load(Ordering::SeqCst) {
                self.discovery_before_recovery.store(true, Ordering::SeqCst);
            }
            self.discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        fn recover_incomplete_downloads(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<QueuedDownload>> {
            let attempt = self.recovery_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                anyhow::bail!("intentional recovery failure")
            }
            self.recovery_succeeded.store(true, Ordering::SeqCst);
            Ok(Vec::new())
        }

        fn commit_ready_variant(
            &self,
            _job_id: &str,
            _variant: &VideoVariant,
        ) -> anyhow::Result<Option<VideoJob>> {
            Ok(None)
        }

        fn get_asset(
            &self,
            _database: &str,
            _asset_id: &str,
        ) -> anyhow::Result<Option<VideoAsset>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn recovery_failure_blocks_discovery_until_periodic_retry_succeeds() {
        let root = temp_root("recovery");
        let quarantine_root = root.join("quarantine");
        let media_root = root.join("media");
        std::fs::create_dir_all(&quarantine_root).unwrap();
        std::fs::create_dir_all(&media_root).unwrap();
        let database = Arc::new(FlakyRecoveryDatabase::new());
        let mut databases: HashMap<String, Arc<dyn VideoDatabase>> = HashMap::new();
        databases.insert(DEFAULT_DATABASE_NAME.into(), database.clone());
        let manager = Arc::new(VideoManager {
            databases: RwLock::new(databases),
            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root: std::fs::canonicalize(&quarantine_root).unwrap(),
            media_root: std::fs::canonicalize(&media_root).unwrap(),
            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            live_notify: tokio::sync::Notify::new(),
            live_runtime_state: Mutex::new(VideoLiveRuntimeState::Disabled),
            live_runtime_claimed: AtomicBool::new(false),
            live_idle_secs: 7200,
        });
        let mut worker_policy = policy(&root);
        worker_policy.recovery_scan = Duration::from_millis(20);
        let handle = manager
            .clone()
            .spawn_download_worker(worker_policy)
            .unwrap();

        for _ in 0..100 {
            if database.recovery_attempts.load(Ordering::SeqCst) >= 2
                && database.discoveries.load(Ordering::SeqCst) > 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(database.recovery_attempts.load(Ordering::SeqCst) >= 2);
        assert!(database.discoveries.load(Ordering::SeqCst) > 0);
        assert!(!database.discovery_before_recovery.load(Ordering::SeqCst));
        handle.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(root);
    }
}
