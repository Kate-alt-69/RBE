from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()

status = '''#[derive(Debug, Clone, Serialize)]
pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub live_runtime: &'static str,
    pub live_idle_secs: u64,
}
'''
status_new = '''#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoWorkerState {
    Disabled,
    Sleeping,
    Processing,
    Degraded,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoDownloadWorkerStatus {
    pub state: VideoWorkerState,
    pub queued_downloads: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub download_worker: VideoDownloadWorkerStatus,
    pub live_runtime: &'static str,
    pub live_idle_secs: u64,
}
'''
source = replace_once(source, status, status_new, "Video Manager status models")

trait_anchor = '''    fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        Ok(None)
    }
'''
trait_new = '''    fn queued_download_count(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
    fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        Ok(None)
    }
'''
source = replace_once(source, trait_anchor, trait_new, "VideoDatabase queue count hook")

impl_anchor = '''    fn next_queued_download(&self, database: &str) -> anyhow::Result<Option<QueuedDownload>> {
'''
impl_method = r'''    fn queued_download_count(&self) -> anyhow::Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let count = connection.query_row(
            "SELECT COUNT(*) FROM video_jobs WHERE job_type = 'download' AND state = 'queued'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        u64::try_from(count).context("Video Manager queued download count is negative")
    }

'''
if impl_anchor not in source:
    raise SystemExit("SQLite queue discovery anchor missing")
source = source.replace(impl_anchor, impl_method + impl_anchor, 1)

manager_fields = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    work_notify: tokio::sync::Notify,
    live_idle_secs: u64,
}
'''
manager_fields_new = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    work_notify: tokio::sync::Notify,
    worker_state: Mutex<VideoWorkerState>,
    live_idle_secs: u64,
}
'''
source = replace_once(source, manager_fields, manager_fields_new, "VideoManager worker state field")
source = replace_once(
    source,
    '''            media_root,
            work_notify: tokio::sync::Notify::new(),
            live_idle_secs,
''',
    '''            media_root,
            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            live_idle_secs,
''',
    "VideoManager worker state initializer",
)

manager_anchor = '''    fn next_queued_download(
        &self,
        requested_database: Option<&str>,
    ) -> anyhow::Result<Option<QueuedDownload>> {
'''
manager_helpers = r'''    fn queued_download_count(&self) -> anyhow::Result<u64> {
        let mut total = 0u64;
        for name in self.database_names()? {
            let (_, database) = self.resolve_database(Some(&name))?;
            total = total
                .checked_add(database.queued_download_count()?)
                .ok_or_else(|| anyhow::anyhow!("Video Manager queued download count overflowed"))?;
        }
        Ok(total)
    }

    fn set_worker_state(&self, state: VideoWorkerState) -> anyhow::Result<()> {
        let mut current = self
            .worker_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))?;
        *current = state;
        Ok(())
    }

    fn worker_state(&self) -> anyhow::Result<VideoWorkerState> {
        self.worker_state
            .lock()
            .map(|state| *state)
            .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))
    }

'''
if manager_anchor not in source:
    raise SystemExit("VideoManager queue discovery anchor missing")
source = source.replace(manager_anchor, manager_helpers + manager_anchor, 1)

status_impl = '''    pub fn status(&self) -> anyhow::Result<VideoManagerStatus> {
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let mut names = databases.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let default_ok = databases
            .get(&self.default_database)
            .map(|database| database.health().ok)
            .unwrap_or(false);
        Ok(VideoManagerStatus {
            ok: default_ok,
            databases: names,
            default_database: self.default_database.clone(),
            live_runtime: "sleeping",
            live_idle_secs: self.live_idle_secs,
        })
    }
'''
status_impl_new = '''    pub fn status(&self) -> anyhow::Result<VideoManagerStatus> {
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let mut names = databases.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let default_ok = databases
            .get(&self.default_database)
            .map(|database| database.health().ok)
            .unwrap_or(false);
        drop(databases);
        let worker_state = self.worker_state()?;
        let queued_downloads = self.queued_download_count()?;
        Ok(VideoManagerStatus {
            ok: default_ok && worker_state != VideoWorkerState::Degraded,
            databases: names,
            default_database: self.default_database.clone(),
            download_worker: VideoDownloadWorkerStatus {
                state: worker_state,
                queued_downloads,
            },
            live_runtime: "sleeping",
            live_idle_secs: self.live_idle_secs,
        })
    }
'''
source = replace_once(source, status_impl, status_impl_new, "VideoManager status implementation")

# Status tests cover safe public state and queue depth without worker IDs/paths.
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Video Manager tests tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn status_reports_worker_state_and_aggregate_queue_depth() {
        let path = temp_db("worker-status");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let initial = manager.status().unwrap();
        assert_eq!(initial.download_worker.state, VideoWorkerState::Disabled);
        assert_eq!(initial.download_worker.queued_downloads, 0);
        assert!(initial.ok);

        manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "status".into(),
                group: "queue".into(),
                title: "Waiting".into(),
                url: "https://example.invalid/waiting.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let queued = manager.status().unwrap();
        assert_eq!(queued.download_worker.queued_downloads, 1);
        assert_eq!(queued.download_worker.state, VideoWorkerState::Disabled);
        assert!(queued.ok);

        manager.set_worker_state(VideoWorkerState::Degraded).unwrap();
        let degraded = manager.status().unwrap();
        assert_eq!(degraded.download_worker.state, VideoWorkerState::Degraded);
        assert!(!degraded.ok);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# Worker state transitions: infrastructure errors degrade, individual jobs do not.
path = Path("engine/crates/video-manager/src/worker.rs")
source = path.read_text()
source = replace_once(
    source,
    '''        policy.validate()?;
        Ok(tokio::spawn(async move {
''',
    '''        policy.validate()?;
        self.set_worker_state(crate::VideoWorkerState::Sleeping)?;
        Ok(tokio::spawn(async move {
''',
    "worker initial sleeping state",
)
source = replace_once(
    source,
    '''                Err(error) => tracing::error!(
                    error = %error,
                    "Video Manager failed to complete startup download recovery"
                ),
''',
    '''                Err(error) => {
                    let _ = self.set_worker_state(crate::VideoWorkerState::Degraded);
                    tracing::error!(
                        error = %error,
                        "Video Manager failed to complete startup download recovery"
                    );
                }
''',
    "worker recovery degraded state",
)
source = replace_once(
    source,
    '''                    Ok(Some(queued)) => {
                        let asset_id = queued.asset.id.clone();
''',
    '''                    Ok(Some(queued)) => {
                        if let Err(error) = self.set_worker_state(crate::VideoWorkerState::Processing) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
                        let asset_id = queued.asset.id.clone();
''',
    "worker processing state",
)
source = replace_once(
    source,
    '''                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::error!(
                        error = %error,
                        "Video Manager failed to discover queued download work"
                    ),
''',
    '''                        }
                        if let Err(error) = self.set_worker_state(crate::VideoWorkerState::Sleeping) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
                        continue;
                    }
                    Ok(None) => {
                        if let Err(error) = self.set_worker_state(crate::VideoWorkerState::Sleeping) {
                            tracing::error!(error = %error, "Video Manager worker telemetry failed");
                        }
                    }
                    Err(error) => {
                        let _ = self.set_worker_state(crate::VideoWorkerState::Degraded);
                        tracing::error!(
                            error = %error,
                            "Video Manager failed to discover queued download work"
                        );
                    }
''',
    "worker idle/degraded transitions",
)
path.write_text(source)
