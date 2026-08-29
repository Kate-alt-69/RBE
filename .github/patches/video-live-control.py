from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# --- video-manager/lib.rs -------------------------------------------------
path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "mod ffprobe;\nmod live.rs;" if False else "mod ffprobe;\n",
    "mod ffprobe;\nmod live;\n",
    "live module declaration",
)
source = replace_once(
    source,
    "pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};\n",
    '''pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};
pub use live::{
    ReserveLiveSessionRequest, VideoLiveRuntimeState, VideoLiveSession,
    VideoLiveSessionCounts, VideoLiveSessionState,
};
''',
    "live public exports",
)

trait_anchor = '''    fn list_variants(&self, _asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
        Ok(Vec::new())
    }
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
trait_replacement = '''    fn list_variants(&self, _asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
        Ok(Vec::new())
    }
    fn insert_live_session(
        &self,
        _database: &str,
        _session: &VideoLiveSession,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Video Manager database adapter does not support live sessions")
    }
    fn get_live_session(
        &self,
        _database: &str,
        _session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        Ok(None)
    }
    fn transition_live_session(
        &self,
        _database: &str,
        _session_id: &str,
        _expected: VideoLiveSessionState,
        _next: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        anyhow::bail!("Video Manager database adapter does not support live session transitions")
    }
    fn live_session_counts(&self) -> anyhow::Result<VideoLiveSessionCounts> {
        Ok(VideoLiveSessionCounts::default())
    }
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
source = replace_once(source, trait_anchor, trait_replacement, "VideoDatabase live hooks")

status_old = '''pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub download_worker: VideoDownloadWorkerStatus,
    pub live_runtime: &'static str,
    pub live_idle_secs: u64,
}
'''
status_new = '''pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub download_worker: VideoDownloadWorkerStatus,
    pub live_runtime: VideoLiveRuntimeState,
    pub live_sessions: VideoLiveSessionCounts,
    pub live_idle_secs: u64,
}
'''
source = replace_once(source, status_old, status_new, "VideoManagerStatus live fields")

sqlite_insert_anchor = '''    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>> {
'''
sqlite_live_methods = r'''    fn insert_live_session(
        &self,
        _database: &str,
        session: &VideoLiveSession,
    ) -> anyhow::Result<()> {
        validate_generated_uuid("live session id", &session.id)?;
        validate_generated_uuid("live session asset id", &session.asset_id)?;
        if session.state != VideoLiveSessionState::Reserved
            || session.ingest_protocol.is_some()
            || session.ingest_endpoint.is_some()
            || session.playback_endpoint.is_some()
            || session.started_at_ms.is_some()
            || session.ended_at_ms.is_some()
        {
            anyhow::bail!("Video Manager new live sessions must begin as an unbound reservation");
        }

        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let asset = transaction
            .query_row(
                "SELECT source_type, state FROM video_assets WHERE id = ?1",
                params![session.asset_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((source_type, asset_state)) = asset else {
            anyhow::bail!("Video Manager live session references a missing asset");
        };
        if source_type != "live" || asset_state != "reserved" {
            anyhow::bail!("Video Manager live session requires a reserved live source asset");
        }
        let existing = transaction.query_row(
            "SELECT COUNT(*) FROM video_live_sessions WHERE asset_id = ?1 AND state NOT IN ('ended', 'failed')",
            params![session.asset_id],
            |row| row.get::<_, i64>(0),
        )?;
        if existing != 0 {
            anyhow::bail!("Video Manager live asset already has an active reservation/session");
        }
        transaction.execute(
            "INSERT INTO video_live_sessions (id, asset_id, state, ingest_protocol, ingest_endpoint, playback_endpoint, started_at_ms, ended_at_ms) VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, NULL)",
            params![session.id, session.asset_id, session.state.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn get_live_session(
        &self,
        database: &str,
        session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        validate_generated_uuid("live session id", session_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let row = connection
            .query_row(
                "SELECT id, asset_id, state, ingest_protocol, ingest_endpoint, playback_endpoint, started_at_ms, ended_at_ms FROM video_live_sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(id, asset_id, state, ingest_protocol, ingest_endpoint, playback_endpoint, started_at_ms, ended_at_ms)| {
                Ok(VideoLiveSession {
                    id,
                    asset_id,
                    database: database.to_string(),
                    state: VideoLiveSessionState::parse(&state)?,
                    ingest_protocol,
                    ingest_endpoint,
                    playback_endpoint,
                    started_at_ms,
                    ended_at_ms,
                })
            },
        )
        .transpose()
    }

    fn transition_live_session(
        &self,
        database: &str,
        session_id: &str,
        expected: VideoLiveSessionState,
        next: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        validate_generated_uuid("live session id", session_id)?;
        crate::live::validate_live_transition(expected, next)?;
        let now = now_ms();
        let started = (next == VideoLiveSessionState::Live).then_some(now);
        let ended = matches!(next, VideoLiveSessionState::Ended | VideoLiveSessionState::Failed)
            .then_some(now);
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE video_live_sessions SET state = ?1, started_at_ms = COALESCE(started_at_ms, ?2), ended_at_ms = COALESCE(ended_at_ms, ?3) WHERE id = ?4 AND state = ?5",
            params![next.as_str(), started, ended, session_id, expected.as_str()],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        let row = transaction
            .query_row(
                "SELECT id, asset_id, state, ingest_protocol, ingest_endpoint, playback_endpoint, started_at_ms, ended_at_ms FROM video_live_sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        transaction.commit()?;
        row.map(
            |(id, asset_id, state, ingest_protocol, ingest_endpoint, playback_endpoint, started_at_ms, ended_at_ms)| {
                Ok(VideoLiveSession {
                    id,
                    asset_id,
                    database: database.to_string(),
                    state: VideoLiveSessionState::parse(&state)?,
                    ingest_protocol,
                    ingest_endpoint,
                    playback_endpoint,
                    started_at_ms,
                    ended_at_ms,
                })
            },
        )
        .transpose()
    }

    fn live_session_counts(&self) -> anyhow::Result<VideoLiveSessionCounts> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let (reserved, starting, live, stopping) = connection.query_row(
            "SELECT SUM(CASE WHEN state = 'reserved' THEN 1 ELSE 0 END), SUM(CASE WHEN state = 'starting' THEN 1 ELSE 0 END), SUM(CASE WHEN state = 'live' THEN 1 ELSE 0 END), SUM(CASE WHEN state = 'stopping' THEN 1 ELSE 0 END) FROM video_live_sessions",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ))
            },
        )?;
        Ok(VideoLiveSessionCounts {
            reserved: u64::try_from(reserved).context("Video Manager reserved live count is negative")?,
            starting: u64::try_from(starting).context("Video Manager starting live count is negative")?,
            live: u64::try_from(live).context("Video Manager active live count is negative")?,
            stopping: u64::try_from(stopping).context("Video Manager stopping live count is negative")?,
        })
    }

'''
source = source.replace(sqlite_insert_anchor, sqlite_live_methods + sqlite_insert_anchor, 1)

status_anchor = '''        let worker_state = self.worker_state()?;
        let queued_downloads = self.queued_download_count()?;
        Ok(VideoManagerStatus {
'''
status_replace = '''        let worker_state = self.worker_state()?;
        let queued_downloads = self.queued_download_count()?;
        let live_sessions = self.live_session_counts()?;
        let live_runtime = VideoLiveRuntimeState::from_counts(live_sessions);
        Ok(VideoManagerStatus {
'''
source = replace_once(source, status_anchor, status_replace, "status live counts")
source = replace_once(
    source,
    '''            live_runtime: "sleeping",
            live_idle_secs: self.live_idle_secs,
''',
    '''            live_runtime,
            live_sessions,
            live_idle_secs: self.live_idle_secs,
''',
    "status live output",
)
source = replace_once(
    source,
    '''CREATE INDEX IF NOT EXISTS idx_video_jobs_state ON video_jobs(state);
"#;
''',
    '''CREATE INDEX IF NOT EXISTS idx_video_jobs_state ON video_jobs(state);
CREATE INDEX IF NOT EXISTS idx_video_live_sessions_asset ON video_live_sessions(asset_id);
CREATE INDEX IF NOT EXISTS idx_video_live_sessions_state ON video_live_sessions(state);
"#;
''',
    "live schema indexes",
)

tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("video-manager tests tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn live_reservations_persist_and_trusted_transitions_are_fail_closed() {
        let path = temp_db("live-session");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "streamer".into(),
                group: "live".into(),
                title: "Broadcast".into(),
                source_type: VideoSourceType::Live,
                source_uri: None,
                metadata: serde_json::Value::Null,
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        let session = manager
            .reserve_live_session(ReserveLiveSessionRequest {
                database: None,
                asset_id: asset.id.clone(),
            })
            .unwrap();
        assert_eq!(session.state, VideoLiveSessionState::Reserved);
        assert_eq!(manager.live_session_counts().unwrap().reserved, 1);
        assert!(manager
            .transition_live_session_trusted(
                None,
                &session.id,
                VideoLiveSessionState::Reserved,
                VideoLiveSessionState::Live,
            )
            .is_err());
        let starting = manager
            .transition_live_session_trusted(
                None,
                &session.id,
                VideoLiveSessionState::Reserved,
                VideoLiveSessionState::Starting,
            )
            .unwrap()
            .unwrap();
        assert_eq!(starting.state, VideoLiveSessionState::Starting);
        let live = manager
            .transition_live_session_trusted(
                None,
                &session.id,
                VideoLiveSessionState::Starting,
                VideoLiveSessionState::Live,
            )
            .unwrap()
            .unwrap();
        assert_eq!(live.state, VideoLiveSessionState::Live);
        assert!(live.started_at_ms.is_some());
        let status = manager.status().unwrap();
        assert_eq!(status.live_runtime, VideoLiveRuntimeState::Active);
        assert_eq!(status.live_sessions.live, 1);
        let stopping = manager
            .request_end_live_session(None, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(stopping.state, VideoLiveSessionState::Stopping);
        let ended = manager
            .transition_live_session_trusted(
                None,
                &session.id,
                VideoLiveSessionState::Stopping,
                VideoLiveSessionState::Ended,
            )
            .unwrap()
            .unwrap();
        assert!(ended.ended_at_ms.is_some());
        assert_eq!(manager.status().unwrap().live_runtime, VideoLiveRuntimeState::Sleeping);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn live_asset_rejects_duplicate_nonterminal_sessions() {
        let path = temp_db("live-duplicate");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "streamer".into(),
                group: "live".into(),
                title: "Broadcast".into(),
                source_type: VideoSourceType::Live,
                source_uri: None,
                metadata: serde_json::Value::Null,
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        manager
            .reserve_live_session(ReserveLiveSessionRequest {
                database: None,
                asset_id: asset.id.clone(),
            })
            .unwrap();
        assert!(manager
            .reserve_live_session(ReserveLiveSessionRequest {
                database: None,
                asset_id: asset.id,
            })
            .is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# --- core/video_language.rs ----------------------------------------------
path = Path("crates/core/src/video_language.rs")
source = path.read_text()
source = replace_once(
    source,
    '''    CreateAssetRequest, QueueDownloadRequest, VideoAssetState, VideoManager, VideoSourceType,
    VideoVariant,
''',
    '''    CreateAssetRequest, QueueDownloadRequest, ReserveLiveSessionRequest, VideoAssetState,
    VideoLiveSession, VideoManager, VideoSourceType, VideoVariant,
''',
    "Video language live imports",
)
insert_before = '''            "create" => {
'''
live_arms = r'''            "reserveLive" | "reserve_live" => {
                expect_arity_range(function, args, 1, 2)?;
                let asset_id = required_string(args, 0, "asset id")?;
                let database = optional_string(args.get(1), "database")?;
                let asset = manager
                    .get_asset(database.as_deref(), asset_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new("VID3002", "Video Manager live asset does not exist")
                    })?;
                ensure_owned(module_owner, &asset.namespace)?;
                let session = manager
                    .reserve_live_session(ReserveLiveSessionRequest {
                        database,
                        asset_id: asset_id.to_string(),
                    })
                    .map_err(operation_error)?;
                Ok(public_live_session_value(session))
            }
            "liveSession" | "live_session" => {
                expect_arity_range(function, args, 1, 2)?;
                let session_id = required_string(args, 0, "live session id")?;
                let database = optional_string(args.get(1), "database")?;
                let Some(session) = manager
                    .get_live_session(database.as_deref(), session_id)
                    .map_err(operation_error)?
                else {
                    return Ok(Value::Null);
                };
                let asset = manager
                    .get_asset(Some(&session.database), &session.asset_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new(
                            "VID3002",
                            "Video Manager live session references a missing asset",
                        )
                    })?;
                ensure_owned(module_owner, &asset.namespace)?;
                Ok(public_live_session_value(session))
            }
            "endLive" | "end_live" => {
                expect_arity_range(function, args, 1, 2)?;
                let session_id = required_string(args, 0, "live session id")?;
                let database = optional_string(args.get(1), "database")?;
                let Some(existing) = manager
                    .get_live_session(database.as_deref(), session_id)
                    .map_err(operation_error)?
                else {
                    return Ok(Value::Null);
                };
                let asset = manager
                    .get_asset(Some(&existing.database), &existing.asset_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new(
                            "VID3002",
                            "Video Manager live session references a missing asset",
                        )
                    })?;
                ensure_owned(module_owner, &asset.namespace)?;
                let session = manager
                    .request_end_live_session(Some(&existing.database), session_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new(
                            "VID3002",
                            "Video Manager live session disappeared during end request",
                        )
                    })?;
                Ok(public_live_session_value(session))
            }
'''
source = source.replace(insert_before, live_arms + insert_before, 1)

public_anchor = '''fn public_variant_value(variant: VideoVariant) -> Value {
'''
live_public = r'''fn public_live_session_value(session: VideoLiveSession) -> Value {
    serde_json::json!({
        "id": session.id,
        "assetId": session.asset_id,
        "state": session.state,
        "startedAtMs": session.started_at_ms,
        "endedAtMs": session.ended_at_ms,
    })
}

'''
source = source.replace(public_anchor, live_public + public_anchor, 1)

tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Video language tests tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn live_language_is_owner_scoped_and_cannot_self_activate_transport() {
        let path = temp_db("live-owner");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager.clone()));
        let asset = language
            .call(
                "stream.owner",
                "create",
                &[
                    Value::String("live".into()),
                    Value::String("Broadcast".into()),
                    Value::String("live".into()),
                ],
            )
            .unwrap();
        let asset_id = asset["id"].as_str().unwrap().to_string();
        let session = language
            .call(
                "stream.owner",
                "reserveLive",
                &[Value::String(asset_id)],
            )
            .unwrap();
        assert_eq!(session["state"], "reserved");
        assert!(session.get("ingestEndpoint").is_none());
        assert!(session.get("playbackEndpoint").is_none());
        let session_id = session["id"].as_str().unwrap().to_string();
        let denied = language
            .call(
                "other.module",
                "liveSession",
                &[Value::String(session_id.clone())],
            )
            .unwrap_err();
        assert_eq!(denied.code, "VID3003");
        let ended = language
            .call(
                "stream.owner",
                "endLive",
                &[Value::String(session_id.clone())],
            )
            .unwrap();
        assert_eq!(ended["state"], "ended");
        let stored = manager.get_live_session(None, &session_id).unwrap().unwrap();
        assert_eq!(stored.state, video_manager::VideoLiveSessionState::Ended);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# --- route-engine/modules.rs ---------------------------------------------
path = Path("crates/route-engine/src/modules.rs")
source = path.read_text()
old = '''                | "queueDownload"
                | "queue_download"
        ),
'''
new = '''                | "queueDownload"
                | "queue_download"
                | "reserveLive"
                | "reserve_live"
                | "liveSession"
                | "live_session"
                | "endLive"
                | "end_live"
        ),
'''
source = replace_once(source, old, new, "Video Manager builtin function list")
path.write_text(source)
