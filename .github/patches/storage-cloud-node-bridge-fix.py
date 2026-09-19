from pathlib import Path
import sys


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


def pre() -> None:
    replace_once(
        ".github/patches/storage-cloud-node-bridge.py",
        'insert_anchor = "\\n    #[test]\\n    fn sync_header_round_trip() {"',
        'insert_anchor = "\\n    #[test]\\n    fn recovery_plan_orders_folder_video_then_file() {"',
        "sync priority staging anchor",
    )
    replace_once(
        ".github/patches/storage-cloud-node-bridge-race-fix.py",
        '''        .create(true)\n        .read(true)\n        .write(true)\n        .open(locks.join(format!("{id}.lock")))?;''',
        '''        .create(true)\n        .read(true)\n        .write(true)\n        .truncate(false)\n        .open(locks.join(format!("{id}.lock")))?;''',
        "journal lock open intent",
    )


def post() -> None:
    replace_once(
        "engine/crates/cloud-node/src/sync.rs",
        '''    pub fn priority_ordered(&self) -> impl Iterator<Item = &SyncObject> {
        let sort = |objects: &[SyncObject]| {
            let mut values = objects.iter().collect::<Vec<_>>();
            values.sort_by(|left, right| {
                left.priority
                    .cmp(&right.priority)
                    .then(left.logical_path.cmp(&right.logical_path))
                    .then(left.object_key.cmp(&right.object_key))
            });
            values
        };
        sort(&self.folders)
            .into_iter()
            .chain(sort(&self.videos))
            .chain(sort(&self.files))
    }
''',
        '''    pub fn priority_ordered(&self) -> impl Iterator<Item = &SyncObject> {
        let mut folders = self.folders.iter().collect::<Vec<_>>();
        folders.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then(left.logical_path.cmp(&right.logical_path))
                .then(left.object_key.cmp(&right.object_key))
        });
        let mut videos = self.videos.iter().collect::<Vec<_>>();
        videos.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then(left.logical_path.cmp(&right.logical_path))
                .then(left.object_key.cmp(&right.object_key))
        });
        let mut files = self.files.iter().collect::<Vec<_>>();
        files.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then(left.logical_path.cmp(&right.logical_path))
                .then(left.object_key.cmp(&right.object_key))
        });
        folders.into_iter().chain(videos).chain(files)
    }
''',
        "priority iterator lifetime fix",
    )

    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '''    let project_root = cloud_node_project_root(&config_path)?;
    let ingested = store.ingest_storage_journal(&project_root)?;
    if ingested > 0 {
        eprintln!(
            "cloud_node: ingested {ingested} project-root Storage write(s) from {}",
            project_root.display()
        );
    }
    match command {
''',
        '''    let project_root = cloud_node_project_root(&config_path)?;
    ingest_project_writes(&store, &project_root)?;
    match command {
''',
        "startup journal helper",
    )
    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '        "run" => run_daemon(&settings, &store).await?,\n',
        '        "run" => run_daemon(&settings, &store, &project_root).await?,\n',
        "daemon ProjectRoot call",
    )
    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '''async fn run_daemon(settings: &CloudNodeSettings, store: &CloudNodeStore) -> anyhow::Result<()> {
    if settings.provider.is_some() {
        return run_provider_daemon(settings, store).await;
    }
    run_peer_daemon(settings, store).await
}

async fn run_peer_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<()> {
''',
        '''fn ingest_project_writes(store: &CloudNodeStore, project_root: &Path) -> anyhow::Result<()> {
    let ingested = store.ingest_storage_journal(project_root)?;
    if ingested > 0 {
        eprintln!(
            "cloud_node: ingested {ingested} project-root Storage write(s) from {}",
            project_root.display()
        );
    }
    Ok(())
}

async fn run_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
) -> anyhow::Result<()> {
    if settings.provider.is_some() {
        return run_provider_daemon(settings, store, project_root).await;
    }
    run_peer_daemon(settings, store, project_root).await
}

async fn run_peer_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
) -> anyhow::Result<()> {
''',
        "daemon ProjectRoot signatures",
    )
    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '''        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream or provider"))?;
    loop {
        match probe_upstream(settings).await {
''',
        '''        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node run mode requires an upstream or provider"))?;
    loop {
        ingest_project_writes(store, project_root)?;
        match probe_upstream(settings).await {
''',
        "peer daemon journal cycle",
    )
    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '''async fn run_provider_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
) -> anyhow::Result<()> {
''',
        '''async fn run_provider_daemon(
    settings: &CloudNodeSettings,
    store: &CloudNodeStore,
    project_root: &Path,
) -> anyhow::Result<()> {
''',
        "provider daemon ProjectRoot signature",
    )
    replace_once(
        "engine/crates/cloud-node/src/main.rs",
        '''    let mut retry_delay_ms = provider.reconnect_delay_ms;
    let mut write_probe_verified = false;
    loop {
        let result = if provider.sync_on_connect {
''',
        '''    let mut retry_delay_ms = provider.reconnect_delay_ms;
    let mut write_probe_verified = false;
    loop {
        ingest_project_writes(store, project_root)?;
        let result = if provider.sync_on_connect {
''',
        "provider daemon journal cycle",
    )
    replace_once(
        "doc/storage.md",
        "Cloud Node ingests the current file idempotently, records Data-Level as local scheduling metadata, and acknowledges only the exact intent it consumed.",
        "Cloud Node ingests the current file idempotently at startup and before every daemon sync cycle, records Data-Level as local scheduling metadata, and acknowledges only the exact intent it consumed.",
        "continuous journal docs",
    )


if len(sys.argv) != 2 or sys.argv[1] not in {"pre", "post"}:
    raise SystemExit("usage: storage-cloud-node-bridge-fix.py <pre|post>")

if sys.argv[1] == "pre":
    pre()
else:
    post()
