use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{
    DownloadPolicy, FfmpegPolicy, FfprobePolicy, QueuedDownload, VideoManager, VideoWorkerState,
};

const MAX_RECOVERY_SCAN: Duration = Duration::from_secs(60 * 60);
const WORKER_RESTART_BASE_DELAY: Duration = Duration::from_millis(250);
const WORKER_RESTART_MAX_DELAY: Duration = Duration::from_secs(30);
const WORKER_STABLE_WINDOW: Duration = Duration::from_secs(60);

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
    fn recover_worker_database(&self, name: &str) -> anyhow::Result<usize> {
        let (_, database) = self.resolve_database(Some(name))?;
        let mut count = 0usize;
        for queued in database.recover_incomplete_downloads(name)? {
            match self.cleanup_recovered_download_artifacts(&queued.asset.id, &queued.job.id) {
                Ok(()) => count += 1,
                Err(error) => {
                    let detail = format!(
                        "Video Manager could not safely recover interrupted download: {error}"
                    );
                    database.update_job(&queued.job.id, "failed", 0.0, Some(&detail))?;
                    tracing::error!(
                        database = %name,
                        asset_id = %queued.asset.id,
                        job_id = %queued.job.id,
                        error = %error,
                        "Video Manager failed closed while recovering interrupted download"
                    );
                }
            }
        }
        if count > 0 {
            self.work_notify.notify_one();
        }
        Ok(count)
    }

    fn next_recovered_queued_download(
        &self,
        recovered_databases: &HashSet<String>,
    ) -> anyhow::Result<Option<QueuedDownload>> {
        let names = self
            .database_names()?
            .into_iter()
            .filter(|name| recovered_databases.contains(name))
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(None);
        }

        let start = self
            .worker_database_cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % names.len();
        let mut failures = Vec::new();
        for offset in 0..names.len() {
            let name = &names[(start + offset) % names.len()];
            let (_, database) = self.resolve_database(Some(name))?;
            match database.next_queued_download(name) {
                Ok(Some(queued)) => {
                    if !failures.is_empty() {
                        tracing::warn!(
                            failed_databases = ?failures,
                            database = %name,
                            "Video Manager isolated queue discovery failure and continued with recovered database"
                        );
                    }
                    return Ok(Some(queued));
                }
                Ok(None) => {}
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(None)
        } else {
            anyhow::bail!(
                "Video Manager queue discovery failed for recovered database adapter(s): {}",
                failures.join("; ")
            )
        }
    }

    /// Start the mother-owned download worker. The outer task supervises the
    /// processing loop so an unexpected panic/exit degrades telemetry and is
    /// restarted with bounded exponential backoff instead of silently killing
    /// queue processing for the rest of the backend lifetime.
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
        let task_manager = self.clone();
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut restart_attempts = 0u32;
            loop {
                let started_at = Instant::now();
                let mut workers = tokio::task::JoinSet::new();
                workers.spawn(run_download_worker_loop(
                    task_manager.clone(),
                    policy.clone(),
                    shutdown_rx.clone(),
                ));
                let result = workers.join_next().await;
                let uptime = started_at.elapsed();

                if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
                    break;
                }
                if uptime >= WORKER_STABLE_WINDOW {
                    restart_attempts = 0;
                }
                restart_attempts = restart_attempts.saturating_add(1);
                let delay = worker_restart_delay(restart_attempts);
                let _ = task_manager.set_worker_state(VideoWorkerState::Degraded);
                match result {
                    Some(Ok(())) => tracing::warn!(
                        attempt = restart_attempts,
                        uptime_ms = uptime.as_millis(),
                        backoff_ms = delay.as_millis(),
                        "Video Manager download worker exited unexpectedly; scheduling replacement"
                    ),
                    Some(Err(error)) => tracing::error!(
                        attempt = restart_attempts,
                        uptime_ms = uptime.as_millis(),
                        backoff_ms = delay.as_millis(),
                        error = %error,
                        "Video Manager download worker task failed; scheduling replacement"
                    ),
                    None => tracing::error!(
                        attempt = restart_attempts,
                        uptime_ms = uptime.as_millis(),
                        backoff_ms = delay.as_millis(),
                        "Video Manager worker supervisor lost its child task; scheduling replacement"
                    ),
                }

                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }

            if let Err(error) = task_manager.set_worker_state(VideoWorkerState::Disabled) {
                tracing::error!(error = %error, "Video Manager worker supervisor exit telemetry failed");
            }
            if let Err(error) = task_manager.set_worker_encoder(None) {
                tracing::error!(error = %error, "Video Manager worker supervisor encoder cleanup failed");
            }
        });

        Ok(VideoWorkerHandle {
            manager,
            shutdown,
            task,
        })
    }
}

async fn run_download_worker_loop(
    manager: Arc<VideoManager>,
    policy: VideoWorkerPolicy,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut known_databases = HashSet::new();
    let mut recovered_databases = HashSet::new();
    let mut recovery_required = true;

    loop {
        if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
            break;
        }

        let database_names = match manager.database_names() {
            Ok(names) => names,
            Err(error) => {
                let _ = manager.set_worker_state(VideoWorkerState::Degraded);
                tracing::error!(
                    error = %error,
                    "Video Manager could not inspect database registry during worker recovery"
                );
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = manager.work_notify.notified() => {}
                    _ = tokio::time::sleep(policy.recovery_scan) => {
                        recovery_required = true;
                    }
                }
                continue;
            }
        };
        let current_databases = database_names.iter().cloned().collect::<HashSet<_>>();
        recovered_databases.retain(|name| current_databases.contains(name));
        let newly_registered = database_names
            .iter()
            .any(|name| !known_databases.contains(name));
        known_databases = current_databases;

        if recovery_required || newly_registered {
            if database_names.len() == 1 && recovered_databases.is_empty() {
                let name = &database_names[0];
                match manager.recover_incomplete_downloads() {
                    Ok(count) => {
                        recovered_databases.insert(name.clone());
                        if count > 0 {
                            tracing::warn!(
                                database = %name,
                                count,
                                "Video Manager re-queued interrupted download job(s)"
                            );
                        }
                    }
                    Err(error) => tracing::error!(
                        database = %name,
                        error = %error,
                        "Video Manager database recovery failed; this adapter remains blocked until recovery succeeds"
                    ),
                }
            } else {
                for name in &database_names {
                    if recovered_databases.contains(name) {
                        continue;
                    }
                    match manager.recover_worker_database(name) {
                        Ok(0) => {
                            recovered_databases.insert(name.clone());
                        }
                        Ok(count) => {
                            recovered_databases.insert(name.clone());
                            tracing::warn!(
                                database = %name,
                                count,
                                "Video Manager re-queued interrupted download job(s)"
                            );
                        }
                        Err(error) => {
                            tracing::error!(
                                database = %name,
                                error = %error,
                                "Video Manager isolated database recovery failure; this adapter remains blocked until recovery succeeds"
                            );
                        }
                    }
                }
            }
            recovery_required = false;
        }

        let recovery_blocked = database_names
            .iter()
            .any(|name| !recovered_databases.contains(name));
        if recovery_blocked {
            let _ = manager.set_worker_state(VideoWorkerState::Degraded);
        }

        let queued = if recovery_blocked {
            manager.next_recovered_queued_download(&recovered_databases)
        } else {
            manager.next_queued_download(None)
        };
        match queued {
            Ok(Some(queued)) => {
                if let Err(error) = manager.set_worker_state(VideoWorkerState::Processing) {
                    tracing::error!(error = %error, "Video Manager worker telemetry failed");
                }
                let asset_id = queued.asset.id.clone();
                let job_id = queued.job.id.clone();
                match manager
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
                let idle_state = if recovery_blocked {
                    VideoWorkerState::Degraded
                } else {
                    VideoWorkerState::Sleeping
                };
                if let Err(error) = manager.set_worker_state(idle_state) {
                    tracing::error!(error = %error, "Video Manager worker telemetry failed");
                }
                continue;
            }
            Ok(None) => {
                let idle_state = if recovery_blocked {
                    VideoWorkerState::Degraded
                } else {
                    VideoWorkerState::Sleeping
                };
                if let Err(error) = manager.set_worker_state(idle_state) {
                    tracing::error!(error = %error, "Video Manager worker telemetry failed");
                }
            }
            Err(error) => {
                let _ = manager.set_worker_state(VideoWorkerState::Degraded);
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
            _ = manager.work_notify.notified() => {}
            _ = tokio::time::sleep(policy.recovery_scan) => {
                recovery_required = true;
            }
        }
    }
}

fn worker_restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << shift;
    let millis = WORKER_RESTART_BASE_DELAY
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(
        millis
            .saturating_mul(factor)
            .min(WORKER_RESTART_MAX_DELAY.as_millis() as u64),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CreateAssetRequest, DatabaseHealth, VideoAsset, VideoDatabase, VideoJob,
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
        panic_first: bool,
        fail_first: bool,
        fail_forever: bool,
    }

    impl FlakyRecoveryDatabase {
        fn new() -> Self {
            Self {
                recovery_attempts: AtomicUsize::new(0),
                recovery_succeeded: AtomicBool::new(false),
                discovery_before_recovery: AtomicBool::new(false),
                discoveries: AtomicUsize::new(0),
                panic_first: false,
                fail_first: true,
                fail_forever: false,
            }
        }

        fn panicking() -> Self {
            Self {
                panic_first: true,
                ..Self::new()
            }
        }

        fn healthy() -> Self {
            Self {
                fail_first: false,
                ..Self::new()
            }
        }

        fn broken() -> Self {
            Self {
                fail_first: false,
                fail_forever: true,
                ..Self::new()
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
            if attempt == 0 && self.panic_first {
                panic!("intentional recovery panic");
            }
            if self.fail_forever || (attempt == 0 && self.fail_first) {
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
    async fn recovery_failure_blocks_only_that_database_until_retry_succeeds() {
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
            worker_database_cursor: AtomicUsize::new(0),
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

    #[tokio::test]
    async fn broken_recovery_adapter_does_not_block_healthy_database_discovery() {
        let root = temp_root("recovery-isolation");
        let quarantine_root = root.join("quarantine");
        let media_root = root.join("media");
        std::fs::create_dir_all(&quarantine_root).unwrap();
        std::fs::create_dir_all(&media_root).unwrap();
        let healthy = Arc::new(FlakyRecoveryDatabase::healthy());
        let broken = Arc::new(FlakyRecoveryDatabase::broken());
        let mut databases: HashMap<String, Arc<dyn VideoDatabase>> = HashMap::new();
        databases.insert(DEFAULT_DATABASE_NAME.into(), healthy.clone());
        databases.insert("broken".into(), broken.clone());
        let manager = Arc::new(VideoManager {
            databases: RwLock::new(databases),
            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root: std::fs::canonicalize(&quarantine_root).unwrap(),
            media_root: std::fs::canonicalize(&media_root).unwrap(),
            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            worker_database_cursor: AtomicUsize::new(0),
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
            if healthy.discoveries.load(Ordering::SeqCst) > 0
                && broken.recovery_attempts.load(Ordering::SeqCst) > 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(healthy.recovery_succeeded.load(Ordering::SeqCst));
        assert!(healthy.discoveries.load(Ordering::SeqCst) > 0);
        assert!(!healthy.discovery_before_recovery.load(Ordering::SeqCst));
        assert!(!broken.recovery_succeeded.load(Ordering::SeqCst));
        assert!(broken.recovery_attempts.load(Ordering::SeqCst) > 0);
        assert_eq!(broken.discoveries.load(Ordering::SeqCst), 0);
        assert_eq!(
            manager.status().unwrap().download_worker.state,
            VideoWorkerState::Degraded
        );

        handle.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn worker_supervisor_restarts_after_child_panic() {
        let root = temp_root("supervisor-panic");
        let quarantine_root = root.join("quarantine");
        let media_root = root.join("media");
        std::fs::create_dir_all(&quarantine_root).unwrap();
        std::fs::create_dir_all(&media_root).unwrap();
        let database = Arc::new(FlakyRecoveryDatabase::panicking());
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
            worker_database_cursor: AtomicUsize::new(0),
            live_notify: tokio::sync::Notify::new(),
            live_runtime_state: Mutex::new(VideoLiveRuntimeState::Disabled),
            live_runtime_claimed: AtomicBool::new(false),
            live_idle_secs: 7200,
        });
        let handle = manager
            .clone()
            .spawn_download_worker(policy(&root))
            .unwrap();

        for _ in 0..150 {
            if database.recovery_attempts.load(Ordering::SeqCst) >= 2
                && database.discoveries.load(Ordering::SeqCst) > 0
                && manager.status().unwrap().download_worker.state == VideoWorkerState::Sleeping
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(database.recovery_attempts.load(Ordering::SeqCst) >= 2);
        assert!(database.discoveries.load(Ordering::SeqCst) > 0);
        assert_eq!(
            manager.status().unwrap().download_worker.state,
            VideoWorkerState::Sleeping
        );
        handle.shutdown(Duration::from_secs(1)).await;
        let _ = std::fs::remove_dir_all(root);
    }
}
