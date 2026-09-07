//! Capability-scoped language facade for the global Video Manager.
//!
//! The language never receives a raw database handle, filesystem path, or
//! process-spawn primitive. Module ownership is supplied by the evaluator and
//! is used to pin mutating operations to `module:<owner>` namespaces.

use std::sync::Arc;

use serde_json::Value;
use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, ReserveLiveSessionRequest, VideoAssetState,
    VideoLiveSession, VideoManager, VideoSourceType, VideoVariant,
};

#[derive(Clone)]
pub struct VideoLanguage {
    manager: Option<Arc<VideoManager>>,
}

impl VideoLanguage {
    pub fn new(manager: Option<Arc<VideoManager>>) -> Self {
        Self { manager }
    }

    pub fn call(
        &self,
        module_owner: &str,
        function: &str,
        args: &[Value],
    ) -> Result<Value, VideoLanguageError> {
        let manager = self.manager()?;
        match function {
            "status" => {
                expect_arity_range(function, args, 0, 0)?;
                json_result(serde_json::to_value(
                    manager.status().map_err(operation_error)?,
                ))
            }
            "databaseHealth" | "database_health" => {
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
            "get" => {
                expect_arity_range(function, args, 1, 2)?;
                let asset_id = required_string(args, 0, "asset id")?;
                let database = optional_string(args.get(1), "database")?;
                let asset = manager
                    .get_asset(database.as_deref(), asset_id)
                    .map_err(operation_error)?;
                match asset {
                    None => Ok(Value::Null),
                    Some(asset) => {
                        ensure_owned(module_owner, &asset.namespace)?;
                        Ok(public_asset_value(asset))
                    }
                }
            }
            "job" => {
                expect_arity_range(function, args, 1, 2)?;
                let job_id = required_string(args, 0, "job id")?;
                let database = optional_string(args.get(1), "database")?;
                let job = manager
                    .get_job(database.as_deref(), job_id)
                    .map_err(operation_error)?;
                let Some(job) = job else {
                    return Ok(Value::Null);
                };
                let asset = manager
                    .get_asset(database.as_deref(), &job.asset_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new(
                            "VID3002",
                            "Video Manager job references a missing asset",
                        )
                    })?;
                ensure_owned(module_owner, &asset.namespace)?;
                Ok(public_job_value(job))
            }
            "variants" => {
                expect_arity_range(function, args, 1, 2)?;
                let asset_id = required_string(args, 0, "asset id")?;
                let database = optional_string(args.get(1), "database")?;
                let asset = manager
                    .get_asset(database.as_deref(), asset_id)
                    .map_err(operation_error)?;
                let Some(asset) = asset else {
                    return Ok(Value::Null);
                };
                ensure_owned(module_owner, &asset.namespace)?;
                let variants = manager
                    .list_variants(database.as_deref(), asset_id)
                    .map_err(operation_error)?;
                Ok(Value::Array(
                    variants.into_iter().map(public_variant_value).collect(),
                ))
            }
            "reserveLive" | "reserve_live" => {
                expect_arity_range(function, args, 1, 2)?;
                let asset_id = required_string(args, 0, "asset id")?;
                let database = optional_string(args.get(1), "database")?;
                let asset = manager
                    .get_asset(database.as_deref(), asset_id)
                    .map_err(operation_error)?
                    .ok_or_else(|| {
                        VideoLanguageError::new(
                            "VID3002",
                            "Video Manager live asset does not exist",
                        )
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
            "create" => {
                expect_arity_range(function, args, 3, 6)?;
                let group = required_string(args, 0, "group")?.to_string();
                let title = required_string(args, 1, "title")?.to_string();
                let source_type = parse_source_type(required_string(args, 2, "source type")?)?;
                let source_uri = optional_string(args.get(3), "source URI")?;
                let metadata = args.get(4).cloned().unwrap_or(Value::Null);
                let database = optional_string(args.get(5), "database")?;
                let asset = manager
                    .create_asset(CreateAssetRequest {
                        database,
                        namespace_kind: "module".into(),
                        namespace_owner: module_owner.to_string(),
                        group,
                        title,
                        source_type,
                        source_uri,
                        metadata,
                        initial_state: VideoAssetState::Reserved,
                    })
                    .map_err(operation_error)?;
                Ok(public_asset_value(asset))
            }
            "queueDownload" | "queue_download" => {
                expect_arity_range(function, args, 3, 5)?;
                let group = required_string(args, 0, "group")?.to_string();
                let title = required_string(args, 1, "title")?.to_string();
                let url = required_string(args, 2, "URL")?.to_string();
                let metadata = args.get(3).cloned().unwrap_or(Value::Null);
                let database = optional_string(args.get(4), "database")?;
                let queued = manager
                    .queue_download(QueueDownloadRequest {
                        database,
                        namespace_kind: "module".into(),
                        namespace_owner: module_owner.to_string(),
                        group,
                        title,
                        url,
                        metadata,
                    })
                    .map_err(operation_error)?;
                Ok(serde_json::json!({
                    "asset": public_asset_value(queued.asset),
                    "job": public_job_value(queued.job),
                }))
            }
            other => Err(VideoLanguageError::new(
                "VID3001",
                format!("unknown Video Manager language function {other:?}"),
            )),
        }
    }

    fn manager(&self) -> Result<&VideoManager, VideoLanguageError> {
        self.manager
            .as_deref()
            .ok_or_else(|| VideoLanguageError::new("VID3000", "Video Manager is disabled"))
    }
}

#[derive(Debug, Clone)]
pub struct VideoLanguageError {
    pub code: &'static str,
    pub message: String,
}

impl VideoLanguageError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VideoLanguageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for VideoLanguageError {}

fn ensure_owned(module_owner: &str, namespace: &str) -> Result<(), VideoLanguageError> {
    let expected = format!("module:{module_owner}");
    if namespace == expected {
        Ok(())
    } else {
        Err(VideoLanguageError::new(
            "VID3003",
            format!("asset belongs to namespace {namespace:?}, not {expected:?}"),
        ))
    }
}

fn parse_source_type(value: &str) -> Result<VideoSourceType, VideoLanguageError> {
    match value {
        "upload" => Ok(VideoSourceType::Upload),
        "local" => Ok(VideoSourceType::Local),
        "generated" => Ok(VideoSourceType::Generated),
        "live" => Ok(VideoSourceType::Live),
        "recorded_live" | "recordedLive" => Ok(VideoSourceType::RecordedLive),
        "download" => Err(VideoLanguageError::new(
            "VID3004",
            "use vm.queueDownload() or video-manager.queueDownload() for download assets so they enter quarantine",
        )),
        other => Err(VideoLanguageError::new(
            "VID3004",
            format!("unsupported video source type {other:?}"),
        )),
    }
}

fn expect_arity_range(
    function: &str,
    args: &[Value],
    minimum: usize,
    maximum: usize,
) -> Result<(), VideoLanguageError> {
    if (minimum..=maximum).contains(&args.len()) {
        Ok(())
    } else {
        Err(VideoLanguageError::new(
            "VID3001",
            format!(
                "Video Manager {function}() expects {minimum}..={maximum} argument(s), got {}",
                args.len()
            ),
        ))
    }
}

fn required_string<'a>(
    args: &'a [Value],
    index: usize,
    label: &str,
) -> Result<&'a str, VideoLanguageError> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(VideoLanguageError::new(
            "VID3001",
            format!("Video Manager argument {index} ({label}) must be a string"),
        )),
    }
}

fn optional_string(
    value: Option<&Value>,
    label: &str,
) -> Result<Option<String>, VideoLanguageError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(VideoLanguageError::new(
            "VID3001",
            format!("optional Video Manager {label} must be a string or null"),
        )),
    }
}

fn public_asset_value(asset: video_manager::VideoAsset) -> Value {
    serde_json::json!({
        "id": asset.id,
        "uri": asset.uri,
        "database": asset.database,
        "namespace": asset.namespace,
        "group": asset.group,
        "title": asset.title,
        "state": asset.state,
        "sourceType": asset.source_type,
        "metadata": asset.metadata,
        "createdAtMs": asset.created_at_ms,
        "updatedAtMs": asset.updated_at_ms,
    })
}

fn public_job_value(job: video_manager::VideoJob) -> Value {
    serde_json::json!({
        "id": job.id,
        "assetId": job.asset_id,
        "jobType": job.job_type,
        "state": job.state,
        "progress": job.progress,
        "attempts": job.attempts,
        "failed": job.error.is_some(),
        "createdAtMs": job.created_at_ms,
        "updatedAtMs": job.updated_at_ms,
    })
}

fn public_live_session_value(session: VideoLiveSession) -> Value {
    serde_json::json!({
        "id": session.id,
        "assetId": session.asset_id,
        "state": session.state,
        "startedAtMs": session.started_at_ms,
        "endedAtMs": session.ended_at_ms,
    })
}

fn public_variant_value(variant: VideoVariant) -> Value {
    serde_json::json!({
        "id": variant.id,
        "assetId": variant.asset_id,
        "profile": variant.profile,
        "codec": variant.codec,
        "width": variant.width,
        "height": variant.height,
        "fps": variant.fps,
        "bitrate": variant.bitrate,
        "sizeBytes": variant.size_bytes,
        "state": variant.state,
        "createdAtMs": variant.created_at_ms,
        "updatedAtMs": variant.updated_at_ms,
    })
}

fn operation_error(error: anyhow::Error) -> VideoLanguageError {
    tracing::warn!(error = %error, "Video Manager language operation failed");
    VideoLanguageError::new("VID3002", "Video Manager operation failed")
}

fn json_result(value: serde_json::Result<Value>) -> Result<Value, VideoLanguageError> {
    value.map_err(|error| VideoLanguageError::new("VID3002", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-video-language-{name}-{}-{nonce}.db",
            std::process::id()
        ))
    }

    #[test]
    fn creates_and_reads_only_the_calling_modules_assets() {
        let path = temp_db("ownership");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let created = language
            .call(
                "learning.catalog",
                "create",
                &[
                    Value::String("tutorials".into()),
                    Value::String("Intro".into()),
                    Value::String("generated".into()),
                    Value::Null,
                    serde_json::json!({"lesson": 1}),
                ],
            )
            .unwrap();
        assert_eq!(created["namespace"], "module:learning.catalog");
        let asset_id = created["id"].as_str().unwrap().to_string();
        let loaded = language
            .call(
                "learning.catalog",
                "get",
                &[Value::String(asset_id.clone())],
            )
            .unwrap();
        assert_eq!(loaded["id"], asset_id);
        let error = language
            .call("another.module", "get", &[Value::String(asset_id)])
            .unwrap_err();
        assert_eq!(error.code, "VID3003");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn queue_download_only_creates_quarantined_control_plane_records() {
        let path = temp_db("queue");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let queued = language
            .call(
                "learning.catalog",
                "queueDownload",
                &[
                    Value::String("tutorials".into()),
                    Value::String("Remote".into()),
                    Value::String("https://example.invalid/video.mp4".into()),
                ],
            )
            .unwrap();
        assert_eq!(queued["asset"]["state"], "quarantined");
        assert_eq!(queued["job"]["state"], "queued");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn variant_language_view_never_exposes_storage_path() {
        let variant = VideoVariant {
            id: "variant-id".into(),
            asset_id: "asset-id".into(),
            profile: "standard".into(),
            codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            fps: Some(30.0),
            bitrate: Some(4_000_000),
            size_bytes: 1_000_000,
            path: "secret/internal/primary.mp4".into(),
            state: "ready".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let value = public_variant_value(variant);
        assert!(value.get("path").is_none());
        assert_eq!(value["profile"], "standard");
        assert_eq!(value["assetId"], "asset-id");
    }

    #[test]
    fn variants_enforces_asset_namespace_ownership() {
        let path = temp_db("variant-ownership");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let created = language
            .call(
                "owner.module",
                "create",
                &[
                    Value::String("clips".into()),
                    Value::String("Private".into()),
                    Value::String("generated".into()),
                ],
            )
            .unwrap();
        let asset_id = created["id"].as_str().unwrap().to_string();
        let error = language
            .call("other.module", "variants", &[Value::String(asset_id)])
            .unwrap_err();
        assert_eq!(error.code, "VID3003");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn job_language_view_hides_internal_error_text() {
        let job = video_manager::VideoJob {
            id: "job-id".into(),
            asset_id: "asset-id".into(),
            job_type: "download".into(),
            state: "failed".into(),
            progress: 0.45,
            attempts: 1,
            error: Some("/secret/internal/path ffmpeg failed".into()),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let value = public_job_value(job);
        assert!(value.get("error").is_none());
        assert_eq!(value["failed"], true);
        assert_eq!(value["progress"], 0.45);
    }

    #[test]
    fn job_lookup_enforces_asset_namespace_ownership() {
        let path = temp_db("job-ownership");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let queued = language
            .call(
                "owner.module",
                "queueDownload",
                &[
                    Value::String("clips".into()),
                    Value::String("Private".into()),
                    Value::String("https://example.invalid/private.mp4".into()),
                ],
            )
            .unwrap();
        let job_id = queued["job"]["id"].as_str().unwrap().to_string();
        let own = language
            .call("owner.module", "job", &[Value::String(job_id.clone())])
            .unwrap();
        assert_eq!(own["id"], job_id);
        assert!(own.get("error").is_none());
        let error = language
            .call("other.module", "job", &[Value::String(job_id)])
            .unwrap_err();
        assert_eq!(error.code, "VID3003");
        let _ = std::fs::remove_file(path);
    }

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
            .call("stream.owner", "reserveLive", &[Value::String(asset_id)])
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
        let stored = manager
            .get_live_session(None, &session_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, video_manager::VideoLiveSessionState::Ended);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

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
        assert!(!health.to_string().contains(path.to_string_lossy().as_ref()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn operation_errors_do_not_expose_trusted_internal_details() {
        let error = operation_error(anyhow::anyhow!(
            "open /srv/rbe/private/video-manager.db failed with secret adapter detail"
        ));
        assert_eq!(error.code, "VID3002");
        assert_eq!(error.message, "Video Manager operation failed");
        assert!(!error.message.contains("/srv/rbe"));
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn asset_language_view_hides_local_source_paths() {
        let path = temp_db("asset-source-redaction");
        let manager = Arc::new(VideoManager::open_default(&path, 7200).unwrap());
        let language = VideoLanguage::new(Some(manager));
        let secret_source = "/srv/rbe/private/imports/master.mov";
        let created = language
            .call(
                "learning.catalog",
                "create",
                &[
                    Value::String("private".into()),
                    Value::String("Local import".into()),
                    Value::String("local".into()),
                    Value::String(secret_source.into()),
                ],
            )
            .unwrap();
        assert!(created.get("sourceUri").is_none());
        assert!(!created.to_string().contains(secret_source));
        let id = created["id"].as_str().unwrap().to_string();
        let loaded = language
            .call("learning.catalog", "get", &[Value::String(id)])
            .unwrap();
        assert!(loaded.get("sourceUri").is_none());
        assert!(!loaded.to_string().contains(secret_source));
        let _ = std::fs::remove_file(path);
    }
}
