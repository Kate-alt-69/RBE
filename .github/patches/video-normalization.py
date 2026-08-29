from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()
source = replace_once(
    source,
    "mod ffprobe;\n",
    "mod ffprobe;\nmod ffmpeg;\nmod normalization;\n",
    "video module declarations",
)
source = replace_once(
    source,
    "pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};\n",
    "pub use ffprobe::{FfprobePolicy, MediaProbe, VideoStreamProbe};\npub use ffmpeg::{FfmpegPolicy, NormalizedMedia};\n",
    "video public ffprobe exports",
)

video_job_end = '''pub struct VideoJob {
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
'''
variant = video_job_end + '''
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
'''
source = replace_once(source, video_job_end, variant, "VideoJob model")

trait_anchor = '''    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>>;
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
trait_replacement = '''    fn get_job(&self, job_id: &str) -> anyhow::Result<Option<VideoJob>>;
    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
source = replace_once(source, trait_anchor, trait_replacement, "VideoDatabase trait hook")

impl_anchor = '''    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>> {
'''
commit_method = r'''    fn commit_ready_variant(
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
        if variant.fps.is_some_and(|fps| !fps.is_finite() || fps <= 0.0) {
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
            anyhow::bail!("Video Manager normalization job {job_id:?} lost its state during commit");
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

'''
if impl_anchor not in source:
    raise SystemExit("Sqlite get_asset insertion anchor missing")
source = source.replace(impl_anchor, commit_method + impl_anchor, 1)

manager_fields = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    live_idle_secs: u64,
}
'''
manager_fields_new = '''pub struct VideoManager {
    databases: RwLock<HashMap<String, Arc<dyn VideoDatabase>>>,
    default_database: String,
    quarantine_root: PathBuf,
    media_root: PathBuf,
    live_idle_secs: u64,
}
'''
source = replace_once(source, manager_fields, manager_fields_new, "VideoManager fields")

open_anchor = '''        let quarantine_root = std::fs::canonicalize(&quarantine_root).with_context(|| {
            format!(
                "canonicalize Video Manager quarantine directory {}",
                quarantine_root.display()
            )
        })?;
        let mut databases = HashMap::new();
'''
open_replacement = '''        let quarantine_root = std::fs::canonicalize(&quarantine_root).with_context(|| {
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
'''
source = replace_once(source, open_anchor, open_replacement, "VideoManager media root creation")
source = replace_once(
    source,
    '''            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root,
            live_idle_secs,
''',
    '''            default_database: DEFAULT_DATABASE_NAME.into(),
            quarantine_root,
            media_root,
            live_idle_secs,
''',
    "VideoManager initializer",
)

validate_anchor = '''fn validate_segment(label: &str, value: &str) -> anyhow::Result<()> {
'''
relative_helper = r'''fn validate_relative_media_path(value: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
        })
    {
        anyhow::bail!("Video Manager media path must be a non-empty relative normal path");
    }
    Ok(())
}

'''
if validate_anchor not in source:
    raise SystemExit("media path validator insertion anchor missing")
source = source.replace(validate_anchor, relative_helper + validate_anchor, 1)

# Atomic DB promotion regression.
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("video-manager tests tail missing")
source = source[:tests_end] + r'''

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
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
        assert_eq!(asset.state, VideoAssetState::Ready);
        assert!(database
            .commit_ready_variant(&queued.job.id, &variant)
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# Remove the old public API name from developer-facing language errors.
path = Path("crates/core/src/video_language.rs")
source = path.read_text()
source = source.replace(
    'format!("unknown video language function {other:?}"),',
    'format!("unknown Video Manager language function {other:?}"),',
)
source = source.replace(
    '"use video.queueDownload() for download assets so they enter quarantine",',
    '"use vm.queueDownload() or video-manager.queueDownload() for download assets so they enter quarantine",',
)
source = source.replace(
    '"video.{function}() expects {minimum}..={maximum} argument(s), got {}",',
    '"Video Manager {function}() expects {minimum}..={maximum} argument(s), got {}",',
)
source = source.replace(
    'format!("video argument {index} ({label}) must be a string"),',
    'format!("Video Manager argument {index} ({label}) must be a string"),',
)
source = source.replace(
    'format!("optional video {label} must be a string or null"),',
    'format!("optional Video Manager {label} must be a string or null"),',
)
path.write_text(source)
