from pathlib import Path

path = Path("crates/video-manager/src/live_runtime.rs")
source = path.read_text()

source = source.replace(
'''            match tokio::time::timeout(LIVE_RUNTIME_START_TIMEOUT, driver.start(manager.clone()))
                .await
            {
                Ok(Ok(())) => {
                    active = true;
                    let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Active);
                    tracing::info!("Video Manager live runtime activated on demand");
                    continue;
                }
                Ok(Err(error)) => tracing::error!(
                    error = %error,
                    "Video Manager live runtime failed to start"
                ),
                Err(_) => tracing::error!(
                    timeout_secs = LIVE_RUNTIME_START_TIMEOUT.as_secs(),
                    "Video Manager live runtime start timed out"
                ),
            }
''',
'''            match run_live_driver_call(
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
''',
1,
)

source = source.replace(
"stop_live_runtime(manager.clone(), driver.as_ref(), false).await",
"stop_live_runtime(manager.clone(), driver.clone(), false).await",
)
source = source.replace(
"stop_live_runtime(manager.clone(), driver.as_ref(), true).await",
"stop_live_runtime(manager.clone(), driver.clone(), true).await",
)

old_stop = '''async fn stop_live_runtime(
    manager: Arc<VideoManager>,
    driver: &dyn LiveRuntimeDriver,
    idle_stop: bool,
) -> bool {
    let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Draining);
    match tokio::time::timeout(LIVE_RUNTIME_STOP_TIMEOUT, driver.stop(manager.clone())).await {
        Ok(Ok(())) => {
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Sleeping);
            if idle_stop {
                tracing::info!("Video Manager live runtime stopped after idle drain");
            }
            true
        }
        Ok(Err(error)) => {
            tracing::error!(error = %error, "Video Manager live runtime failed to stop");
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
            false
        }
        Err(_) => {
            tracing::error!(
                timeout_secs = LIVE_RUNTIME_STOP_TIMEOUT.as_secs(),
                "Video Manager live runtime stop timed out"
            );
            let _ = manager.set_live_runtime_state(VideoLiveRuntimeState::Degraded);
            false
        }
    }
}
'''
new_stop = '''#[derive(Debug, Clone, Copy)]
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
'''
if old_stop not in source:
    raise SystemExit("live runtime stop helper anchor missing")
source = source.replace(old_stop, new_stop, 1)

insert_before = '''    fn temp_db(name: &str) -> PathBuf {
'''
fixtures = r'''    struct PanicStartRuntime {
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

'''
if insert_before not in source:
    raise SystemExit("live runtime test fixture anchor missing")
source = source.replace(insert_before, fixtures + insert_before, 1)

last = source.rfind("\n}")
if last < 0:
    raise SystemExit("live runtime test module tail missing")
tests = r'''

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
'''
source = source[:last] + tests + source[last:]
path.write_text(source)
