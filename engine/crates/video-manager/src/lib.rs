//! Global RBE Video Manager control/data plane.
//!
//! Trusted Rust owns stable video identity, quarantine-safe HTTP download,
//! container inspection, FFprobe validation, fixed-profile FFmpeg normalization,
//! database promotion, and job state. Arbitrary process execution is never
//! exposed to route/module/service language code; live RTMP/HLS workers remain
//! separate and lazy.

mod container_probe;
mod download;
mod download_worker;
mod ffmpeg;
mod ffmpeg_capabilities;
mod ffprobe;
mod live;
mod normalization;
mod pipeline;
mod worker;

pub use container_probe::{
    probe_quarantine_container, sniff_video_container, ContainerProbe, VideoContainerKind,
};
pub use download::{
    is_public_download_ip, parse_download_target, resolve_download_target, DownloadTarget,
    ResolvedDownloadTarget,
};
pub use download_worker::{DownloadPolicy, DownloadReceipt};
pub use ffmpeg::{FfmpegPolicy, FfmpegVideoEncoder, NormalizedMedia};
pub use ffmpeg_capabilities::{probe_ffmpeg_capabilities, FfmpegCapabilities};
pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};
pub use live::{
    ReserveLiveSessionRequest, VideoLiveRuntimeState, VideoLiveSession, VideoLiveSessionCounts,
    VideoLiveSessionState,
};
pub use worker::{VideoWorkerHandle, VideoWorkerPolicy};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_DATABASE_NAME: &str = "default";

pub(crate) const PROGRESS_QUEUED: f64 = 0.0;
pub(crate) const PROGRESS_DOWNLOADING: f64 = 0.05;
pub(crate) const PROGRESS_DOWNLOADED: f64 = 0.45;
pub(crate) const PROGRESS_CONTAINER_CHECKED: f64 = 0.55;
pub(crate) const PROGRESS_PROBED: f64 = 0.70;
pub(crate) const PROGRESS_NORMALIZING: f64 = 0.75;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoVariant {
    pub id: String,
    pub asset_id: String,
    pub profile: String,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bitrate: Option<u64>,
    pub size_bytes: u64,
    pub path: String,
    pub state: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoWorkerState {
    Disabled,
    Sleeping,
    Processing,
    Degraded,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoDownloadWorkerStatus {
    pub state: VideoWorkerState,
    pub queued_downloads: u64,
    pub video_encoder: Option<FfmpegVideoEncoder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoManagerStatus {
    pub ok: bool,
    pub databases: Vec<String>,
    pub default_database: String,
    pub download_worker: VideoDownloadWorkerStatus,
    pub live_runtime: VideoLiveRuntimeState,
    pub live_sessions: VideoLiveSessionCounts,
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
    fn claim_job(
        &self,
        job_id: &str,
        expected_state: &str,
        claimed_state: &str,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn update_job(
        &self,
        job_id: &str,
        state: &str,
        progress: f64,
        error: Option<&str>,
    ) -> anyhow::Result<()>;
    fn transition_job(
        &self,
        job_id: &str,
        expected_state: &str,
        next_state: &str,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>>;
    fn queued_download_count(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
    fn next_queued_download(&self, _database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        Ok(None)
    }
    fn recover_incomplete_downloads(&self, _database: &str) -> anyhow::Result<Vec<QueuedDownload>> {
        Ok(Vec::new())
    }
    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn list_variants(&self, _asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
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
        validate_job_state(&job.state)?;
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

    fn claim_job(
        &self,
        job_id: &str,
        expected_state: &str,
        claimed_state: &str,
    ) -> anyhow::Result<Option<VideoJob>> {
        validate_job_state(expected_state)?;
        validate_job_state(claimed_state)?;
        let now = now_ms();
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE video_jobs SET state = ?1, attempts = attempts + 1, error = NULL, updated_at_ms = ?2 WHERE id = ?3 AND state = ?4",
            params![claimed_state, now, job_id, expected_state],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        let job = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        transaction.commit()?;
        Ok(job)
    }

    fn update_job(
        &self,
        job_id: &str,
        state: &str,
        progress: f64,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_job_state(state)?;
        if !progress.is_finite() || !(0.0..=1.0).contains(&progress) {
            anyhow::bail!("Video Manager job progress must be finite and between 0 and 1");
        }
        let now = now_ms();
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let changed = connection.execute(
            "UPDATE video_jobs SET state = ?1, progress = ?2, error = ?3, updated_at_ms = ?4 WHERE id = ?5",
            params![state, progress, error, now, job_id],
        )?;
        if changed != 1 {
            anyhow::bail!("Video Manager job {job_id:?} does not exist");
        }
        Ok(())
    }

    fn transition_job(
        &self,
        job_id: &str,
        expected_state: &str,
        next_state: &str,
    ) -> anyhow::Result<Option<VideoJob>> {
        validate_job_state(expected_state)?;
        validate_job_state(next_state)?;
        let now = now_ms();
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE video_jobs SET state = ?1, error = NULL, updated_at_ms = ?2 WHERE id = ?3 AND state = ?4",
            params![next_state, now, job_id, expected_state],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        let job = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        transaction.commit()?;
        Ok(job)
    }

    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        Ok(connection
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?)
    }

    fn queued_download_count(&self) -> anyhow::Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let count = connection.query_row(
            "SELECT COUNT(*) FROM video_jobs WHERE job_type = 'download' AND state = 'queued'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        u64::try_from(count).context("Video Manager queued download count is negative")
    }

    fn next_queued_download(&self, database: &str) -> anyhow::Result<Option<QueuedDownload>> {
        let pair = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
            connection
                .query_row(
                    "SELECT id, asset_id FROM video_jobs WHERE job_type = 'download' AND state = 'queued' ORDER BY created_at_ms ASC, id ASC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
        };
        let Some((job_id, asset_id)) = pair else {
            return Ok(None);
        };
        let job = self
            .get_job(&job_id)?
            .ok_or_else(|| anyhow::anyhow!("Video Manager queued job {job_id:?} disappeared"))?;
        let asset = self.get_asset(database, &asset_id)?.ok_or_else(|| {
            anyhow::anyhow!("Video Manager queued asset {asset_id:?} disappeared")
        })?;
        Ok(Some(QueuedDownload { asset, job }))
    }

    fn recover_incomplete_downloads(&self, database: &str) -> anyhow::Result<Vec<QueuedDownload>> {
        let pairs = {
            let now = now_ms();
            let connection = self
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
            let transaction = connection.unchecked_transaction()?;
            let mut statement = transaction.prepare(
                "SELECT id, asset_id FROM video_jobs WHERE job_type = 'download' AND state IN ('downloading', 'downloaded', 'inspecting', 'container_checked', 'probing', 'probed', 'normalizing') ORDER BY created_at_ms ASC, id ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.execute(
                "UPDATE video_jobs SET state = 'queued', progress = 0.0, error = NULL, updated_at_ms = ?1 WHERE job_type = 'download' AND state IN ('downloading', 'downloaded', 'inspecting', 'container_checked', 'probing', 'probed', 'normalizing')",
                params![now],
            )?;
            transaction.commit()?;
            rows
        };

        let mut recovered = Vec::with_capacity(pairs.len());
        for (job_id, asset_id) in pairs {
            let job = self.get_job(&job_id)?.ok_or_else(|| {
                anyhow::anyhow!("Video Manager recovered job {job_id:?} disappeared")
            })?;
            let asset = self.get_asset(database, &asset_id)?.ok_or_else(|| {
                anyhow::anyhow!("Video Manager recovered asset {asset_id:?} disappeared")
            })?;
            recovered.push(QueuedDownload { asset, job });
        }
        Ok(recovered)
    }

    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>> {
        validate_generated_uuid("job id", job_id)?;
        validate_generated_uuid("variant id", &variant.id)?;
        validate_generated_uuid("variant asset id", &variant.asset_id)?;
        validate_segment("variant profile", &variant.profile)?;
        validate_job_state(&variant.state)?;
        if variant.state != "ready" {
            anyhow::bail!("Video Manager committed variant state must be ready");
        }
        if let Some(codec) = &variant.codec {
            validate_segment("variant codec", codec)?;
        }
        if variant
            .fps
            .is_some_and(|fps| !fps.is_finite() || fps <= 0.0)
        {
            anyhow::bail!("Video Manager variant fps must be finite and positive");
        }
        let bitrate = variant
            .bitrate
            .map(i64::try_from)
            .transpose()
            .context("Video Manager variant bitrate exceeds SQLite integer range")?;
        let size_bytes = i64::try_from(variant.size_bytes)
            .context("Video Manager variant size exceeds SQLite integer range")?;
        validate_relative_media_path(&variant.path)?;

        let now = now_ms();
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let transaction = connection.unchecked_transaction()?;
        let job = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        let Some(job) = job else {
            transaction.commit()?;
            return Ok(None);
        };
        if job.state != "normalizing"
            || job.asset_id != variant.asset_id
            || job.job_type != "download"
        {
            transaction.commit()?;
            return Ok(None);
        }

        transaction.execute(
            "INSERT INTO video_variants (id, asset_id, profile, codec, width, height, fps, bitrate, size_bytes, path, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                variant.id,
                variant.asset_id,
                variant.profile,
                variant.codec,
                variant.width.map(i64::from),
                variant.height.map(i64::from),
                variant.fps,
                bitrate,
                size_bytes,
                variant.path,
                variant.state,
                variant.created_at_ms,
                variant.updated_at_ms,
            ],
        )?;
        let asset_changed = transaction.execute(
            "UPDATE video_assets SET state = 'ready', updated_at_ms = ?1 WHERE id = ?2 AND state = 'quarantined'",
            params![now, variant.asset_id],
        )?;
        if asset_changed != 1 {
            anyhow::bail!(
                "Video Manager asset {:?} is not quarantined and cannot be promoted",
                variant.asset_id
            );
        }
        let job_changed = transaction.execute(
            "UPDATE video_jobs SET state = 'ready', progress = 1.0, error = NULL, updated_at_ms = ?1 WHERE id = ?2 AND state = 'normalizing'",
            params![now, job_id],
        )?;
        if job_changed != 1 {
            anyhow::bail!(
                "Video Manager normalization job {job_id:?} lost its state during commit"
            );
        }
        let committed = transaction
            .query_row(
                "SELECT id, asset_id, job_type, state, progress, attempts, error, created_at_ms, updated_at_ms FROM video_jobs WHERE id = ?1",
                params![job_id],
                read_video_job,
            )
            .optional()?;
        transaction.commit()?;
        Ok(committed)
    }

    fn list_variants(&self, asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
        validate_generated_uuid("asset id", asset_id)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager database mutex is poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT id, asset_id, profile, codec, width, height, fps, bitrate, size_bytes, path, state, created_at_ms, updated_at_ms FROM video_variants WHERE asset_id = ?1 ORDER BY created_at_ms ASC, id ASC",
        )?;
        let rows = statement
            .query_map(params![asset_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<f64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        rows.into_iter()
            .map(
                |(
                    id,
                    asset_id,
                    profile,
                    codec,
                    width,
                    height,
                    fps,
                    bitrate,
                    size_bytes,
                    path,
                    state,
                    created_at_ms,
                    updated_at_ms,
                )| {
                    Ok(VideoVariant {
                        id,
                        asset_id,
                        profile,
                        codec,
                        width: width
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant width is outside u32 range")?,
                        height: height
                            .map(u32::try_from)
                            .transpose()
                            .context("Video Manager variant height is outside u32 range")?,
                        fps,
                        bitrate: bitrate
                            .map(u64::try_from)
                            .transpose()
                            .context("Video Manager variant bitrate is negative")?,
                        size_bytes: u64::try_from(size_bytes)
                            .context("Video Manager variant size is negative")?,
                        path,
                        state,
                        created_at_ms,
                        updated_at_ms,
                    })
                },
            )
            .collect()
    }

    fn insert_live_session(
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
            |(
                id,
                asset_id,
                state,
                ingest_protocol,
                ingest_endpoint,
                playback_endpoint,
                started_at_ms,
                ended_at_ms,
            )| {
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
        let ended = matches!(
            next,
            VideoLiveSessionState::Ended | VideoLiveSessionState::Failed
        )
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
            |(
                id,
                asset_id,
                state,
                ingest_protocol,
                ingest_endpoint,
                playback_endpoint,
                started_at_ms,
                ended_at_ms,
            )| {
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
            reserved: u64::try_from(reserved)
                .context("Video Manager reserved live count is negative")?,
            starting: u64::try_from(starting)
                .context("Video Manager starting live count is negative")?,
            live: u64::try_from(live).context("Video Manager active live count is negative")?,
            stopping: u64::try_from(stopping)
                .context("Video Manager stopping live count is negative")?,
        })
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

fn read_video_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<VideoJob> {
    Ok(VideoJob {
        id: row.get(0)?,
        asset_id: row.get(1)?,
        job_type: row.get(2)?,
        state: row.get(3)?,
        progress: row.get(4)?,
        attempts: row.get(5)?,
        error: row.get(6)?,
        created_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

fn validate_job_state(state: &str) -> anyhow::Result<()> {
    if state.is_empty()
        || state.len() > 64
        || !state.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        anyhow::bail!("Video Manager job state is not a valid state token: {state:?}");
    }
    Ok(())
}

pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    work_notify: tokio::sync::Notify,
    worker_state: Mutex<VideoWorkerState>,
    worker_encoder: Mutex<Option<FfmpegVideoEncoder>>,
    live_idle_secs: u64,
}

impl VideoManager {
    pub fn open_default(
        database_path: impl AsRef<Path>,
        live_idle_secs: u64,
    ) -> anyhow::Result<Self> {
        let database_path = database_path.as_ref();
        let default: Arc<dyn VideoDatabase> = Arc::new(SqliteVideoDatabase::open(database_path)?);
        let data_dir = database_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let quarantine_root = data_dir.join("quarantine").join("downloads");
        std::fs::create_dir_all(&quarantine_root).with_context(|| {
            format!(
                "create Video Manager quarantine directory {}",
                quarantine_root.display()
            )
        })?;
        let quarantine_root = std::fs::canonicalize(&quarantine_root).with_context(|| {
            format!(
                "canonicalize Video Manager quarantine directory {}",
                quarantine_root.display()
            )
        })?;
        let media_root = data_dir.join("media").join("assets");
        std::fs::create_dir_all(&media_root).with_context(|| {
            format!(
                "create Video Manager media directory {}",
                media_root.display()
            )
        })?;
        let media_root = std::fs::canonicalize(&media_root).with_context(|| {
            format!(
                "canonicalize Video Manager media directory {}",
                media_root.display()
            )
        })?;
        let mut databases = HashMap::new();
        databases.insert(DEFAULT_DATABASE_NAME.to_string(), default);
        Ok(Self {
            databases: RwLock::new(databases),
            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root,
            media_root,
            work_notify: tokio::sync::Notify::new(),
            worker_state: Mutex::new(VideoWorkerState::Disabled),
            worker_encoder: Mutex::new(None),
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
        let target = parse_download_target(&request.url)?;
        let database_selection = request.database.clone();
        let asset = self.create_asset(CreateAssetRequest {
            database: database_selection.clone(),
            namespace_kind: request.namespace_kind,
            namespace_owner: request.namespace_owner,
            group: request.group,
            title: request.title,
            source_type: VideoSourceType::Download,
            source_uri: Some(target.normalized_url().to_string()),
            metadata: request.metadata,
            initial_state: VideoAssetState::Quarantined,
        })?;
        let now = now_ms();
        let job = VideoJob {
            id: Uuid::new_v4().to_string(),
            asset_id: asset.id.clone(),
            job_type: "download".into(),
            state: "queued".into(),
            progress: PROGRESS_QUEUED,
            attempts: 0,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let (_, database) = self.resolve_database(database_selection.as_deref())?;
        let quarantine_path = self.reserve_download_quarantine(&asset.id, &job.id)?;
        if let Err(error) = database.insert_job(&job) {
            let _ = std::fs::remove_file(&quarantine_path);
            return Err(error);
        }
        self.work_notify.notify_one();
        Ok(QueuedDownload { asset, job })
    }

    /// Atomically claim and execute one queued download job. A successful HTTP
    /// fetch remains quarantined until content validation explicitly accepts it.
    pub async fn run_queued_download(
        &self,
        queued: &QueuedDownload,
        policy: DownloadPolicy,
    ) -> anyhow::Result<DownloadReceipt> {
        if queued.asset.id != queued.job.asset_id || queued.job.job_type != "download" {
            anyhow::bail!("Video Manager queued download identity/type mismatch");
        }
        let (_, database) = self.resolve_database(Some(&queued.asset.database))?;
        let claimed = database
            .claim_job(&queued.job.id, "queued", "downloading")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Video Manager download job {:?} is not queued and cannot be claimed",
                    queued.job.id
                )
            })?;
        if claimed.asset_id != queued.asset.id || claimed.job_type != "download" {
            let detail = "Video Manager claimed job does not match its queued download asset/type";
            let _ = database.update_job(&claimed.id, "failed", PROGRESS_DOWNLOADING, Some(detail));
            if claimed.job_type == "download" {
                if let Ok(path) = self.quarantine_path(&claimed.asset_id, &claimed.id) {
                    let _ = std::fs::remove_file(path);
                }
            }
            anyhow::bail!("{detail}");
        }

        database.update_job(&claimed.id, "downloading", PROGRESS_DOWNLOADING, None)?;
        let mut claimed_download = queued.clone();
        claimed_download.job = claimed;
        match self
            .execute_queued_download(&claimed_download, policy)
            .await
        {
            Ok(receipt) => {
                database.update_job(&queued.job.id, "downloaded", PROGRESS_DOWNLOADED, None)?;
                Ok(receipt)
            }
            Err(error) => {
                let detail = error.to_string();
                if let Err(state_error) = database.update_job(
                    &queued.job.id,
                    "failed",
                    PROGRESS_DOWNLOADING,
                    Some(&detail),
                ) {
                    return Err(anyhow::anyhow!(
                        "Video Manager download failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// Run the cheap fail-closed container signature gate on a completed download.
    /// Passing this stage does not mark the asset ready; FFprobe still has to prove
    /// that the object contains a decodable video stream.
    pub async fn inspect_download_container(
        &self,
        queued: &QueuedDownload,
    ) -> anyhow::Result<ContainerProbe> {
        if queued.asset.id != queued.job.asset_id || queued.job.job_type != "download" {
            anyhow::bail!("Video Manager container inspection identity/type mismatch");
        }
        let (_, database) = self.resolve_database(Some(&queued.asset.database))?;
        let transitioned = database
            .transition_job(&queued.job.id, "downloaded", "inspecting")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Video Manager download job {:?} is not downloaded and cannot be inspected",
                    queued.job.id
                )
            })?;
        if transitioned.asset_id != queued.asset.id || transitioned.job_type != "download" {
            let detail = "Video Manager transitioned job does not match its download asset/type";
            let _ = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_DOWNLOADED,
                Some(detail),
            );
            anyhow::bail!("{detail}");
        }

        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
        match probe_quarantine_container(&quarantine).await {
            Ok(probe) => {
                database.update_job(
                    &transitioned.id,
                    "container_checked",
                    PROGRESS_CONTAINER_CHECKED,
                    None,
                )?;
                Ok(probe)
            }
            Err(error) => {
                let detail = error.to_string();
                let _ = tokio::fs::remove_file(&quarantine).await;
                if let Err(state_error) = database.update_job(
                    &transitioned.id,
                    "failed",
                    PROGRESS_DOWNLOADED,
                    Some(&detail),
                ) {
                    return Err(anyhow::anyhow!(
                        "Video Manager container inspection failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// Run trusted FFprobe only after the cheap container gate accepted the file.
    /// A successful probe still leaves the asset quarantined; normalization and final
    /// promotion are separate stages.
    pub async fn probe_download_media(
        &self,
        queued: &QueuedDownload,
        policy: &FfprobePolicy,
    ) -> anyhow::Result<MediaProbe> {
        if queued.asset.id != queued.job.asset_id || queued.job.job_type != "download" {
            anyhow::bail!("Video Manager FFprobe identity/type mismatch");
        }
        let (_, database) = self.resolve_database(Some(&queued.asset.database))?;
        let transitioned = database
            .transition_job(&queued.job.id, "container_checked", "probing")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Video Manager download job {:?} has not passed container validation",
                    queued.job.id
                )
            })?;
        if transitioned.asset_id != queued.asset.id || transitioned.job_type != "download" {
            let detail = "Video Manager FFprobe job does not match its download asset/type";
            let _ = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_CONTAINER_CHECKED,
                Some(detail),
            );
            anyhow::bail!("{detail}");
        }

        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
        match ffprobe::run_ffprobe(&quarantine, policy).await {
            Ok(probe) => {
                database.update_job(&transitioned.id, "probed", PROGRESS_PROBED, None)?;
                Ok(probe)
            }
            Err(error) => {
                let detail = error.to_string();
                let _ = tokio::fs::remove_file(&quarantine).await;
                if let Err(state_error) = database.update_job(
                    &transitioned.id,
                    "failed",
                    PROGRESS_CONTAINER_CHECKED,
                    Some(&detail),
                ) {
                    return Err(anyhow::anyhow!(
                        "Video Manager FFprobe failed: {detail}; additionally failed to persist job failure: {state_error}"
                    ));
                }
                Err(error)
            }
        }
    }

    fn database_names(&self) -> anyhow::Result<Vec<String>> {
        let databases = self
            .databases
            .read()
            .map_err(|_| anyhow::anyhow!("Video Manager database registry is poisoned"))?;
        let mut names = databases.keys().cloned().collect::<Vec<_>>();
        names.sort_by_key(|name| (name != &self.default_database, name.clone()));
        Ok(names)
    }

    fn queued_download_count(&self) -> anyhow::Result<u64> {
        let mut total = 0u64;
        for name in self.database_names()? {
            let (_, database) = self.resolve_database(Some(&name))?;
            total = total
                .checked_add(database.queued_download_count()?)
                .ok_or_else(|| anyhow::anyhow!("Video Manager queued download count overflowed"))?;
        }
        Ok(total)
    }

    fn set_worker_state(&self, state: VideoWorkerState) -> anyhow::Result<()> {
        let mut current = self
            .worker_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))?;
        *current = state;
        Ok(())
    }

    fn worker_state(&self) -> anyhow::Result<VideoWorkerState> {
        self.worker_state
            .lock()
            .map(|state| *state)
            .map_err(|_| anyhow::anyhow!("Video Manager worker state mutex is poisoned"))
    }

    fn set_worker_encoder(&self, encoder: Option<FfmpegVideoEncoder>) -> anyhow::Result<()> {
        let mut current = self
            .worker_encoder
            .lock()
            .map_err(|_| anyhow::anyhow!("Video Manager worker encoder mutex is poisoned"))?;
        *current = encoder;
        Ok(())
    }

    fn worker_encoder(&self) -> anyhow::Result<Option<FfmpegVideoEncoder>> {
        self.worker_encoder
            .lock()
            .map(|encoder| *encoder)
            .map_err(|_| anyhow::anyhow!("Video Manager worker encoder mutex is poisoned"))
    }

    fn next_queued_download(
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

    fn recover_incomplete_downloads(&self) -> anyhow::Result<usize> {
        let mut count = 0usize;
        for name in self.database_names()? {
            let (_, database) = self.resolve_database(Some(&name))?;
            for queued in database.recover_incomplete_downloads(&name)? {
                match self.cleanup_recovered_download_artifacts(&queued.asset.id, &queued.job.id) {
                    Ok(()) => count += 1,
                    Err(error) => {
                        let detail = format!(
                            "Video Manager could not safely recover interrupted download: {error}"
                        );
                        database.update_job(&queued.job.id, "failed", 0.0, Some(&detail))?;
                        tracing::error!(
                            database = %name,
                            asset_id = %queued.asset.id,
                            job_id = %queued.job.id,
                            error = %error,
                            "Video Manager failed closed while recovering interrupted download"
                        );
                    }
                }
            }
        }
        if count > 0 {
            self.work_notify.notify_one();
        }
        Ok(count)
    }

    fn cleanup_recovered_download_artifacts(
        &self,
        asset_id: &str,
        job_id: &str,
    ) -> anyhow::Result<()> {
        validate_generated_uuid("asset id", asset_id)?;
        validate_generated_uuid("job id", job_id)?;
        let asset_dir = self.media_root.join(asset_id);
        let metadata = match std::fs::symlink_metadata(&asset_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_dir() {
            anyhow::bail!("Video Manager recovery asset path is not a real directory");
        }
        let canonical = std::fs::canonicalize(&asset_dir)?;
        if !canonical.starts_with(&self.media_root) {
            anyhow::bail!("Video Manager recovery asset directory escaped its storage root");
        }
        for path in [
            canonical.join(format!(".{job_id}.normalizing.mp4")),
            canonical.join("primary.mp4"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "remove stale Video Manager recovery artifact {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        let _ = std::fs::remove_dir(&canonical);
        Ok(())
    }

    pub fn get_asset(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Option<VideoAsset>> {
        let (database_name, database) = self.resolve_database(database)?;
        database.get_asset(&database_name, asset_id)
    }

    pub fn get_job(
        &self,
        database: Option<&str>,
        job_id: &str,
    ) -> anyhow::Result<Option<VideoJob>> {
        let (_, database) = self.resolve_database(database)?;
        database.get_job(job_id)
    }

    pub fn list_variants(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Vec<VideoVariant>> {
        let (_, database) = self.resolve_database(database)?;
        database.list_variants(asset_id)
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
        drop(databases);
        let worker_state = self.worker_state()?;
        let queued_downloads = self.queued_download_count()?;
        let live_sessions = self.live_session_counts()?;
        let live_runtime = VideoLiveRuntimeState::from_counts(live_sessions);
        Ok(VideoManagerStatus {
            ok: default_ok && worker_state != VideoWorkerState::Degraded,
            databases: names,
            default_database: self.default_database.clone(),
            download_worker: VideoDownloadWorkerStatus {
                state: worker_state,
                queued_downloads,
                video_encoder: self.worker_encoder()?,
            },
            live_runtime,
            live_sessions,
            live_idle_secs: self.live_idle_secs,
        })
    }

    fn quarantine_path(&self, asset_id: &str, job_id: &str) -> anyhow::Result<PathBuf> {
        validate_generated_uuid("asset id", asset_id)?;
        validate_generated_uuid("job id", job_id)?;
        Ok(self
            .quarantine_root
            .join(asset_id)
            .join(format!("{job_id}.part")))
    }

    fn reserve_download_quarantine(&self, asset_id: &str, job_id: &str) -> anyhow::Result<PathBuf> {
        let path = self.quarantine_path(asset_id, job_id)?;
        let asset_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Video Manager quarantine path has no parent"))?;
        std::fs::create_dir_all(asset_dir).with_context(|| {
            format!(
                "create Video Manager asset quarantine directory {}",
                asset_dir.display()
            )
        })?;
        let canonical_asset_dir = std::fs::canonicalize(asset_dir).with_context(|| {
            format!(
                "canonicalize Video Manager asset quarantine directory {}",
                asset_dir.display()
            )
        })?;
        if !canonical_asset_dir.starts_with(&self.quarantine_root) {
            anyhow::bail!("Video Manager quarantine directory escaped its storage root");
        }
        let canonical_path = canonical_asset_dir.join(
            path.file_name()
                .ok_or_else(|| anyhow::anyhow!("Video Manager quarantine path has no filename"))?,
        );
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&canonical_path)
            .with_context(|| {
                format!(
                    "reserve Video Manager quarantine file {}",
                    canonical_path.display()
                )
            })?;
        Ok(canonical_path)
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

fn validate_generated_uuid(label: &str, value: &str) -> anyhow::Result<()> {
    let parsed = Uuid::parse_str(value)
        .with_context(|| format!("Video Manager {label} is not a UUID: {value:?}"))?;
    if parsed.hyphenated().to_string() != value {
        anyhow::bail!("Video Manager {label} must use canonical lowercase UUID form");
    }
    Ok(())
}

fn validate_relative_media_path(value: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("Video Manager media path must be a non-empty relative normal path");
    }
    Ok(())
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
CREATE INDEX IF NOT EXISTS idx_video_live_sessions_asset ON video_live_sessions(asset_id);
CREATE INDEX IF NOT EXISTS idx_video_live_sessions_state ON video_live_sessions(state);
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
                url: "HTTPS://Example.INVALID:443/avatar.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(queued.asset.state, VideoAssetState::Quarantined);
        assert_eq!(
            queued.asset.source_uri.as_deref(),
            Some("https://example.invalid/avatar.mp4")
        );
        assert_eq!(queued.job.job_type, "download");
        assert_eq!(queued.job.state, "queued");
        let quarantine_path = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        assert!(quarantine_path.is_file());
        assert_eq!(quarantine_path.metadata().unwrap().len(), 0);
        let expected_root =
            std::fs::canonicalize(path.parent().unwrap().join("quarantine").join("downloads"))
                .unwrap();
        assert!(quarantine_path.starts_with(expected_root));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quarantine_paths_reject_noncanonical_generated_ids() {
        let path = temp_db("quarantine-path");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let valid = Uuid::new_v4().to_string();
        assert!(manager.quarantine_path("../escape", &valid).is_err());
        assert!(manager
            .quarantine_path(&valid.to_uppercase(), &valid)
            .is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_job_claim_is_atomic() {
        let path = temp_db("claim");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "claims".into(),
                title: "Claim".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let (_, database) = manager.resolve_database(None).unwrap();
        let claimed = database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        assert_eq!(claimed.state, "downloading");
        assert_eq!(claimed.attempts, 1);
        assert!(database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .is_none());
        database
            .update_job(&queued.job.id, "downloaded", 1.0, None)
            .unwrap();
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "downloaded");
        assert_eq!(stored.progress, 1.0);
        assert_eq!(stored.attempts, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn download_runner_records_preflight_failure_and_cleans_quarantine() {
        let path = temp_db("runner-failure");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "failures".into(),
                title: "Failure".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        let policy = DownloadPolicy {
            max_bytes: 0,
            ..DownloadPolicy::default()
        };
        assert!(manager.run_queued_download(&queued, policy).await.is_err());
        let (_, database) = manager.resolve_database(None).unwrap();
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "failed");
        assert_eq!(stored.attempts, 1);
        assert!(stored
            .error
            .as_deref()
            .is_some_and(|error| error.contains("byte limit")));
        assert!(!quarantine.exists());
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Quarantined);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn losing_download_claim_does_not_delete_quarantine() {
        let path = temp_db("runner-claim");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "claims".into(),
                title: "Claim".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        let (_, database) = manager.resolve_database(None).unwrap();
        database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        assert!(manager
            .run_queued_download(&queued, DownloadPolicy::default())
            .await
            .is_err());
        assert!(quarantine.exists());
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "downloading");
        assert_eq!(stored.attempts, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn mark_downloaded(manager: &VideoManager, queued: &QueuedDownload) -> Arc<dyn VideoDatabase> {
        let (_, database) = manager.resolve_database(None).unwrap();
        database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        database
            .update_job(&queued.job.id, "downloaded", 1.0, None)
            .unwrap();
        database
    }

    #[tokio::test]
    async fn container_stage_accepts_mp4_without_incrementing_attempts() {
        let path = temp_db("container-pass");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "validation".into(),
                title: "MP4".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let database = mark_downloaded(&manager, &queued);
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        std::fs::write(
            &quarantine,
            b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
        )
        .unwrap();
        let probe = manager.inspect_download_container(&queued).await.unwrap();
        assert_eq!(probe.kind, VideoContainerKind::IsoBmff);
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "container_checked");
        assert_eq!(stored.attempts, 1);
        assert!(quarantine.exists());
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Quarantined);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn container_stage_rejects_unknown_bytes_and_deletes_quarantine() {
        let path = temp_db("container-reject");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "validation".into(),
                title: "Bad".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let database = mark_downloaded(&manager, &queued);
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        std::fs::write(&quarantine, b"definitely not a video container").unwrap();
        assert!(manager.inspect_download_container(&queued).await.is_err());
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "failed");
        assert_eq!(stored.attempts, 1);
        assert!(stored
            .error
            .as_deref()
            .is_some_and(|error| error.contains("container signature")));
        assert!(!quarantine.exists());
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Quarantined);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn ffprobe_stage_records_worker_failure_and_deletes_quarantine() {
        let path = temp_db("ffprobe-failure");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "validation".into(),
                title: "Probe".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let database = mark_downloaded(&manager, &queued);
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        std::fs::write(
            &quarantine,
            b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
        )
        .unwrap();
        manager.inspect_download_container(&queued).await.unwrap();
        let missing = std::env::temp_dir().join(format!("rbe-missing-ffprobe-{}", Uuid::new_v4()));
        let policy = FfprobePolicy::new(missing);
        assert!(manager
            .probe_download_media(&queued, &policy)
            .await
            .is_err());
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "failed");
        assert_eq!(stored.attempts, 1);
        assert!(stored
            .error
            .as_deref()
            .is_some_and(|error| error.contains("FFprobe executable")));
        assert!(!quarantine.exists());
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Quarantined);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn losing_ffprobe_transition_preserves_quarantine() {
        let path = temp_db("ffprobe-claim");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "validation".into(),
                title: "Probe Claim".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let database = mark_downloaded(&manager, &queued);
        let quarantine = manager
            .quarantine_path(&queued.asset.id, &queued.job.id)
            .unwrap();
        std::fs::write(
            &quarantine,
            b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
        )
        .unwrap();
        manager.inspect_download_container(&queued).await.unwrap();
        database
            .transition_job(&queued.job.id, "container_checked", "probing")
            .unwrap()
            .unwrap();
        let missing = std::env::temp_dir().join(format!("rbe-missing-ffprobe-{}", Uuid::new_v4()));
        let policy = FfprobePolicy::new(missing);
        assert!(manager
            .probe_download_media(&queued, &policy)
            .await
            .is_err());
        assert!(quarantine.exists());
        let stored = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(stored.state, "probing");
        assert_eq!(stored.attempts, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn explicit_unknown_database_does_not_fall_back() {
        let path = temp_db("override");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let error = manager.database_health(Some("custom")).unwrap_err();
        assert!(error.to_string().contains("not registered"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ready_variant_commit_atomically_promotes_asset_and_job() {
        let path = temp_db("ready-variant");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "normalization".into(),
                title: "Ready".into(),
                url: "https://example.invalid/video.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let (_, database) = manager.resolve_database(None).unwrap();
        database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        database
            .update_job(&queued.job.id, "downloaded", 1.0, None)
            .unwrap();
        for (from, to) in [
            ("downloaded", "inspecting"),
            ("inspecting", "container_checked"),
            ("container_checked", "probing"),
            ("probing", "probed"),
            ("probed", "normalizing"),
        ] {
            database
                .transition_job(&queued.job.id, from, to)
                .unwrap()
                .unwrap();
        }
        let now = now_ms();
        let variant = VideoVariant {
            id: Uuid::new_v4().to_string(),
            asset_id: queued.asset.id.clone(),
            profile: "standard".into(),
            codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            fps: Some(30.0),
            bitrate: Some(4_000_000),
            size_bytes: 1_000_000,
            path: format!("{}/primary.mp4", queued.asset.id),
            state: "ready".into(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        let committed = database
            .commit_ready_variant(&queued.job.id, &variant)
            .unwrap()
            .unwrap();
        assert_eq!(committed.state, "ready");
        assert_eq!(committed.progress, 1.0);
        let variants = manager.list_variants(None, &queued.asset.id).unwrap();
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].id, variant.id);
        assert_eq!(variants[0].path, variant.path);
        assert_eq!(variants[0].size_bytes, variant.size_bytes);
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Ready);
        assert!(database
            .commit_ready_variant(&queued.job.id, &variant)
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn queued_download_discovery_returns_oldest_waiting_job() {
        let path = temp_db("worker-discovery");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let first = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "queue".into(),
                title: "First".into(),
                url: "https://example.invalid/first.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let _second = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "queue".into(),
                title: "Second".into(),
                url: "https://example.invalid/second.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let discovered = manager.next_queued_download(None).unwrap().unwrap();
        assert_eq!(discovered.job.id, first.job.id);
        assert_eq!(discovered.asset.id, first.asset.id);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn interrupted_downloads_requeue_and_remove_stale_normalization_output() {
        let path = temp_db("worker-recovery");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "worker".into(),
                group: "recovery".into(),
                title: "Interrupted".into(),
                url: "https://example.invalid/interrupted.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let (_, database) = manager.resolve_database(None).unwrap();
        database
            .claim_job(&queued.job.id, "queued", "downloading")
            .unwrap()
            .unwrap();
        for (from, to) in [
            ("downloading", "downloaded"),
            ("downloaded", "inspecting"),
            ("inspecting", "container_checked"),
            ("container_checked", "probing"),
            ("probing", "probed"),
            ("probed", "normalizing"),
        ] {
            database
                .transition_job(&queued.job.id, from, to)
                .unwrap()
                .unwrap();
        }
        let asset_dir = manager.media_root.join(&queued.asset.id);
        std::fs::create_dir_all(&asset_dir).unwrap();
        let staging = asset_dir.join(format!(".{}.normalizing.mp4", queued.job.id));
        let final_path = asset_dir.join("primary.mp4");
        std::fs::write(&staging, b"partial").unwrap();
        std::fs::write(&final_path, b"uncommitted").unwrap();

        assert_eq!(manager.recover_incomplete_downloads().unwrap(), 1);
        let recovered = database.get_job(&queued.job.id).unwrap().unwrap();
        assert_eq!(recovered.state, "queued");
        assert_eq!(recovered.progress, 0.0);
        assert!(!staging.exists());
        assert!(!final_path.exists());
        assert_eq!(
            manager.next_queued_download(None).unwrap().unwrap().job.id,
            queued.job.id
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn status_reports_worker_state_and_aggregate_queue_depth() {
        let path = temp_db("worker-status");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let initial = manager.status().unwrap();
        assert_eq!(initial.download_worker.state, VideoWorkerState::Disabled);
        assert_eq!(initial.download_worker.queued_downloads, 0);
        assert!(initial.ok);

        manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "status".into(),
                group: "queue".into(),
                title: "Waiting".into(),
                url: "https://example.invalid/waiting.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let queued = manager.status().unwrap();
        assert_eq!(queued.download_worker.queued_downloads, 1);
        assert_eq!(queued.download_worker.state, VideoWorkerState::Disabled);
        assert!(queued.ok);

        manager
            .set_worker_state(VideoWorkerState::Degraded)
            .unwrap();
        let degraded = manager.status().unwrap();
        assert_eq!(degraded.download_worker.state, VideoWorkerState::Degraded);
        assert!(!degraded.ok);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    const _: () = {
        assert!(PROGRESS_QUEUED < PROGRESS_DOWNLOADING);
        assert!(PROGRESS_DOWNLOADING < PROGRESS_DOWNLOADED);
        assert!(PROGRESS_DOWNLOADED < PROGRESS_CONTAINER_CHECKED);
        assert!(PROGRESS_CONTAINER_CHECKED < PROGRESS_PROBED);
        assert!(PROGRESS_PROBED < PROGRESS_NORMALIZING);
        assert!(PROGRESS_NORMALIZING < 1.0);
    };

    #[test]
    fn job_lookup_and_progress_constants_form_a_monotonic_pipeline() {
        assert_eq!(PROGRESS_QUEUED, 0.0);

        let path = temp_db("job-lookup");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let queued = manager
            .queue_download(QueueDownloadRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "progress".into(),
                group: "queue".into(),
                title: "Progress".into(),
                url: "https://example.invalid/progress.mp4".into(),
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        let job = manager.get_job(None, &queued.job.id).unwrap().unwrap();
        assert_eq!(job.id, queued.job.id);
        assert_eq!(job.progress, PROGRESS_QUEUED);
        assert!(manager
            .get_job(None, &Uuid::new_v4().to_string())
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

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
        assert_eq!(
            manager.status().unwrap().live_runtime,
            VideoLiveRuntimeState::Sleeping
        );
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
}
