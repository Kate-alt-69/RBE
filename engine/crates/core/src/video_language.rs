//! Capability-scoped language facade for the global Video Manager.
//!
//! The language never receives a raw database handle, filesystem path, or
//! process-spawn primitive. Module ownership is supplied by the evaluator and
//! is used to pin mutating operations to `module:<owner>` namespaces.

use std::sync::Arc;

use serde_json::Value;
use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, VideoAssetState, VideoManager, VideoSourceType,
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
                json_result(serde_json::to_value(
                    manager
                        .database_health(database.as_deref())
                        .map_err(operation_error)?,
                ))
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
                        json_result(serde_json::to_value(asset))
                    }
                }
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
                json_result(serde_json::to_value(asset))
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
                json_result(serde_json::to_value(queued))
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

fn operation_error(error: anyhow::Error) -> VideoLanguageError {
    VideoLanguageError::new("VID3002", error.to_string())
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
}
