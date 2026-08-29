from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# live.rs: typed trusted binding + endpoint policy.
path = Path("crates/video-manager/src/live.rs")
source = path.read_text()
anchor = '''#[derive(Debug, Clone)]
pub struct ReserveLiveSessionRequest {
    pub database: Option<String>,
    pub asset_id: String,
}
'''
replacement = anchor + r'''
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLiveIngestProtocol {
    Rtmp,
    Whip,
}

impl VideoLiveIngestProtocol {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Rtmp => "rtmp",
            Self::Whip => "whip",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoLiveBinding {
    pub ingest_protocol: VideoLiveIngestProtocol,
    pub ingest_endpoint: String,
    pub playback_endpoint: Option<String>,
}

fn validate_endpoint(label: &str, value: &str, allowed_schemes: &[&str]) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("Video Manager {label} URL is invalid: {error}"))?;
    if parsed.host_str().is_none() {
        anyhow::bail!("Video Manager {label} URL must include a host");
    }
    if !allowed_schemes.iter().any(|scheme| *scheme == parsed.scheme()) {
        anyhow::bail!(
            "Video Manager {label} URL scheme {:?} is not allowed",
            parsed.scheme()
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        anyhow::bail!(
            "Video Manager {label} URL must not contain credentials or a fragment"
        );
    }
    Ok(())
}

fn validate_live_binding(binding: &VideoLiveBinding) -> anyhow::Result<()> {
    match binding.ingest_protocol {
        VideoLiveIngestProtocol::Rtmp => {
            validate_endpoint("RTMP ingest", &binding.ingest_endpoint, &["rtmp", "rtmps"])?;
        }
        VideoLiveIngestProtocol::Whip => {
            validate_endpoint("WHIP ingest", &binding.ingest_endpoint, &["https"])?;
        }
    }
    if let Some(playback) = &binding.playback_endpoint {
        validate_endpoint("live playback", playback, &["https"])?;
    }
    Ok(())
}
'''
source = replace_once(source, anchor, replacement, "live binding types")

transition_anchor = '''    /// Trusted Rust-only lifecycle transition used by the future ingest/live
    /// runtime. This method is deliberately not exposed as a language function.
    pub fn transition_live_session_trusted(
'''
bind_methods = r'''    /// Trusted media-runtime binding. Language code never receives this API and
    /// therefore cannot forge ingest/playback endpoints or self-promote a
    /// reservation into an active transport.
    pub fn bind_live_session_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
        binding: VideoLiveBinding,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        crate::validate_generated_uuid("live session id", session_id)?;
        validate_live_binding(&binding)?;
        let (database_name, database) = self.resolve_database(database)?;
        database.bind_live_session(&database_name, session_id, &binding)
    }

    pub fn mark_live_session_ready_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        self.transition_live_session_trusted(
            database,
            session_id,
            VideoLiveSessionState::Starting,
            VideoLiveSessionState::Live,
        )
    }

    pub fn mark_live_session_failed_trusted(
        &self,
        database: Option<&str>,
        session_id: &str,
        expected: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        self.transition_live_session_trusted(
            database,
            session_id,
            expected,
            VideoLiveSessionState::Failed,
        )
    }

'''
source = source.replace(transition_anchor, bind_methods + transition_anchor, 1)

tests_end = source.rfind("\n}")
source = source[:tests_end] + r'''

    #[test]
    fn trusted_live_binding_rejects_wrong_schemes_and_url_credentials() {
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Rtmp,
            ingest_endpoint: "https://example.com/live/key".into(),
            playback_endpoint: None,
        })
        .is_err());
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Whip,
            ingest_endpoint: "https://user:pass@example.com/whip".into(),
            playback_endpoint: None,
        })
        .is_err());
        assert!(validate_live_binding(&VideoLiveBinding {
            ingest_protocol: VideoLiveIngestProtocol::Rtmp,
            ingest_endpoint: "rtmps://ingest.example.com/live/key".into(),
            playback_endpoint: Some("https://cdn.example.com/live/index.m3u8".into()),
        })
        .is_ok());
    }
''' + source[tests_end:]
path.write_text(source)

# lib.rs: export binding types + adapter hook + SQLite CAS binding.
path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    '''pub use live::{
    ReserveLiveSessionRequest, VideoLiveRuntimeState, VideoLiveSession,
    VideoLiveSessionCounts, VideoLiveSessionState,
};
''',
    '''pub use live::{
    ReserveLiveSessionRequest, VideoLiveBinding, VideoLiveIngestProtocol, VideoLiveRuntimeState,
    VideoLiveSession, VideoLiveSessionCounts, VideoLiveSessionState,
};
''',
    "live binding exports",
)
trait_anchor = '''    fn transition_live_session(
        &self,
        _database: &str,
        _session_id: &str,
        _expected: VideoLiveSessionState,
        _next: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        anyhow::bail!("Video Manager database adapter does not support live session transitions")
    }
'''
trait_new = trait_anchor + '''    fn bind_live_session(
        &self,
        _database: &str,
        _session_id: &str,
        _binding: &VideoLiveBinding,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        anyhow::bail!("Video Manager database adapter does not support live session binding")
    }
'''
source = replace_once(source, trait_anchor, trait_new, "database live binding hook")

sqlite_anchor = '''    fn transition_live_session(
        &self,
        database: &str,
        session_id: &str,
        expected: VideoLiveSessionState,
        next: VideoLiveSessionState,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
'''
idx = source.index(sqlite_anchor)
next_method = source.index("\n    fn live_session_counts", idx)
transition_block = source[idx:next_method]
bind_impl = r'''
    fn bind_live_session(
        &self,
        database: &str,
        session_id: &str,
        binding: &VideoLiveBinding,
    ) -> anyhow::Result<Option<VideoLiveSession>> {
        validate_generated_uuid("live session id", session_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE video_live_sessions SET state = 'starting', ingest_protocol = ?1, ingest_endpoint = ?2, playback_endpoint = ?3 WHERE id = ?4 AND state = 'reserved' AND ingest_protocol IS NULL AND ingest_endpoint IS NULL",
            params![
                binding.ingest_protocol.as_str(),
                binding.ingest_endpoint,
                binding.playback_endpoint,
                session_id,
            ],
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
'''
source = source[:next_method] + bind_impl + source[next_method:]

tests_end = source.rfind("\n}")
source = source[:tests_end] + r'''

    #[test]
    fn trusted_live_binding_atomically_moves_reservation_to_starting() {
        let path = temp_db("live-binding");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "streamer".into(),
                group: "live".into(),
                title: "Bound".into(),
                source_type: VideoSourceType::Live,
                source_uri: None,
                metadata: serde_json::Value::Null,
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        let session = manager
            .reserve_live_session(ReserveLiveSessionRequest {
                database: None,
                asset_id: asset.id,
            })
            .unwrap();
        let bound = manager
            .bind_live_session_trusted(
                None,
                &session.id,
                VideoLiveBinding {
                    ingest_protocol: VideoLiveIngestProtocol::Rtmp,
                    ingest_endpoint: "rtmp://127.0.0.1:1935/live/opaque-key".into(),
                    playback_endpoint: Some("https://cdn.example.invalid/live/index.m3u8".into()),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(bound.state, VideoLiveSessionState::Starting);
        assert_eq!(bound.ingest_protocol.as_deref(), Some("rtmp"));
        assert!(manager
            .bind_live_session_trusted(
                None,
                &session.id,
                VideoLiveBinding {
                    ingest_protocol: VideoLiveIngestProtocol::Rtmp,
                    ingest_endpoint: "rtmp://127.0.0.1:1935/live/another".into(),
                    playback_endpoint: None,
                },
            )
            .unwrap()
            .is_none());
        let live = manager
            .mark_live_session_ready_trusted(None, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(live.state, VideoLiveSessionState::Live);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)
