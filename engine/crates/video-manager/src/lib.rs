//! Global RBE Video Manager control/data plane.
//!
//! This crate intentionally does **not** run FFmpeg, RTMP, HLS, or arbitrary
//! downloads yet. It owns stable video identity, the default database, custom
//! database adapter registration, and job records so media workers can remain
//! isolated and lazy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_DATABASE_NAME: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSourceType {
    Upload,
    Download,
    Local,
    Generated,
    Live,
    RecordedLive,
}

impl VideoSourceType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
            Self::Local => "local",
            Self::Generated => "generated",
            Self::Live => "live",
            Self::RecordedLive => "recorded_live",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoAssetState {
    Reserved,
    Quarantined,
    Processing,
    Ready,
    Failed,
    Deleted,
}

impl VideoAssetState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Quarantined => "quarantined",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "quarantined" => Self::Quarantined,
            "processing" => Self::Processing,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "deleted" => Self::Deleted,
            _ => Self::Reserved,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoAsset {
    pub id: String,
    pub uri: String,
    pub database: String,
    pub namespace: String,
    pub group: String,
    pub title: String,
    pub state: VideoAssetState,
    pub source_type: VideoSourceType,
    pub source_uri: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoJob {
    pub id: String,
    pub asset_id: String,
    pub job_type: String,
    pub state: String,
    pub progress: f64,
    pub attempts: u32,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct CreateAssetRequest {
    pub database: Option<String>,
    pub namespace_kind: String,
    pub namespace_owner: String,
    pub group: String,
    pub title: String,
    pub source_type: VideoSourceType,
    pub source_uri: Option<String>,
    pub metadata: serde_json::Value,
    pub initial_state: VideoAssetState,
}

#[derive(Debug, Clone)]
pub struct QueueDownloadRequest {
    pub database: Option<String>,
    pub namespace_kind: String,
    pub namespace_owner: String,
    pub group: String,
    pub title: String,
    pub url: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueuedDownload {
    pub asset: VideoAsset,
    pub job: VideoJob,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseHealth {
    pub ok: bool,
    pub kind: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub live_runtime: &'static str,
    pub live_idle_secs: u64,
}

/// Storage contract used by Video Manager. The built-in SQLite implementation
/// is registered as `default`; applications may register their own adapter
/// under another name. Selecting an unknown name is always an error.
pub trait VideoDatabase: Send + Sync {
    fn kind(&self) -> &'static str;
    fn health(&self) -> DatabaseHealth;
    fn create_asset(
        &self,
        database: &str,
        request: &CreateAssetRequest,
    ) -> anyhow::Result<VideoAsset>;
    fn insert_job(&self, job: &VideoJob) -> anyhow::Result<()>;
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
}

pub struct SqliteVideoDatabase {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl SqliteVideoDatabase {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create Video Manager database directory {}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("open Video Manager database {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl VideoDatabase for SqliteVideoDatabase {
    fn kind(&self) -> &'static str {
        "sqlite"
    }

    fn health(&self) -> DatabaseHealth {
        match self.connection.lock() {
            Ok(connection) => {
                match connection.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)) {
                    Ok(1) => DatabaseHealth {
                        ok: true,
                        kind: self.kind().into(),
                        detail: Some(self.path.display().to_string()),
                    },
                    Ok(_) => DatabaseHealth {
                        ok: false,
                        kind: self.kind().into(),
                        detail: Some("SQLite health query returned unexpected value".into()),
                    },
                    Err(error) => DatabaseHealth {
                        ok: false,
                        kind: self.kind().into(),
                        detail: Some(error.to_string()),
                    },
                }
            }
            Err(_) => DatabaseHealth {
                ok: false,
                kind: self.kind().into(),
                detail: Some("Video Manager database mutex is poisoned".into()),
            },
        }
    }

    fn create_asset(
        &self,
        database: &str,
        request: &CreateAssetRequest,
    ) -> anyhow::Result<VideoAsset> {
        validate_segment("namespace kind", &request.namespace_kind)?;
        validate_segment("namespace owner", &request.namespace_owner)?;
        validate_segment("group", &request.group)?;

        let namespace = format!("{}:{}", request.namespace_kind, request.namespace_owner);
        let asset_id = Uuid::new_v4().to_string();
        let now = now_ms();
        let uri = format!("vm://{namespace}/{}/{}", request.group, asset_id);
        let metadata_json = serde_json::to_string(&request.metadata)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;

        transaction.execute(
            "INSERT OR IGNORE INTO video_namespaces (id, kind, owner, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![namespace, request.namespace_kind, request.namespace_owner, now],
        )?;

        let group_id: String = transaction
            .query_row(
                "SELECT id FROM video_groups WHERE namespace_id = ?1 AND name = ?2",
                params![namespace, request.group],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        transaction.execute(
            "INSERT OR IGNORE INTO video_groups (id, namespace_id, name, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![group_id, namespace, request.group, now],
        )?;

        transaction.execute(
            "INSERT INTO video_assets (id, group_id, title, state, source_type, source_uri, metadata_json, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                asset_id,
                group_id,
                request.title,
                request.initial_state.as_str(),
                request.source_type.as_str(),
                request.source_uri,
                metadata_json,
                now,
                now,
            ],
        )?;
        transaction.commit()?;

        Ok(VideoAsset {
            id: asset_id,
            uri,
            database: database.to_string(),
            namespace,
            group: request.group.clone(),
            title: request.title.clone(),
            state: request.initial_state,
            source_type: request.source_type,
            source_uri: request.source_uri.clone(),
            metadata: request.metadata.clone(),
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    fn insert_job(&self, job: &VideoJob) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        connection.execute(
            "INSERT INTO video_jobs (id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                job.id,
                job.asset_id,
                job.job_type,
                job.state,
                job.progress,
                job.attempts,
                job.error,
                job.created_at_ms,
                job.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        connection
            .query_row(
                "SELECT a.id, a.title, a.state, a.source_type, a.source_uri, a.metadata_json, a.created_at_ms, a.updated_at_ms, n.id, g.name
                 FROM video_assets a
                 JOIN video_groups g ON g.id = a.group_id
                 JOIN video_namespaces n ON n.id = g.namespace_id
                 WHERE a.id = ?1",
                params![asset_id],
                |row| {
                    let id: String = row.get(0)?;
                    let title: String = row.get(1)?;
                    let state: String = row.get(2)?;
                    let source_type: String = row.get(3)?;
                    let source_uri: Option<String> = row.get(4)?;
                    let metadata_json: String = row.get(5)?;
                    let created_at_ms: i64 = row.get(6)?;
                    let updated_at_ms: i64 = row.get(7)?;
                    let namespace: String = row.get(8)?;
                    let group: String = row.get(9)?;
                    Ok((
                        id,
                        title,
                        state,
                        source_type,
                        source_uri,
                        metadata_json,
                        created_at_ms,
                        updated_at_ms,
                        namespace,
                        group,
                    ))
                },
            )
            .optional()?
            .map(|row| {
                let source_type = parse_source_type(&row.3);
                let metadata = serde_json::from_str(&row.5).unwrap_or(serde_json::Value::Null);
                VideoAsset {
                    uri: format!("vm://{}/{}/{}", row.8, row.9, row.0),
                    id: row.0,
                    database: database.to_string(),
                    namespace: row.8,
                    group: row.9,
                    title: row.1,
                    state: VideoAssetState::parse(&row.2),
                    source_type,
                    source_uri: row.4,
                    metadata,
                    created_at_ms: row.6,
                    updated_at_ms: row.7,
                }
            })
            .pipe(Ok)
    }
}

pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    live_idle_secs: u64,
}

impl VideoManager {
    pub fn open_default(
        database_path: impl AsRef<Path>,
        live_idle_secs: u64,
    ) -> anyhow::Result<Self> {
        let default: Arc<dyn VideoDatabase> =
            Arc::new(SqliteVideoDatabase::open(database_path.as_ref())?);
        let mut databases = HashMap::new();
        databases.insert(DEFAULT_DATABASE_NAME.to_string(), default);
        Ok(Self {
            databases: RwLock::new(databases),
            default_database: DEFAULT_DATABASE_NAME.into(),
            live_idle_secs,
        })
    }

    pub fn register_database(
        &self,
        name: impl Into<String>,
        database: Arc<dyn VideoDatabase>,
    ) -> anyhow::Result<()> {
        let name = name.into();
        validate_segment("database name", &name)?;
        let mut databases = self
            .databases
            .write()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        if databases.contains_key(&name) {
            anyhow::bail!("Video Manager database {name:?} is already registered");
        }
        databases.insert(name, database);
        Ok(())
    }

    pub fn create_asset(&self, request: CreateAssetRequest) -> anyhow::Result<VideoAsset> {
        let (database_name, database) = self.resolve_database(request.database.as_deref())?;
        database.create_asset(&database_name, &request)
    }

    /// Creates a quarantined asset and an explicit download job. The worker
    /// that performs DNS/IP policy, byte limits, magic-byte inspection,
    /// FFprobe, and sandboxed FFmpeg normalization is intentionally separate.
    pub fn queue_download(&self, request: QueueDownloadRequest) -> anyhow::Result<QueuedDownload> {
        if !request.url.starts_with("https://") {
            anyhow::bail!("Video Manager downloads currently require an https:// URL");
        }
        let database_selection = request.database.clone();
        let asset = self.create_asset(CreateAssetRequest {
            database: database_selection.clone(),
            namespace_kind: request.namespace_kind,
            namespace_owner: request.namespace_owner,
            group: request.group,
            title: request.title,
            source_type: VideoSourceType::Download,
            source_uri: Some(request.url),
            metadata: request.metadata,
            initial_state: VideoAssetState::Quarantined,
        })?;
        let now = now_ms();
        let job = VideoJob {
            id: Uuid::new_v4().to_string(),
            asset_id: asset.id.clone(),
            job_type: "download".into(),
            state: "queued".into(),
            progress: 0.0,
            attempts: 0,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let (_, database) = self.resolve_database(database_selection.as_deref())?;
        database.insert_job(&job)?;
        Ok(QueuedDownload { asset, job })
    }

    pub fn get_asset(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Option<VideoAsset>> {
        let (database_name, database) = self.resolve_database(database)?;
        database.get_asset(&database_name, asset_id)
    }

    pub fn database_health(&self, database: Option<&str>) -> anyhow::Result<DatabaseHealth> {
        let (_, database) = self.resolve_database(database)?;
        Ok(database.health())
    }

    pub fn status(&self) -> anyhow::Result<VideoManagerStatus> {
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let mut names = databases.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let default_ok = databases
            .get(&self.default_database)
            .map(|database| database.health().ok)
            .unwrap_or(false);
        Ok(VideoManagerStatus {
            ok: default_ok,
            databases: names,
            default_database: self.default_database.clone(),
            live_runtime: "sleeping",
            live_idle_secs: self.live_idle_secs,
        })
    }

    fn resolve_database(
        &self,
        requested: Option<&str>,
    ) -> anyhow::Result<(String, Arc<dyn VideoDatabase>)> {
        let name = requested.unwrap_or(&self.default_database);
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let database = databases.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("Video Manager database override {name:?} is not registered")
        })?;
        Ok((name.to_string(), database))
    }
}

fn parse_source_type(value: &str) -> VideoSourceType {
    match value {
        "upload" => VideoSourceType::Upload,
        "download" => VideoSourceType::Download,
        "local" => VideoSourceType::Local,
        "generated" => VideoSourceType::Generated,
        "live" => VideoSourceType::Live,
        "recorded_live" => VideoSourceType::RecordedLive,
        _ => VideoSourceType::Local,
    }
}

fn validate_segment(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        anyhow::bail!(
            "{label} must contain only ASCII letters, digits, '-', '_' or '.', got {value:?}"
        );
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS video_namespaces (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    owner TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS video_groups (
    id TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL REFERENCES video_namespaces(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(namespace_id, name)
);
CREATE TABLE IF NOT EXISTS video_assets (
    id TEXT PRIMARY KEY,
    group_id TEXT NOT NULL REFERENCES video_groups(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    state TEXT NOT NULL,
    source_type TEXT NOT NULL,
    source_uri TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS video_variants (
    id TEXT PRIMARY KEY,
    asset_id TEXT NOT NULL REFERENCES video_assets(id) ON DELETE CASCADE,
    profile TEXT NOT NULL,
    codec TEXT,
    width INTEGER,
    height INTEGER,
    fps REAL,
    bitrate INTEGER,
    size_bytes INTEGER,
    path TEXT,
    state TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS video_jobs (
    id TEXT PRIMARY KEY,
    asset_id TEXT NOT NULL REFERENCES video_assets(id) ON DELETE CASCADE,
    job_type TEXT NOT NULL,
    state TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS video_live_sessions (
    id TEXT PRIMARY KEY,
    asset_id TEXT NOT NULL REFERENCES video_assets(id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    ingest_protocol TEXT,
    ingest_endpoint TEXT,
    playback_endpoint TEXT,
    started_at_ms INTEGER,
    ended_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_video_assets_group ON video_assets(group_id);
CREATE INDEX IF NOT EXISTS idx_video_jobs_asset ON video_jobs(asset_id);
CREATE INDEX IF NOT EXISTS idx_video_jobs_state ON video_jobs(state);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rbe-video-manager-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("video.db")
    }

    #[test]
    fn creates_stable_vm_identity_and_reads_it_back() {
        let path = temp_db("identity");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "service".into(),
                namespace_owner: "kastrick-learning".into(),
                group: "tutorials".into(),
                title: "Intro".into(),
                source_type: VideoSourceType::Generated,
                source_uri: None,
                metadata: serde_json::json!({"lesson": 1}),
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        assert!(asset
            .uri
            .starts_with("vm://service:kastrick-learning/tutorials/"));
        let loaded = manager.get_asset(None, &asset.id).unwrap().unwrap();
        assert_eq!(loaded.uri, asset.uri);
        assert_eq!(loaded.metadata, serde_json::json!({"lesson": 1}));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn queues_download_as_quarantined_job_without_fetching_network() {
        let path = temp_db("download");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "uac".into(),
                group: "avatars".into(),
                title: "Avatar".into(),
                url: "https://example.invalid/avatar.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(queued.asset.state, VideoAssetState::Quarantined);
        assert_eq!(queued.job.job_type, "download");
        assert_eq!(queued.job.state, "queued");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_unknown_database_does_not_fall_back() {
        let path = temp_db("override");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let error = manager.database_health(Some("custom")).unwrap_err();
        assert!(error.to_string().contains("not registered"));
        let _ = std::fs::remove_file(path);
    }
}
