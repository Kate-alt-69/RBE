from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Video Manager: queue discovery, restart recovery, Notify wakeups, worker.
# ---------------------------------------------------------------------------
path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(source, "mod pipeline;\n", "mod pipeline;\nmod worker;\n", "worker module declaration")
source = replace_once(
    source,
    "pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};\n",
    "pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};\npub use worker::VideoWorkerPolicy;\n",
    "worker public export",
)

trait_anchor = '''    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>>;
    fn commit_ready_variant(
'''
trait_new = '''    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>>;
    fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        Ok(None)
    }
    fn recover_incomplete_downloads(
        &self,
        _database: &str,
    ) -> anyhow::Result<Vec<QueuedDownload>> {
        Ok(Vec::new())
    }
    fn commit_ready_variant(
'''
source = replace_once(source, trait_anchor, trait_new, "VideoDatabase worker hooks")

impl_anchor = '''    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>> {
'''
worker_db_methods = r'''    fn next_queued_download(&self, database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        let pair = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
            connection
                .query_row(
                    "SELECT id, asset_id FROM video_jobs WHERE job_type = 'download' AND state = 'queued' ORDER BY created_at_ms ASC, id ASC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
        };
        let Some((job_id, asset_id)) = pair else {
            return Ok(None);
        };
        let job = self
            .get_job(&job_id)?
            .ok_or_else(|| anyhow::anyhow!("Video Manager queued job {job_id:?} disappeared"))?;
        let asset = self.get_asset(database, &asset_id)?.ok_or_else(|| {
            anyhow::anyhow!("Video Manager queued asset {asset_id:?} disappeared")
        })?;
        Ok(Some(QueuedDownload { asset, job }))
    }

    fn recover_incomplete_downloads(
        &self,
        database: &str,
    ) -> anyhow::Result<Vec<QueuedDownload>> {
        let pairs = {
            let now = now_ms();
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
            let transaction = connection.unchecked_transaction()?;
            let mut statement = transaction.prepare(
                "SELECT id, asset_id FROM video_jobs WHERE job_type = 'download' AND state IN ('downloading', 'downloaded', 'inspecting', 'container_checked', 'probing', 'probed', 'normalizing') ORDER BY created_at_ms ASC, id ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.execute(
                "UPDATE video_jobs SET state = 'queued', progress = 0.0, error = NULL, updated_at_ms = ?1 WHERE job_type = 'download' AND state IN ('downloading', 'downloaded', 'inspecting', 'container_checked', 'probing', 'probed', 'normalizing')",
                params![now],
            )?;
            transaction.commit()?;
            rows
        };

        let mut recovered = Vec::with_capacity(pairs.len());
        for (job_id, asset_id) in pairs {
            let job = self.get_job(&job_id)?.ok_or_else(|| {
                anyhow::anyhow!("Video Manager recovered job {job_id:?} disappeared")
            })?;
            let asset = self.get_asset(database, &asset_id)?.ok_or_else(|| {
                anyhow::anyhow!("Video Manager recovered asset {asset_id:?} disappeared")
            })?;
            recovered.push(QueuedDownload { asset, job });
        }
        Ok(recovered)
    }

'''
if impl_anchor not in source:
    raise SystemExit("Sqlite commit method anchor missing")
source = source.replace(impl_anchor, worker_db_methods + impl_anchor, 1)

manager_fields = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    live_idle_secs: u64,
}
'''
manager_fields_new = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    work_notify: tokio::sync::Notify,
    live_idle_secs: u64,
}
'''
source = replace_once(source, manager_fields, manager_fields_new, "VideoManager worker notify field")
source = replace_once(
    source,
    '''            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root,
            media_root,
            live_idle_secs,
''',
    '''            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root,
            media_root,
            work_notify: tokio::sync::Notify::new(),
            live_idle_secs,
''',
    "VideoManager worker notify initializer",
)
source = replace_once(
    source,
    '''        if let Err(error) = database.insert_job(&job) {
            let _ = std::fs::remove_file(&quarantine_path);
            return Err(error);
        }
        Ok(QueuedDownload { asset, job })
''',
    '''        if let Err(error) = database.insert_job(&job) {
            let _ = std::fs::remove_file(&quarantine_path);
            return Err(error);
        }
        self.work_notify.notify_one();
        Ok(QueuedDownload { asset, job })
''',
    "queue download worker notification",
)

manager_insert = '''    pub fn get_asset(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Option<VideoAsset>> {
'''
manager_methods = r'''    fn database_names(&self) -> anyhow::Result<Vec<String>> {
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let mut names = databases.keys().cloned().collect::<Vec<_>>();
        names.sort_by_key(|name| (name != &self.default_database, name.clone()));
        Ok(names)
    }

    fn next_queued_download(
        &self,
        requested_database: Option<&str>,
    ) -> anyhow::Result<Option<QueuedDownload>> {
        let names = match requested_database {
            Some(name) => vec![name.to_string()],
            None => self.database_names()?,
        };
        for name in names {
            let (_, database) = self.resolve_database(Some(&name))?;
            if let Some(queued) = database.next_queued_download(&name)? {
                return Ok(Some(queued));
            }
        }
        Ok(None)
    }

    fn recover_incomplete_downloads(&self) -> anyhow::Result<usize> {
        let mut count = 0usize;
        for name in self.database_names()? {
            let (_, database) = self.resolve_database(Some(&name))?;
            for queued in database.recover_incomplete_downloads(&name)? {
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
        }
        if count > 0 {
            self.work_notify.notify_one();
        }
        Ok(count)
    }

    fn cleanup_recovered_download_artifacts(
        &self,
        asset_id: &str,
        job_id: &str,
    ) -> anyhow::Result<()> {
        validate_generated_uuid("asset id", asset_id)?;
        validate_generated_uuid("job id", job_id)?;
        let asset_dir = self.media_root.join(asset_id);
        let metadata = match std::fs::symlink_metadata(&asset_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_dir() {
            anyhow::bail!("Video Manager recovery asset path is not a real directory");
        }
        let canonical = std::fs::canonicalize(&asset_dir)?;
        if !canonical.starts_with(&self.media_root) {
            anyhow::bail!("Video Manager recovery asset directory escaped its storage root");
        }
        for path in [
            canonical.join(format!(".{job_id}.normalizing.mp4")),
            canonical.join("primary.mp4"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "remove stale Video Manager recovery artifact {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        let _ = std::fs::remove_dir(&canonical);
        Ok(())
    }

'''
if manager_insert not in source:
    raise SystemExit("VideoManager get_asset insertion anchor missing")
source = source.replace(manager_insert, manager_methods + manager_insert, 1)

# Add recovery/discovery regression tests.
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Video Manager test module tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn queued_download_discovery_returns_oldest_waiting_job() {
        let path = temp_db("worker-discovery");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let first = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "queue".into(),
                title: "First".into(),
                url: "https://example.invalid/first.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let _second = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "queue".into(),
                title: "Second".into(),
                url: "https://example.invalid/second.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let discovered = manager.next_queued_download(None).unwrap().unwrap();
        assert_eq!(discovered.job.id, first.job.id);
        assert_eq!(discovered.asset.id, first.asset.id);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn interrupted_downloads_requeue_and_remove_stale_normalization_output() {
        let path = temp_db("worker-recovery");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "recovery".into(),
                title: "Interrupted".into(),
                url: "https://example.invalid/interrupted.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let (_, database) = manager.resolve_database(None).unwrap();
        database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        for (from, to) in [
            ("downloading", "downloaded"),
            ("downloaded", "inspecting"),
            ("inspecting", "container_checked"),
            ("container_checked", "probing"),
            ("probing", "probed"),
            ("probed", "normalizing"),
        ] {
            database
                .transition_job(&queued.job.id, from, to)
                .unwrap()
                .unwrap();
        }
        let asset_dir = manager.media_root.join(&queued.asset.id);
        std::fs::create_dir_all(&asset_dir).unwrap();
        let staging = asset_dir.join(format!(".{}.normalizing.mp4", queued.job.id));
        let final_path = asset_dir.join("primary.mp4");
        std::fs::write(&staging, b"partial").unwrap();
        std::fs::write(&final_path, b"uncommitted").unwrap();

        assert_eq!(manager.recover_incomplete_downloads().unwrap(), 1);
        let recovered = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(recovered.state, "queued");
        assert_eq!(recovered.progress, 0.0);
        assert!(!staging.exists());
        assert!(!final_path.exists());
        assert_eq!(
            manager.next_queued_download(None).unwrap().unwrap().job.id,
            queued.job.id
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# Worker implementation: event-driven sleep + startup crash recovery.
Path("engine/crates/video-manager/src/worker.rs").write_text(r'''use std::sync::Arc;
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
        self.download.validate()?;
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
            match self.recover_incomplete_downloads() {
                Ok(0) => {}
                Ok(count) => tracing::warn!(
                    count,
                    "Video Manager re-queued interrupted download job(s) after restart"
                ),
                Err(error) => tracing::error!(
                    error = %error,
                    "Video Manager failed to complete startup download recovery"
                ),
            }

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
''')

# Policy validation must be available to the trusted worker module.
for file_name, old in [
    ("engine/crates/video-manager/src/download_worker.rs", "    fn validate(&self) -> anyhow::Result<()> {"),
    ("engine/crates/video-manager/src/ffprobe.rs", "    fn validate(&self) -> anyhow::Result<()> {"),
    ("engine/crates/video-manager/src/ffmpeg.rs", "    fn validate(&self) -> anyhow::Result<()> {"),
]:
    p = Path(file_name)
    text = p.read_text()
    text = replace_once(text, old, "    pub(crate) fn validate(&self) -> anyhow::Result<()> {", f"policy validate visibility {file_name}")
    p.write_text(text)

# ---------------------------------------------------------------------------
# Typed config for trusted worker executables.
# ---------------------------------------------------------------------------
path = Path("engine/crates/config/src/lib.rs")
source = path.read_text()
config_struct = '''pub struct VideoManagerConfig {
    pub enabled: bool,
    pub data_dir: String,
    pub default_database: String,
    pub live_idle_secs: u64,
    pub download_max_bytes: u64,
}
'''
config_struct_new = '''pub struct VideoManagerConfig {
    pub enabled: bool,
    pub data_dir: String,
    pub default_database: String,
    pub live_idle_secs: u64,
    pub download_max_bytes: u64,
    pub download_worker_enabled: bool,
    pub ffprobe_executable: String,
    pub ffmpeg_executable: String,
    pub worker_recovery_scan_secs: u64,
}
'''
source = replace_once(source, config_struct, config_struct_new, "VideoManagerConfig fields")
source = replace_once(
    source,
    '''            live_idle_secs: 2 * 60 * 60,
            download_max_bytes: 8 * 1024 * 1024 * 1024,
''',
    '''            live_idle_secs: 2 * 60 * 60,
            download_max_bytes: 8 * 1024 * 1024 * 1024,
            download_worker_enabled: false,
            ffprobe_executable: String::new(),
            ffmpeg_executable: String::new(),
            worker_recovery_scan_secs: 30,
''',
    "VideoManagerConfig defaults",
)
validate_anchor = '''            if self.video_manager.live_idle_secs == 0 {
                return Err(ConfigError::Invalid(
                    "videoManager.liveIdleSecs must be greater than zero".into(),
                ));
            }
'''
validate_new = '''            if self.video_manager.live_idle_secs == 0 {
                return Err(ConfigError::Invalid(
                    "videoManager.liveIdleSecs must be greater than zero".into(),
                ));
            }
            if self.video_manager.download_max_bytes == 0 {
                return Err(ConfigError::Invalid(
                    "videoManager.downloadMaxBytes must be greater than zero".into(),
                ));
            }
            if self.video_manager.worker_recovery_scan_secs == 0
                || self.video_manager.worker_recovery_scan_secs > 3600
            {
                return Err(ConfigError::Invalid(
                    "videoManager.workerRecoveryScanSecs must be between 1 and 3600".into(),
                ));
            }
            if self.video_manager.download_worker_enabled
                && (self.video_manager.ffprobe_executable.trim().is_empty()
                    || self.video_manager.ffmpeg_executable.trim().is_empty())
            {
                return Err(ConfigError::Invalid(
                    "videoManager download worker requires ffprobeExecutable and ffmpegExecutable"
                        .into(),
                ));
            }
'''
source = replace_once(source, validate_anchor, validate_new, "Video Manager config validation")
source = replace_once(
    source,
    '''        assert_eq!(config.video_manager.live_idle_secs, 7200);
''',
    '''        assert_eq!(config.video_manager.live_idle_secs, 7200);
        assert!(!config.video_manager.download_worker_enabled);
        assert_eq!(config.video_manager.worker_recovery_scan_secs, 30);
''',
    "Video Manager config default test",
)
tests_tail = source.rfind("\n}")
source = source[:tests_tail] + r'''

    #[test]
    fn video_worker_requires_explicit_trusted_executables() {
        let config: Config = serde_json::from_str(
            r#"{
                "api": { "host": "0.0.0.0", "port": 8080 },
                "videoManager": { "downloadWorkerEnabled": true }
            }"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("ffprobeExecutable"));
        assert!(error.contains("ffmpegExecutable"));
    }
''' + source[tests_tail:]
path.write_text(source)

# Expose the new settings in the checked-in development config without enabling
# a worker until trusted binaries are explicitly provisioned.
path = Path("engine/settings.json")
source = path.read_text()
source = replace_once(
    source,
    '''    "defaultDatabase": "default",
    "liveIdleSecs": 7200,
    "downloadMaxBytes": 8589934592
''',
    '''    "defaultDatabase": "default",
    "liveIdleSecs": 7200,
    "downloadMaxBytes": 8589934592,
    "downloadWorkerEnabled": false,
    "ffprobeExecutable": "",
    "ffmpegExecutable": "",
    "workerRecoveryScanSecs": 30
''',
    "development Video Manager worker settings",
)
path.write_text(source)

# ---------------------------------------------------------------------------
# Backend owns the lazy worker lifecycle.
# ---------------------------------------------------------------------------
path = Path("engine/crates/backend/src/main.rs")
source = path.read_text()
old_block = '''    let video_manager = if config.video_manager.enabled {
        if config.video_manager.default_database != video_manager::DEFAULT_DATABASE_NAME {
            anyhow::bail!(
                "videoManager.defaultDatabase {:?} is not registered at boot; built-in default is {:?}",
                config.video_manager.default_database,
                video_manager::DEFAULT_DATABASE_NAME
            );
        }
        let data_dir = service_boot::resolve_runtime_path(&config.video_manager.data_dir);
        let database_path = data_dir.join("video-manager.db");
        let manager = video_manager::VideoManager::open_default(
            &database_path,
            config.video_manager.live_idle_secs,
        )?;
        tracing::info!(
            database = %database_path.display(),
            live_idle_secs = config.video_manager.live_idle_secs,
            "Video Manager control plane ready; heavy live/media workers sleeping"
        );
        Some(Arc::new(manager))
    } else {
        tracing::info!("Video Manager disabled by configuration");
        None
    };
'''
new_block = '''    let (video_manager, video_worker_task) = if config.video_manager.enabled {
        if config.video_manager.default_database != video_manager::DEFAULT_DATABASE_NAME {
            anyhow::bail!(
                "videoManager.defaultDatabase {:?} is not registered at boot; built-in default is {:?}",
                config.video_manager.default_database,
                video_manager::DEFAULT_DATABASE_NAME
            );
        }
        let data_dir = service_boot::resolve_runtime_path(&config.video_manager.data_dir);
        let database_path = data_dir.join("video-manager.db");
        let manager = Arc::new(video_manager::VideoManager::open_default(
            &database_path,
            config.video_manager.live_idle_secs,
        )?);
        let worker_task = if config.video_manager.download_worker_enabled {
            let ffprobe = service_boot::resolve_runtime_path(
                config.video_manager.ffprobe_executable.trim(),
            );
            let ffmpeg = service_boot::resolve_runtime_path(
                config.video_manager.ffmpeg_executable.trim(),
            );
            let mut download = video_manager::DownloadPolicy::default();
            download.max_bytes = config.video_manager.download_max_bytes;
            let policy = video_manager::VideoWorkerPolicy {
                download,
                ffprobe: video_manager::FfprobePolicy::new(&ffprobe),
                ffmpeg: video_manager::FfmpegPolicy::new(&ffmpeg),
                recovery_scan: Duration::from_secs(
                    config.video_manager.worker_recovery_scan_secs,
                ),
            };
            let task = manager.clone().spawn_download_worker(policy)?;
            tracing::info!(
                ffprobe = %ffprobe.display(),
                ffmpeg = %ffmpeg.display(),
                recovery_scan_secs = config.video_manager.worker_recovery_scan_secs,
                max_download_bytes = config.video_manager.download_max_bytes,
                "Video Manager lazy download worker ready"
            );
            Some(task)
        } else {
            tracing::info!(
                "Video Manager download worker disabled; queued downloads remain quarantined until a trusted worker is configured"
            );
            None
        };
        tracing::info!(
            database = %database_path.display(),
            live_idle_secs = config.video_manager.live_idle_secs,
            "Video Manager control plane ready; live media workers sleeping"
        );
        (Some(manager), worker_task)
    } else {
        tracing::info!("Video Manager disabled by configuration");
        (None, None)
    };
'''
source = replace_once(source, old_block, new_block, "backend Video Manager startup")
source = replace_once(
    source,
    '''    lifecycle.set(BackendState::Stopping);
    service_manager.shutdown_all().await;
    container_refresh_task.abort();
''',
    '''    lifecycle.set(BackendState::Stopping);
    service_manager.shutdown_all().await;
    if let Some(task) = video_worker_task {
        task.abort();
        let _ = task.await;
    }
    container_refresh_task.abort();
''',
    "backend Video Manager worker shutdown",
)
path.write_text(source)

# Permanent focused rustfmt coverage for the newly active boundaries.
path = Path(".github/workflows/runtime-ci.yml")
source = path.read_text()
source = replace_once(
    source,
    '''            crates/backend/src/service_boot.rs \\
            crates/video-manager/src/lib.rs \\
''',
    '''            crates/backend/src/service_boot.rs \\
            crates/backend/src/main.rs \\
            crates/config/src/lib.rs \\
            crates/video-manager/src/lib.rs \\
''',
    "runtime CI backend/config coverage",
)
source = replace_once(
    source,
    '''            crates/video-manager/src/normalization.rs \\
            crates/video-manager/src/pipeline.rs \\
''',
    '''            crates/video-manager/src/normalization.rs \\
            crates/video-manager/src/pipeline.rs \\
            crates/video-manager/src/worker.rs \\
''',
    "runtime CI worker coverage",
)
path.write_text(source)
