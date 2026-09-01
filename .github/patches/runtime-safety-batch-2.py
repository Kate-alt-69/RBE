from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# BUG-VM-001: supervise unexpected Video Manager worker task exits/panics.
path = Path("crates/video-manager/src/worker.rs")
source = path.read_text()
source = replace_once(
    source,
    "use std::time::Duration;",
    "use std::time::{Duration, Instant};",
    "worker time import",
)
source = replace_once(
    source,
    "const MAX_RECOVERY_SCAN: Duration = Duration::from_secs(60 * 60);",
    "const MAX_RECOVERY_SCAN: Duration = Duration::from_secs(60 * 60);\nconst WORKER_RESTART_BASE_DELAY: Duration = Duration::from_millis(250);\nconst WORKER_RESTART_MAX_DELAY: Duration = Duration::from_secs(30);\nconst WORKER_STABLE_WINDOW: Duration = Duration::from_secs(60);",
    "worker restart constants",
)
start = source.index("impl VideoManager {\n    fn recover_worker_downloads")
end = source.index("\n#[cfg(test)]", start)
replacement = r'''impl VideoManager {
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
    let mut recovery_required = true;
    loop {
        if *shutdown_rx.borrow() || shutdown_rx.has_changed().is_err() {
            break;
        }

        if recovery_required {
            if manager.recover_worker_downloads() {
                recovery_required = false;
            } else {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = manager.work_notify.notified() => {}
                    _ = tokio::time::sleep(policy.recovery_scan) => {}
                }
                continue;
            }
        }

        match manager.next_queued_download(None) {
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
                if let Err(error) = manager.set_worker_state(VideoWorkerState::Sleeping) {
                    tracing::error!(error = %error, "Video Manager worker telemetry failed");
                }
                continue;
            }
            Ok(None) => {
                if let Err(error) = manager.set_worker_state(VideoWorkerState::Sleeping) {
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
'''
source = source[:start] + replacement + source[end:]
source = replace_once(
    source,
    "        discoveries: AtomicUsize,\n    }",
    "        discoveries: AtomicUsize,\n        panic_first: bool,\n    }",
    "flaky recovery fields",
)
source = replace_once(
    source,
    "                discoveries: AtomicUsize::new(0),\n            }\n        }\n    }",
    "                discoveries: AtomicUsize::new(0),\n                panic_first: false,\n            }\n        }\n\n        fn panicking() -> Self {\n            Self {\n                panic_first: true,\n                ..Self::new()\n            }\n        }\n    }",
    "flaky recovery constructors",
)
source = replace_once(
    source,
    "            let attempt = self.recovery_attempts.fetch_add(1, Ordering::SeqCst);\n            if attempt == 0 {\n                anyhow::bail!(\"intentional recovery failure\")\n            }",
    "            let attempt = self.recovery_attempts.fetch_add(1, Ordering::SeqCst);\n            if attempt == 0 && self.panic_first {\n                panic!(\"intentional recovery panic\");\n            }\n            if attempt == 0 {\n                anyhow::bail!(\"intentional recovery failure\")\n            }",
    "flaky recovery panic hook",
)
last = source.rfind("\n}")
if last < 0:
    raise SystemExit("worker test module tail missing")
source = source[:last] + r'''

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
''' + source[last:]
path.write_text(source)


# BUG-VM-002: never return SQLite filesystem details to module language code.
path = Path("crates/core/src/video_language.rs")
source = path.read_text()
old = '''            "databaseHealth" | "database_health" => {
                expect_arity_range(function, args, 0, 1)?;
                let database = optional_string(args.first(), "database")?;
                json_result(serde_json::to_value(
                    manager
                        .database_health(database.as_deref())
                        .map_err(operation_error)?,
                ))
            }
'''
new = '''            "databaseHealth" | "database_health" => {
                expect_arity_range(function, args, 0, 1)?;
                let database = optional_string(args.first(), "database")?;
                let health = manager
                    .database_health(database.as_deref())
                    .map_err(operation_error)?;
                Ok(serde_json::json!({
                    "ok": health.ok,
                    "kind": health.kind,
                }))
            }
'''
source = replace_once(source, old, new, "database health language block")
last = source.rfind("\n}")
if last < 0:
    raise SystemExit("video language test module tail missing")
source = source[:last] + r'''

    #[test]
    fn database_health_hides_host_filesystem_details() {
        let path = temp_db("health-redaction");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let health = language
            .call("learning.catalog", "databaseHealth", &[])
            .unwrap();
        assert_eq!(health["ok"], true);
        assert!(health.get("kind").is_some());
        assert!(health.get("detail").is_none());
        assert!(!health
            .to_string()
            .contains(path.to_string_lossy().as_ref()));
        let _ = std::fs::remove_file(path);
    }
''' + source[last:]
path.write_text(source)


# BUG-SVC-008: clean aliases/children on every failed Service Mother spawn and
# await aborted supervisor tasks so kill-on-drop is observed before returning.
path = Path("crates/backend/src/service_mother.rs")
source = path.read_text()
source = replace_once(
    source,
    "    task: tokio::task::JoinHandle<()>,\n}",
    "    task: tokio::task::JoinHandle<()>,\n    alias: PathBuf,\n}",
    "Service Mother supervisor alias field",
)
source = replace_once(
    source,
    '''                self.task.abort();
            }
        }
    }
}
''',
    '''                self.task.abort();
                let _ = (&mut self.task).await;
            }
        }
        let _ = tokio::fs::remove_file(&self.alias).await;
    }
}
''',
    "Service Mother supervisor timeout cleanup",
)
source = replace_once(
    source,
    '''    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Service Mother stdout unavailable"))?;
''',
    '''    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            cleanup_failed_spawn(&alias, &mut child).await;
            anyhow::bail!("Service Mother stdout unavailable");
        }
    };
''',
    "Service Mother stdout cleanup",
)
old_manager = '''    let manager = match existing_manager {
        Some(manager) => {
            manager.replace_remote(ready.address, token).await?;
            manager.clone()
        }
        None => ServiceManager::remote(ready.address, token)?,
    };
'''
new_manager = '''    let manager_result = match existing_manager {
        Some(manager) => manager
            .replace_remote(ready.address, token)
            .await
            .map(|()| manager.clone()),
        None => ServiceManager::remote(ready.address, token),
    };
    let manager = match manager_result {
        Ok(manager) => manager,
        Err(error) => {
            cleanup_failed_spawn(&alias, &mut child).await;
            return Err(error);
        }
    };
'''
source = replace_once(source, old_manager, new_manager, "Service Mother remote manager cleanup")
source = replace_once(
    source,
    '''    let initial = spawn_process(&settings_path, None).await?;
    let manager = initial.manager();
''',
    '''    let initial = spawn_process(&settings_path, None).await?;
    let alias = initial.alias.clone();
    let manager = initial.manager();
''',
    "Service Mother supervisor alias capture",
)
source = replace_once(
    source,
    '''        shutdown: Some(shutdown_tx),
        task,
    })
''',
    '''        shutdown: Some(shutdown_tx),
        task,
        alias,
    })
''',
    "Service Mother supervisor alias init",
)
path.write_text(source)
