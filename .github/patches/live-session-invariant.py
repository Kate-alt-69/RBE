from pathlib import Path

path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()

old = "        connection.execute_batch(SCHEMA)?;\n        Ok(Self {"
new = "        connection.execute_batch(SCHEMA)?;\n        ensure_live_session_invariant(&connection)?;\n        Ok(Self {"
if old not in source:
    raise SystemExit("SQLite open schema anchor missing")
source = source.replace(old, new, 1)

sqlite_impl = source.index("impl VideoDatabase for SqliteVideoDatabase {")
fn_start = source.index("    fn insert_live_session(\n", sqlite_impl)
fn_end = source.index("\n    fn get_live_session(", fn_start)
block = source[fn_start:fn_end]
old_tx = '''        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
'''
new_tx = '''        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
'''
if old_tx not in block:
    raise SystemExit("live session transaction anchor missing")
block = block.replace(old_tx, new_tx, 1)
source = source[:fn_start] + block + source[fn_end:]

schema_anchor = 'const SCHEMA: &str = r#"\n'
if schema_anchor not in source:
    raise SystemExit("Video Manager schema anchor missing")
invariant = r'''fn ensure_live_session_invariant(connection: &Connection) -> anyhow::Result<()> {
    let duplicate = connection
        .query_row(
            "SELECT asset_id, COUNT(*) FROM video_live_sessions WHERE state NOT IN ('ended', 'failed') GROUP BY asset_id HAVING COUNT(*) > 1 LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((asset_id, count)) = duplicate {
        anyhow::bail!(
            "Video Manager database contains {count} active live sessions for asset {asset_id:?}; refusing to enable the one-active-session invariant"
        );
    }
    connection.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_video_live_sessions_one_active_per_asset \
         ON video_live_sessions(asset_id) WHERE state NOT IN ('ended', 'failed');",
    )?;
    Ok(())
}

'''
source = source.replace(schema_anchor, invariant + schema_anchor, 1)

last = source.rfind("\n}")
if last < 0:
    raise SystemExit("video-manager test module tail missing")
tests = r'''

    #[test]
    fn sqlite_open_rejects_legacy_duplicate_active_live_sessions() {
        let path = temp_db("live-duplicate-migration");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        let asset_id = Uuid::new_v4().to_string();
        for _ in 0..2 {
            connection
                .execute(
                    "INSERT INTO video_live_sessions (id, asset_id, state) VALUES (?1, ?2, 'reserved')",
                    params![Uuid::new_v4().to_string(), asset_id],
                )
                .unwrap();
        }
        drop(connection);

        let error = match SqliteVideoDatabase::open(&path) {
            Ok(_) => panic!("duplicate active live sessions must fail database open"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("active live sessions"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn separate_sqlite_connections_cannot_reserve_two_active_sessions() {
        let path = temp_db("live-concurrent-reservation");
        let first = Arc::new(SqliteVideoDatabase::open(&path).unwrap());
        let second = Arc::new(SqliteVideoDatabase::open(&path).unwrap());
        let asset = first
            .create_asset(
                DEFAULT_DATABASE_NAME,
                &CreateAssetRequest {
                    database: None,
                    namespace_kind: "module".into(),
                    namespace_owner: "live-concurrency".into(),
                    group: "streams".into(),
                    title: "Concurrent".into(),
                    source_type: VideoSourceType::Live,
                    source_uri: None,
                    metadata: serde_json::Value::Null,
                    initial_state: VideoAssetState::Reserved,
                },
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for database in [first.clone(), second] {
            let barrier = barrier.clone();
            let asset_id = asset.id.clone();
            tasks.push(std::thread::spawn(move || {
                let session = VideoLiveSession {
                    id: Uuid::new_v4().to_string(),
                    asset_id,
                    database: DEFAULT_DATABASE_NAME.into(),
                    state: VideoLiveSessionState::Reserved,
                    ingest_protocol: None,
                    ingest_endpoint: None,
                    playback_endpoint: None,
                    started_at_ms: None,
                    ended_at_ms: None,
                };
                barrier.wait();
                database.insert_live_session(DEFAULT_DATABASE_NAME, &session)
            }));
        }
        barrier.wait();
        let results = tasks
            .into_iter()
            .map(|task| task.join().expect("reservation thread panicked"))
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(first.live_session_counts().unwrap().reserved, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
'''
source = source[:last] + tests + source[last:]
path.write_text(source)
