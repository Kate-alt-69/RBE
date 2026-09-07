from pathlib import Path
import sys


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    return source.replace(old, new, 1)


def svc_shutdown() -> None:
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "const RESTART_BASE_DELAY_MS: u64 = 250;\n",
        "const RESTART_BASE_DELAY_MS: u64 = 250;\nconst SERVICE_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);\nconst SERVICE_SHUTDOWN_DRAIN_POLL: Duration = Duration::from_millis(10);\n",
        "service shutdown constants",
    )
    source = replace_once(
        source,
        "        let (address, token, active_call) = {\n            let mut service = handle.lock().await;\n            self.activate_for_call(&mut service).await?;",
        "        let (address, token, active_call) = {\n            let mut service = handle.lock().await;\n            if self.shutting_down.load(Ordering::Acquire) {\n                return Err(ServiceCallError::Unavailable {\n                    service: service_name.to_string(),\n                });\n            }\n            self.activate_for_call(&mut service).await?;",
        "under-lock shutdown invoke check",
    )
    source = replace_once(
        source,
        "        let mut snapshots = tokio::task::JoinSet::new();\n        for handle in handles {\n            snapshots.spawn(snapshot_managed_service(handle));\n        }",
        "        let probe_health = !self.shutting_down.load(Ordering::Acquire);\n        let mut snapshots = tokio::task::JoinSet::new();\n        for handle in handles {\n            snapshots.spawn(snapshot_managed_service(handle, probe_health));\n        }",
        "shutdown-safe snapshot dispatch",
    )
    old = '''        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handle in handles {
            let mut service = handle.lock().await;
            if let Some(mut process) = service.process.take() {
                stop_process(&service.file.name, &mut process).await;
            }
            service.exit_observed = true;
            service.restarting = false;
        }
'''
    new = '''        let handles = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();

        // Acquire every service lock once after raising `shutting_down`. This
        // closes the race with a call that passed the outer check but had not
        // yet acquired its activity guard.
        let mut activity = Vec::with_capacity(handles.len());
        for handle in &handles {
            let service = handle.lock().await;
            activity.push(service.active_calls.clone());
        }
        if !wait_for_service_drain(&activity, SERVICE_SHUTDOWN_DRAIN_TIMEOUT).await {
            tracing::warn!(
                active_calls = total_active_calls(&activity),
                timeout_ms = SERVICE_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                "service shutdown drain timed out; forcing remaining service processes to stop"
            );
        }

        let mut stops = tokio::task::JoinSet::new();
        for handle in handles {
            let process = {
                let mut service = handle.lock().await;
                service.exit_observed = true;
                service.restarting = false;
                service
                    .process
                    .take()
                    .map(|process| (service.file.name.clone(), process))
            };
            if let Some((name, mut process)) = process {
                stops.spawn(async move {
                    stop_process(&name, &mut process).await;
                });
            }
        }
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result {
                tracing::warn!(error = %error, "service shutdown task failed");
            }
        }
'''
    source = replace_once(source, old, new, "concurrent shutdown implementation")
    source = replace_once(
        source,
        "async fn snapshot_managed_service(handle: Arc<Mutex<Managed>>) -> ServiceSnapshot {",
        "async fn snapshot_managed_service(\n    handle: Arc<Mutex<Managed>>,\n    probe_health: bool,\n) -> ServiceSnapshot {",
        "snapshot probe parameter",
    )
    source = replace_once(
        source,
        "        let active_call = health_target\n            .as_ref()\n            .map(|_| ActiveCallGuard::acquire(service.active_calls.clone()));",
        "        let health_target = probe_health.then_some(health_target).flatten();\n        let active_call = health_target\n            .as_ref()\n            .map(|_| ActiveCallGuard::acquire(service.active_calls.clone()));",
        "shutdown health probe suppression",
    )
    insert = r'''

fn total_active_calls(activity: &[Arc<AtomicU32>]) -> u64 {
    activity
        .iter()
        .map(|counter| u64::from(counter.load(Ordering::Acquire)))
        .sum()
}

async fn wait_for_service_drain(activity: &[Arc<AtomicU32>], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if total_active_calls(activity) == 0 {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(SERVICE_SHUTDOWN_DRAIN_POLL.min(deadline - now)).await;
    }
}
'''
    source = replace_once(
        source,
        "\nfn health_value_ready(value: &Value) -> bool {",
        insert + "\nfn health_value_ready(value: &Value) -> bool {",
        "service drain helpers",
    )
    anchor = "    #[test]\n    fn active_call_guard_releases_counter_when_dropped() {"
    test = '''    #[tokio::test]
    async fn service_shutdown_drain_waits_for_active_call() {
        let counter = Arc::new(AtomicU32::new(1));
        let released = counter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            released.store(0, Ordering::Release);
        });
        assert!(wait_for_service_drain(&[counter], Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn service_shutdown_drain_is_bounded() {
        let counter = Arc::new(AtomicU32::new(1));
        assert!(!wait_for_service_drain(&[counter], Duration::from_millis(20)).await);
    }

    #[test]
    fn active_call_guard_releases_counter_when_dropped() {'''
    source = replace_once(source, anchor, test, "service shutdown drain tests")
    path.write_text(source)


def video_database_isolation() -> None:
    path = Path("crates/video-manager/src/lib.rs")
    source = path.read_text()
    old = '''        let start = self.worker_database_cursor.fetch_add(1, Ordering::Relaxed) % names.len();
        for offset in 0..names.len() {
            let name = &names[(start + offset) % names.len()];
            let (_, database) = self.resolve_database(Some(name))?;
            if let Some(queued) = database.next_queued_download(name)? {
                return Ok(Some(queued));
            }
        }
        Ok(None)
'''
    new = '''        let start = self.worker_database_cursor.fetch_add(1, Ordering::Relaxed) % names.len();
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
                            "Video Manager isolated queue discovery failure and continued with healthy database"
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
                "Video Manager queue discovery failed for database adapter(s): {}",
                failures.join("; ")
            )
        }
'''
    source = replace_once(source, old, new, "isolated database queue discovery")

    anchor = "    #[test]\n    fn queued_download_selection_rotates_between_registered_databases() {"
    support = r'''    struct FailingQueueDatabase;

    impl VideoDatabase for FailingQueueDatabase {
        fn kind(&self) -> &'static str {
            "failing-test"
        }

        fn health(&self) -> DatabaseHealth {
            DatabaseHealth {
                ok: false,
                kind: self.kind().into(),
                detail: Some("intentional test failure".into()),
            }
        }

        fn create_asset(
            &self,
            _database: &str,
            _request: &CreateAssetRequest,
        ) -> anyhow::Result<VideoAsset> {
            anyhow::bail!("unsupported test operation")
        }

        fn insert_job(&self, _job: &VideoJob) -> anyhow::Result<()> {
            anyhow::bail!("unsupported test operation")
        }

        fn claim_job(
            &self,
            _job_id: &str,
            _expected_state: &str,
            _claimed_state: &str,
        ) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unsupported test operation")
        }

        fn update_job(
            &self,
            _job_id: &str,
            _state: &str,
            _progress: f64,
            _error: Option<&str>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("unsupported test operation")
        }

        fn transition_job(
            &self,
            _job_id: &str,
            _expected_state: &str,
            _next_state: &str,
        ) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unsupported test operation")
        }

        fn get_job(&self, _job_id: &str) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unsupported test operation")
        }

        fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
            anyhow::bail!("intentional queue discovery failure")
        }

        fn commit_ready_variant(
            &self,
            _job_id: &str,
            _variant: &VideoVariant,
        ) -> anyhow::Result<Option<VideoJob>> {
            anyhow::bail!("unsupported test operation")
        }

        fn get_asset(
            &self,
            _database: &str,
            _asset_id: &str,
        ) -> anyhow::Result<Option<VideoAsset>> {
            anyhow::bail!("unsupported test operation")
        }
    }

    #[test]
    fn broken_database_does_not_block_healthy_queued_download() {
        let path = temp_db("queue-isolation");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        manager
            .register_database("broken", Arc::new(FailingQueueDatabase))
            .unwrap();
        manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "isolation".into(),
                group: "downloads".into(),
                title: "Healthy".into(),
                url: "https://example.invalid/healthy.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();

        // Start on the broken adapter so the healthy default database is only
        // reachable if discovery isolates the adapter error and keeps scanning.
        manager.worker_database_cursor.store(1, Ordering::Relaxed);
        let queued = manager.next_queued_download(None).unwrap().unwrap();
        assert_eq!(queued.asset.database, DEFAULT_DATABASE_NAME);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

'''
    source = replace_once(source, anchor, support + anchor, "database isolation regression support")
    path.write_text(source)


stages = {"svc-shutdown": svc_shutdown, "video-db-isolation": video_database_isolation}
if len(sys.argv) != 2 or sys.argv[1] not in stages:
    raise SystemExit("usage: runtime-drain-db-isolation.py <" + "|".join(stages) + ">")
stages[sys.argv[1]]()
