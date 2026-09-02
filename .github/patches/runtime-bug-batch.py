from pathlib import Path
import sys


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    return source.replace(old, new, 1)


def stage_service_alias() -> None:
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {",
        "if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {",
        "service process alias sanitizer",
    )
    anchor = "    #[test]\n    fn restart_policy_distinguishes_clean_and_failed_exits() {"
    test = '''    #[test]
    fn service_process_alias_preserves_distinct_legal_names() {
        assert_eq!(process_name("cache.v1"), "cache.v1");
        assert_eq!(process_name("cache-v1"), "cache-v1");
        assert_ne!(process_name("cache.v1"), process_name("cache-v1"));
    }

    #[test]
    fn restart_policy_distinguishes_clean_and_failed_exits() {'''
    source = replace_once(source, anchor, test, "service alias regression test")
    path.write_text(source)


def stage_service_activity() -> None:
    path = Path("crates/service-runtime/src/manager.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use std::sync::atomic::{AtomicBool, Ordering};",
        "use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};",
        "service atomic imports",
    )
    source = replace_once(
        source,
        "    active_calls: u32,",
        "    active_calls: Arc<AtomicU32>,",
        "managed active call counter",
    )
    count = source.count("            active_calls: 0,")
    if count != 2:
        raise SystemExit(f"managed constructors: expected two active call initializers, found {count}")
    source = source.replace(
        "            active_calls: 0,",
        "            active_calls: Arc::new(AtomicU32::new(0)),",
    )
    source = replace_once(
        source,
        "            && self.active_calls == 0\n",
        "            && self.active_calls.load(Ordering::Acquire) == 0\n",
        "idle active call check",
    )
    marker = "\n#[derive(Clone, Default)]\npub struct ServiceManager {"
    guard = r'''

struct ActiveCallGuard {
    counter: Arc<AtomicU32>,
}

impl ActiveCallGuard {
    fn acquire(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        let _ = self.counter.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |value| value.checked_sub(1),
        );
    }
}
'''
    source = replace_once(source, marker, guard + marker, "active call guard insertion")

    old = '''        let (address, token) = {
            let mut service = handle.lock().await;
            self.activate_for_call(&mut service).await?;
            let process =
                service
                    .process
                    .as_ref()
                    .ok_or_else(|| ServiceCallError::Unavailable {
                        service: service_name.to_string(),
                    })?;
            let address = process.ready.address;
            let token = process.token.clone();
            service.active_calls = service.active_calls.saturating_add(1);
            service.last_activity = Instant::now();
            (address, token)
        };

        let request = operation.into_request(token);
        let response = rpc(address, request).await;

        {
            let mut service = handle.lock().await;
            service.active_calls = service.active_calls.saturating_sub(1);
            service.last_activity = Instant::now();
        }
'''
    new = '''        let (address, token, active_call) = {
            let mut service = handle.lock().await;
            self.activate_for_call(&mut service).await?;
            let process =
                service
                    .process
                    .as_ref()
                    .ok_or_else(|| ServiceCallError::Unavailable {
                        service: service_name.to_string(),
                    })?;
            let address = process.ready.address;
            let token = process.token.clone();
            service.last_activity = Instant::now();
            let active_call = ActiveCallGuard::acquire(service.active_calls.clone());
            (address, token, active_call)
        };

        let request = operation.into_request(token);
        let response = rpc(address, request).await;

        {
            let mut service = handle.lock().await;
            service.last_activity = Instant::now();
        }
        drop(active_call);
'''
    source = replace_once(source, old, new, "service invoke activity accounting")

    source = replace_once(
        source,
        "    let (mut snapshot, health_target) = {\n",
        "    let (mut snapshot, health_target, active_call) = {\n",
        "snapshot tuple",
    )
    old = '''        if health_target.is_some() {
            service.active_calls = service.active_calls.saturating_add(1);
        }
        (
            ServiceSnapshot {'''
    new = '''        let active_call = health_target
            .as_ref()
            .map(|_| ActiveCallGuard::acquire(service.active_calls.clone()));
        (
            ServiceSnapshot {'''
    source = replace_once(source, old, new, "snapshot activity acquire")
    old = '''            },
            health_target,
        )
    };

    if let Some((address, token)) = health_target {'''
    new = '''            },
            health_target,
            active_call,
        )
    };

    if let Some((address, token)) = health_target {'''
    source = replace_once(source, old, new, "snapshot activity tuple result")
    old = '''        {
            let mut service = handle.lock().await;
            service.active_calls = service.active_calls.saturating_sub(1);
        }
        match response {'''
    new = '''        drop(active_call);
        match response {'''
    source = replace_once(source, old, new, "snapshot activity release")

    anchor = "    #[test]\n    fn service_process_alias_preserves_distinct_legal_names() {"
    test = '''    #[test]
    fn active_call_guard_releases_counter_when_dropped() {
        let counter = Arc::new(AtomicU32::new(0));
        {
            let _guard = ActiveCallGuard::acquire(counter.clone());
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn service_process_alias_preserves_distinct_legal_names() {'''
    source = replace_once(source, anchor, test, "active call guard regression test")
    path.write_text(source)


def stage_video_fairness() -> None:
    path = Path("crates/video-manager/src/lib.rs")
    source = path.read_text()
    source = replace_once(
        source,
        "use std::sync::atomic::AtomicBool;",
        "use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};",
        "video atomic imports",
    )
    source = replace_once(
        source,
        "    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,\n",
        "    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,\n    worker_database_cursor: AtomicUsize,\n",
        "worker database cursor field",
    )
    source = replace_once(
        source,
        "            worker_encoder: Mutex::new(None),\n            live_notify:",
        "            worker_encoder: Mutex::new(None),\n            worker_database_cursor: AtomicUsize::new(0),\n            live_notify:",
        "worker database cursor initializer",
    )
    old = '''    fn next_queued_download(
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
'''
    new = '''    fn next_queued_download(
        &self,
        requested_database: Option<&str>,
    ) -> anyhow::Result<Option<QueuedDownload>> {
        if let Some(name) = requested_database {
            let (_, database) = self.resolve_database(Some(name))?;
            return database.next_queued_download(name);
        }

        let names = self.database_names()?;
        if names.is_empty() {
            return Ok(None);
        }
        let start = self.worker_database_cursor.fetch_add(1, Ordering::Relaxed) % names.len();
        for offset in 0..names.len() {
            let name = &names[(start + offset) % names.len()];
            let (_, database) = self.resolve_database(Some(name))?;
            if let Some(queued) = database.next_queued_download(name)? {
                return Ok(Some(queued));
            }
        }
        Ok(None)
    }
'''
    source = replace_once(source, old, new, "round robin queue selection")
    anchor = "    #[test]\n    fn creates_stable_vm_identity_and_reads_it_back() {"
    test = '''    #[test]
    fn queued_download_selection_rotates_between_registered_databases() {
        let default_path = temp_db("queue-fair-default");
        let archive_path = temp_db("queue-fair-archive");
        let manager = VideoManager::open_default(&default_path, 7200).unwrap();
        manager
            .register_database(
                "archive",
                Arc::new(SqliteVideoDatabase::open(&archive_path).unwrap()),
            )
            .unwrap();

        manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "fairness".into(),
                group: "downloads".into(),
                title: "Default".into(),
                url: "https://example.invalid/default.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        manager
            .queue_download(QueueDownloadRequest {
                database: Some("archive".into()),
                namespace_kind: "module".into(),
                namespace_owner: "fairness".into(),
                group: "downloads".into(),
                title: "Archive".into(),
                url: "https://example.invalid/archive.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();

        let first = manager.next_queued_download(None).unwrap().unwrap();
        let second = manager.next_queued_download(None).unwrap().unwrap();
        assert_ne!(first.asset.database, second.asset.database);
        let mut databases = vec![first.asset.database, second.asset.database];
        databases.sort();
        assert_eq!(databases, vec!["archive", DEFAULT_DATABASE_NAME]);

        let _ = std::fs::remove_dir_all(default_path.parent().unwrap());
        let _ = std::fs::remove_dir_all(archive_path.parent().unwrap());
    }

    #[test]
    fn creates_stable_vm_identity_and_reads_it_back() {'''
    source = replace_once(source, anchor, test, "video fairness regression test")
    path.write_text(source)

    path = Path("crates/video-manager/src/worker.rs")
    source = path.read_text()
    count = source.count("            worker_encoder: Mutex::new(None),\n            live_notify:")
    if count != 2:
        raise SystemExit(f"worker test manager initializers: expected two, found {count}")
    source = source.replace(
        "            worker_encoder: Mutex::new(None),\n            live_notify:",
        "            worker_encoder: Mutex::new(None),\n            worker_database_cursor: AtomicUsize::new(0),\n            live_notify:",
    )
    path.write_text(source)


def stage_live_retry() -> None:
    path = Path("crates/video-manager/src/live_runtime.rs")
    source = path.read_text()
    old = '''                    Ok(false) => {
                        if stop_live_runtime(manager.clone(), driver.clone(), true).await {
                            active = false;
                        }
                        continue;
                    }
'''
    new = '''                    Ok(false) => {
                        if stop_live_runtime(manager.clone(), driver.clone(), true).await {
                            active = false;
                        } else if wait_for_signal(
                            &manager,
                            &mut shutdown,
                            LIVE_RUNTIME_RECOVERY_SCAN,
                        )
                        .await
                        {
                            return stop_live_runtime(manager.clone(), driver.clone(), false).await;
                        }
                        continue;
                    }
'''
    source = replace_once(source, old, new, "failed idle stop retry cadence")
    anchor = "    #[tokio::test]\n    async fn failed_stop_keeps_live_runtime_owned_and_blocks_duplicate_start() {"
    test = '''    #[test]
    fn failed_live_stop_recovery_is_faster_than_default_idle_window() {
        assert!(LIVE_RUNTIME_RECOVERY_SCAN < Duration::from_secs(2 * 60 * 60));
    }

    #[tokio::test]
    async fn failed_stop_keeps_live_runtime_owned_and_blocks_duplicate_start() {'''
    source = replace_once(source, anchor, test, "live stop retry regression test")
    path.write_text(source)


stages = {
    "svc-alias": stage_service_alias,
    "svc-activity": stage_service_activity,
    "video-fairness": stage_video_fairness,
    "live-retry": stage_live_retry,
}

if len(sys.argv) != 2 or sys.argv[1] not in stages:
    raise SystemExit("usage: runtime-bug-batch.py <" + "|".join(stages) + ">")
stages[sys.argv[1]]()
