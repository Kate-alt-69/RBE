from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# Fix the unreferenced coordinator checkpoint to keep the original Arc<VideoManager>
# through stop calls and to claim a single coordinator instance.
path = Path("crates/video-manager/src/live_runtime.rs")
source = path.read_text()
source = source.replace(
    "stop_live_runtime(&manager, driver.as_ref(), false).await;",
    "stop_live_runtime(manager.clone(), driver.as_ref(), false).await;",
)
source = source.replace(
    "stop_live_runtime(&manager, driver.as_ref(), true).await;",
    "stop_live_runtime(manager.clone(), driver.as_ref(), true).await;",
)
source = replace_once(
    source,
    '''async fn stop_live_runtime(
    manager: &VideoManager,
    driver: &dyn LiveRuntimeDriver,
    idle_stop: bool,
) {
''',
    '''async fn stop_live_runtime(
    manager: Arc<VideoManager>,
    driver: &dyn LiveRuntimeDriver,
    idle_stop: bool,
) {
''',
    "live runtime stop signature",
)
source = replace_once(
    source,
    "driver.stop(Arc::new(manager.clone_for_live_runtime())),",
    "driver.stop(manager.clone()),",
    "live runtime stop manager Arc",
)
spawn_anchor = '''    fn spawn_live_runtime_with_idle(
        self: Arc<Self>,
        driver: Arc<dyn LiveRuntimeDriver>,
        idle_timeout: Duration,
    ) -> anyhow::Result<LiveRuntimeHandle> {
        self.set_live_runtime_state(VideoLiveRuntimeState::Sleeping)?;
'''
spawn_new = '''    fn spawn_live_runtime_with_idle(
        self: Arc<Self>,
        driver: Arc<dyn LiveRuntimeDriver>,
        idle_timeout: Duration,
    ) -> anyhow::Result<LiveRuntimeHandle> {
        if self
            .live_runtime_claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            anyhow::bail!("Video Manager live runtime coordinator is already running");
        }
        if let Err(error) = self.set_live_runtime_state(VideoLiveRuntimeState::Sleeping) {
            self.live_runtime_claimed
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
'''
source = replace_once(source, spawn_anchor, spawn_new, "live runtime single owner claim")
exit_anchor = '''            if let Err(error) = manager.set_live_runtime_state(VideoLiveRuntimeState::Disabled) {
                tracing::error!(error = %error, "Video Manager live runtime exit telemetry failed");
            }
'''
exit_new = exit_anchor + '''            manager
                .live_runtime_claimed
                .store(false, std::sync::atomic::Ordering::Release);
'''
source = replace_once(source, exit_anchor, exit_new, "live runtime owner release")
# Ensure every stop call now passes the Arc. A missed old form must fail fast.
if "stop_live_runtime(&manager" in source or "clone_for_live_runtime" in source:
    raise SystemExit("stale live runtime manager clone path remains")
path.write_text(source)

# live.rs: runtime state is actual coordinator state; session changes notify demand.
path = Path("crates/video-manager/src/live.rs")
source = path.read_text()
source = replace_once(
    source,
    '''pub enum VideoLiveRuntimeState {
    Sleeping,
    Starting,
    Active,
    Draining,
}

impl VideoLiveRuntimeState {
    pub(crate) fn from_counts(counts: VideoLiveSessionCounts) -> Self {
        if counts.live > 0 {
            Self::Active
        } else if counts.starting > 0 {
            Self::Starting
        } else if counts.stopping > 0 {
            Self::Draining
        } else {
            Self::Sleeping
        }
    }
}
''',
    '''pub enum VideoLiveRuntimeState {
    Disabled,
    Sleeping,
    Starting,
    Active,
    Draining,
    Degraded,
}

impl VideoLiveRuntimeState {
    pub fn healthy(self) -> bool {
        self != Self::Degraded
    }
}
''',
    "live runtime state model",
)
source = replace_once(
    source,
    '''        database.insert_live_session(&database_name, &session)?;
        Ok(session)
''',
    '''        database.insert_live_session(&database_name, &session)?;
        self.live_notify.notify_waiters();
        Ok(session)
''',
    "live reservation notify",
)
source = replace_once(
    source,
    '''        let (database_name, database) = self.resolve_database(database)?;
        database.bind_live_session(&database_name, session_id, &binding)
''',
    '''        let (database_name, database) = self.resolve_database(database)?;
        let result = database.bind_live_session(&database_name, session_id, &binding)?;
        if result.is_some() {
            self.live_notify.notify_waiters();
        }
        Ok(result)
''',
    "live binding notify",
)
source = replace_once(
    source,
    '''        let (database_name, database) = self.resolve_database(database)?;
        database.transition_live_session(&database_name, session_id, expected, next)
''',
    '''        let (database_name, database) = self.resolve_database(database)?;
        let result = database.transition_live_session(&database_name, session_id, expected, next)?;
        if result.is_some() {
            self.live_notify.notify_waiters();
        }
        Ok(result)
''',
    "live transition notify",
)
path.write_text(source)

# lib.rs: wire coordinator module, state/notify fields, actual runtime telemetry.
path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(source, "mod live;\n", "mod live;\nmod live_runtime;\n", "live runtime module")
live_export_start = source.index("pub use live::{")
live_export_end = source.index("};", live_export_start) + 2
live_export = source[live_export_start:live_export_end]
source = source[:live_export_end] + '''
pub use live_runtime::{LiveRuntimeDriver, LiveRuntimeFuture, LiveRuntimeHandle};''' + source[live_export_end:]
source = replace_once(
    source,
    "use std::sync::{Arc, Mutex, RwLock};",
    "use std::sync::atomic::AtomicBool;\nuse std::sync::{Arc, Mutex, RwLock};",
    "AtomicBool import",
)
source = replace_once(
    source,
    '''    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,
    live_idle_secs: u64,
''',
    '''    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,
    live_notify: tokio::sync::Notify,
    live_runtime_state: Mutex<VideoLiveRuntimeState>,
    live_runtime_claimed: AtomicBool,
    live_idle_secs: u64,
''',
    "VideoManager live runtime fields",
)
source = replace_once(
    source,
    '''            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            live_idle_secs,
''',
    '''            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
            live_notify: tokio::sync::Notify::new(),
            live_runtime_state: Mutex::new(VideoLiveRuntimeState::Disabled),
            live_runtime_claimed: AtomicBool::new(false),
            live_idle_secs,
''',
    "VideoManager live runtime initializer",
)
methods_anchor = '''    pub fn database_health(&self, database: Option<&str>) -> anyhow::Result<DatabaseHealth> {
        let (_, database) = self.resolve_database(database)?;
        Ok(database.health())
    }

'''
methods_new = methods_anchor + '''    pub(crate) fn set_live_runtime_state(
        &self,
        state: VideoLiveRuntimeState,
    ) -> anyhow::Result<()> {
        let mut current = self
            .live_runtime_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager live runtime state mutex is poisoned"))?;
        *current = state;
        Ok(())
    }

    pub fn live_runtime_state(&self) -> anyhow::Result<VideoLiveRuntimeState> {
        self.live_runtime_state
            .lock()
            .map(|state| *state)
            .map_err(|_| anyhow::anyhow!("Video Manager live runtime state mutex is poisoned"))
    }

    pub(crate) fn live_runtime_demand(&self) -> anyhow::Result<bool> {
        let counts = self.live_session_counts()?;
        Ok(counts.reserved > 0 || counts.starting > 0 || counts.live > 0 || counts.stopping > 0)
    }

'''
source = replace_once(source, methods_anchor, methods_new, "live runtime manager methods")
source = replace_once(
    source,
    '''        let live_sessions = self.live_session_counts()?;
        let live_runtime = VideoLiveRuntimeState::from_counts(live_sessions);
        Ok(VideoManagerStatus {
            ok: default_ok && worker_state != VideoWorkerState::Degraded,
''',
    '''        let live_sessions = self.live_session_counts()?;
        let live_runtime = self.live_runtime_state()?;
        Ok(VideoManagerStatus {
            ok: default_ok
                && worker_state != VideoWorkerState::Degraded
                && live_runtime.healthy(),
''',
    "actual live runtime status",
)
# Existing DB-only live tests must no longer pretend a protocol runtime exists.
source = replace_once(
    source,
    "assert_eq!(status.live_runtime, VideoLiveRuntimeState::Active);",
    "assert_eq!(status.live_runtime, VideoLiveRuntimeState::Disabled);",
    "DB-only live test active status",
)
source = replace_once(
    source,
    '''        assert_eq!(
            manager.status().unwrap().live_runtime,
            VideoLiveRuntimeState::Sleeping
        );
''',
    '''        assert_eq!(
            manager.status().unwrap().live_runtime,
            VideoLiveRuntimeState::Disabled
        );
''',
    "DB-only live test sleeping status",
)
path.write_text(source)
