use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::{VideoLiveRuntimeState, VideoManager};

const LIVE_RUNTIME_RECOVERY_SCAN: Duration = Duration::from_secs(30);
const LIVE_RUNTIME_START_TIMEOUT: Duration = Duration::from_secs(60);
const LIVE_RUNTIME_STOP_TIMEOUT: Duration = Duration::from_secs(60);

pub type LiveRuntimeFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

/// Trusted protocol runtime boundary for RTMP/WHIP/HLS implementations.
///
/// Implementations own listeners/processes/protocol resources. The language
/// runtime never receives this trait object and cannot call it directly.
/// `stop` must be idempotent and safe to call after a failed or timed-out
/// `start`, because the coordinator uses it to clean partially started runtime
/// resources before any future start attempt.
pub trait LiveRuntimeDriver: Send + Sync + 'static {
    fn start<'a>(&'a self, manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a>;
    fn stop<'a>(&'a self, manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a>;
}

pub struct LiveRuntimeHandle {
    shutdown: watch::Sender<bool>,
    join: tokio::task::JoinHandle<bool>,
}

impl LiveRuntimeHandle {
    pub async fn shutdown(self) -> bool {
        let _ = self.shutdown.send(true);
        self.join.await.unwrap_or(false)
    }
}

impl VideoManager {
    /// Spawn the lazy live-runtime coordinator. This does not create an RTMP or
    /// WHIP implementation itself; it owns the lifecycle of a trusted driver.
    pub fn spawn_live_runtime(
        self: Arc<Self>,
        driver: Arc<dyn LiveRuntimeDriver>,
    ) -> anyhow::Result<LiveRuntimeHandle> {
        let idle_timeout = Duration::from_secs(self.live_idle_secs);
        self.spawn_live_runtime_with_idle(driver, idle_timeout)
    }

    fn spawn_live_runtime_with_idle(
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
        let (shutdown, shutdown_task) = watch::channel(false);
        let manager = self.clone();
        let join = tokio::spawn(async move {
            let clean_exit =
                run_live_runtime_coordinator(manager.clone(), driver, idle_timeout, shutdown_task)
                    .await;
            if clean_exit {
                if let Err(error) = manager.set_live_runtime_state(VideoLiveRuntimeState::Disabled)
                {
                    tracing::error!(error = %error, "Video Manager live runtime exit telemetry failed");
                }
                manager
                    .live_runtime_claimed
                    .store(false, std::sync::atomic::Ordering::Release);
            } else {
                tracing::error!(
                    "Video Manager live runtime retained ownership after unsafe shutdown"
                );
            }
            clean_exit
        });
        Ok(LiveRuntimeHandle { shutdown, join })
    }
}

async fn run_live_runtime_coordinator(
    manager: Arc<VideoManager>,
    driver: Arc<dyn LiveRuntimeDriver>,
    idle_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> bool {
    let mut active = false;

    loop {
        let demand = match manager.live_runtime_demand() {
            Ok(demand) => demand,
            Err(error) => {
                tracing::error!(error = %error, "Video Manager failed to inspect live runtime demand");
                let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
                if wait_for_signal(&manager, &mut shutdown, LIVE_RUNTIME_RECOVERY_SCAN).await {
                    return if active {
                        stop_live_runtime(manager.clone(), driver.clone(), false).await
                    } else {
                        true
                    };
                }
                continue;
            }
        };

        if demand && !active {
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Starting);
            match run_live_driver_call(
                driver.clone(),
                manager.clone(),
                LiveDriverPhase::Start,
                LIVE_RUNTIME_START_TIMEOUT,
            )
            .await
            {
                Ok(()) => {
                    active = true;
                    let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Active);
                    tracing::info!("Video Manager live runtime activated on demand");
                    continue;
                }
                Err(error) => tracing::error!(
                    error = %error,
                    "Video Manager live runtime failed to start"
                ),
            }

            // A failed or timed-out start can still have created listeners,
            // subprocesses, sockets, or protocol state before it failed. Never
            // retry start until the trusted driver has proved those resources
            // are gone. If cleanup itself is uncertain, retain coordinator
            // ownership and fail closed instead of risking a duplicate runtime.
            if !stop_live_runtime(manager.clone(), driver.clone(), false).await {
                return false;
            }
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
            if wait_for_signal(&manager, &mut shutdown, LIVE_RUNTIME_RECOVERY_SCAN).await {
                return true;
            }
            continue;
        }

        if active && !demand {
            enum IdleSignal {
                Shutdown,
                Changed,
                Expired,
            }
            if *shutdown.borrow() {
                return stop_live_runtime(manager.clone(), driver.clone(), false).await;
            }
            let signal = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        IdleSignal::Shutdown
                    } else {
                        IdleSignal::Changed
                    }
                }
                _ = manager.live_notify.notified() => IdleSignal::Changed,
                _ = tokio::time::sleep(idle_timeout) => IdleSignal::Expired,
            };
            match signal {
                IdleSignal::Changed => continue,
                IdleSignal::Shutdown => {
                    return stop_live_runtime(manager.clone(), driver.clone(), false).await;
                }
                IdleSignal::Expired => match manager.live_runtime_demand() {
                    Ok(true) => continue,
                    Ok(false) => {
                        if stop_live_runtime(manager.clone(), driver.clone(), true).await {
                            active = false;
                        }
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            "Video Manager failed to recheck live demand before idle shutdown"
                        );
                        let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
                        continue;
                    }
                },
            }
        }

        if wait_for_signal(&manager, &mut shutdown, LIVE_RUNTIME_RECOVERY_SCAN).await {
            return if active {
                stop_live_runtime(manager.clone(), driver.clone(), false).await
            } else {
                true
            };
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LiveDriverPhase {
    Start,
    Stop,
}

impl LiveDriverPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

async fn run_live_driver_call(
    driver: Arc<dyn LiveRuntimeDriver>,
    manager: Arc<VideoManager>,
    phase: LiveDriverPhase,
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut task = tokio::spawn(async move {
        match phase {
            LiveDriverPhase::Start => driver.start(manager).await,
            LiveDriverPhase::Stop => driver.stop(manager).await,
        }
    });
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => anyhow::bail!(
            "Video Manager live driver {} task panicked or was cancelled: {error}",
            phase.label()
        ),
        Err(_) => {
            task.abort();
            let _ = task.await;
            anyhow::bail!(
                "Video Manager live driver {} exceeded timeout of {:?}",
                phase.label(),
                timeout
            )
        }
    }
}

async fn stop_live_runtime(
    manager: Arc<VideoManager>,
    driver: Arc<dyn LiveRuntimeDriver>,
    idle_stop: bool,
) -> bool {
    let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Draining);
    match run_live_driver_call(
        driver,
        manager.clone(),
        LiveDriverPhase::Stop,
        LIVE_RUNTIME_STOP_TIMEOUT,
    )
    .await
    {
        Ok(()) => {
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Sleeping);
            if idle_stop {
                tracing::info!("Video Manager live runtime stopped after idle drain");
            }
            true
        }
        Err(error) => {
            tracing::error!(error = %error, "Video Manager live runtime failed to stop safely");
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
            false
        }
    }
}

async fn wait_for_signal(
    manager: &VideoManager,
    shutdown: &mut watch::Receiver<bool>,
    scan: Duration,
) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    tokio::select! {
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
        _ = manager.live_notify.notified() => false,
        _ = tokio::time::sleep(scan) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateAssetRequest, ReserveLiveSessionRequest, VideoAssetState, VideoSourceType};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    struct FakeRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl LiveRuntimeDriver for FakeRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    struct DelayedStartRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl LiveRuntimeDriver for DelayedStartRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(75)).await;
                Ok(())
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    struct FailingStartRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
        fail_cleanup: bool,
    }

    impl LiveRuntimeDriver for FailingStartRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("simulated partial live runtime start failure")
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                if self.fail_cleanup {
                    anyhow::bail!("simulated partial live runtime cleanup failure");
                }
                Ok(())
            })
        }
    }

    struct FailingStopRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl LiveRuntimeDriver for FailingStopRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("simulated live runtime stop failure")
            })
        }
    }

    struct PanicStartRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl LiveRuntimeDriver for PanicStartRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                panic!("simulated live driver start panic");
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    struct PanicStopRuntime {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl LiveRuntimeDriver for PanicStopRuntime {
        fn start<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop<'a>(&'a self, _manager: Arc<VideoManager>) -> LiveRuntimeFuture<'a> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                panic!("simulated live driver stop panic");
            })
        }
    }

    fn temp_db(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rbe-video-live-runtime-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("video.db")
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition timed out");
    }

    fn make_live_session(manager: &VideoManager, title: &str) -> crate::VideoLiveSession {
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "runtime-test".into(),
                group: "live".into(),
                title: title.into(),
                source_type: VideoSourceType::Live,
                source_uri: None,
                metadata: serde_json::Value::Null,
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        manager
            .reserve_live_session(ReserveLiveSessionRequest {
                database: None,
                asset_id: asset.id,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn live_runtime_wakes_on_reservation_and_stops_after_idle() {
        let path = temp_db("lifecycle");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let driver = Arc::new(FakeRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .unwrap();
        let session = make_live_session(&manager, "Demand");
        wait_until(|| driver.starts.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Active
        );

        manager
            .request_end_live_session(None, &session.id)
            .unwrap()
            .unwrap();
        wait_until(|| driver.stops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Sleeping
        );
        assert!(handle.shutdown().await);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Disabled
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn new_live_demand_cancels_idle_shutdown() {
        let path = temp_db("idle-cancel");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let driver = Arc::new(FakeRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(80))
            .unwrap();

        let first = make_live_session(&manager, "First");
        wait_until(|| driver.starts.load(Ordering::SeqCst) == 1).await;
        manager
            .request_end_live_session(None, &first.id)
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = make_live_session(&manager, "Second");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(driver.stops.load(Ordering::SeqCst), 0);
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        manager
            .request_end_live_session(None, &second.id)
            .unwrap()
            .unwrap();
        wait_until(|| driver.stops.load(Ordering::SeqCst) == 1).await;
        assert!(handle.shutdown().await);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn shutdown_signal_is_retained_while_driver_start_is_in_flight() {
        let path = temp_db("shutdown-during-start");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        make_live_session(&manager, "Shutdown");
        let driver = Arc::new(DelayedStartRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_secs(1))
            .unwrap();

        wait_until(|| driver.starts.load(Ordering::SeqCst) == 1).await;
        let clean_shutdown = tokio::time::timeout(Duration::from_millis(500), handle.shutdown())
            .await
            .expect("live runtime shutdown signal was lost during driver start");

        assert!(clean_shutdown);
        assert_eq!(driver.stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Disabled
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_start_is_cleaned_before_runtime_can_retry() {
        let path = temp_db("start-cleanup");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        make_live_session(&manager, "Partial start");
        let driver = Arc::new(FailingStartRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            fail_cleanup: false,
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .unwrap();

        wait_until(|| driver.stops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );

        assert!(handle.shutdown().await);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Disabled
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_start_cleanup_keeps_runtime_owned_fail_closed() {
        let path = temp_db("start-cleanup-fail-closed");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        make_live_session(&manager, "Unsafe partial start");
        let driver = Arc::new(FailingStartRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            fail_cleanup: true,
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .unwrap();

        wait_until(|| driver.stops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );
        assert!(!handle.shutdown().await);
        assert!(manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_stop_keeps_live_runtime_owned_and_blocks_duplicate_start() {
        let path = temp_db("stop-fail-closed");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let driver = Arc::new(FailingStopRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .unwrap();

        let first = make_live_session(&manager, "First");
        wait_until(|| driver.starts.load(Ordering::SeqCst) == 1).await;
        manager
            .request_end_live_session(None, &first.id)
            .unwrap()
            .unwrap();
        wait_until(|| driver.stops.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );

        let _second = make_live_session(&manager, "Second");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);

        assert!(!handle.shutdown().await);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );
        assert!(manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn panicking_driver_start_is_isolated_and_cleaned() {
        let path = temp_db("panic-start");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let driver = Arc::new(PanicStartRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(25))
            .unwrap();
        let _session = make_live_session(&manager, "Panic start");
        wait_until(|| driver.stops.load(Ordering::SeqCst) >= 1).await;
        assert!(driver.starts.load(Ordering::SeqCst) >= 1);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );
        assert!(handle.shutdown().await);
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Disabled
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn panicking_driver_stop_is_isolated_and_keeps_ownership_fail_closed() {
        let path = temp_db("panic-stop");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let driver = Arc::new(PanicStopRuntime {
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let handle = manager
            .clone()
            .spawn_live_runtime_with_idle(driver.clone(), Duration::from_millis(20))
            .unwrap();
        let session = make_live_session(&manager, "Panic stop");
        wait_until(|| driver.starts.load(Ordering::SeqCst) == 1).await;
        manager
            .request_end_live_session(None, &session.id)
            .unwrap()
            .unwrap();
        wait_until(|| driver.stops.load(Ordering::SeqCst) >= 1).await;
        assert_eq!(
            manager.live_runtime_state().unwrap(),
            VideoLiveRuntimeState::Degraded
        );
        assert!(!handle.shutdown().await);
        assert!(manager
            .clone()
            .spawn_live_runtime_with_idle(driver, Duration::from_millis(20))
            .is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
