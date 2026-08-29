from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Video Manager: public trusted job lookup + meaningful aggregate stage progress.
# ---------------------------------------------------------------------------
path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()

const_anchor = 'pub const DEFAULT_DATABASE_NAME: &str = "default";\n'
consts = '''pub const DEFAULT_DATABASE_NAME: &str = "default";

pub(crate) const PROGRESS_QUEUED: f64 = 0.0;
pub(crate) const PROGRESS_DOWNLOADING: f64 = 0.05;
pub(crate) const PROGRESS_DOWNLOADED: f64 = 0.45;
pub(crate) const PROGRESS_CONTAINER_CHECKED: f64 = 0.55;
pub(crate) const PROGRESS_PROBED: f64 = 0.70;
pub(crate) const PROGRESS_NORMALIZING: f64 = 0.75;
'''
source = replace_once(source, const_anchor, consts, "Video Manager progress constants")

source = replace_once(
    source,
    '''            progress: 0.0,
''',
    '''            progress: PROGRESS_QUEUED,
''',
    "queued progress initializer",
)

# Mark the just-claimed download as actually started before network work.
claim_anchor = '''        if claimed.asset_id != queued.asset.id || claimed.job_type != "download" {
            let detail = "Video Manager claimed job does not match its queued download asset/type";
            let _ = database.update_job(&claimed.id, "failed", 0.0, Some(detail));
'''
claim_new = '''        if claimed.asset_id != queued.asset.id || claimed.job_type != "download" {
            let detail = "Video Manager claimed job does not match its queued download asset/type";
            let _ = database.update_job(
                &claimed.id,
                "failed",
                PROGRESS_DOWNLOADING,
                Some(detail),
            );
'''
source = replace_once(source, claim_anchor, claim_new, "download identity progress")

claimed_download_anchor = '''        let mut claimed_download = queued.clone();
        claimed_download.job = claimed;
        match self
'''
claimed_download_new = '''        database.update_job(
            &claimed.id,
            "downloading",
            PROGRESS_DOWNLOADING,
            None,
        )?;
        let mut claimed_download = queued.clone();
        claimed_download.job = claimed;
        match self
'''
source = replace_once(source, claimed_download_anchor, claimed_download_new, "download start progress")
source = replace_once(
    source,
    '''                database.update_job(&queued.job.id, "downloaded", 1.0, None)?;
''',
    '''                database.update_job(
                    &queued.job.id,
                    "downloaded",
                    PROGRESS_DOWNLOADED,
                    None,
                )?;
''',
    "download completed progress",
)
source = replace_once(
    source,
    '''                    database.update_job(&queued.job.id, "failed", 0.0, Some(&detail))
''',
    '''                    database.update_job(
                        &queued.job.id,
                        "failed",
                        PROGRESS_DOWNLOADING,
                        Some(&detail),
                    )
''',
    "download failure progress",
)

# Container gate progress.
source = replace_once(
    source,
    '''            let _ = database.update_job(&transitioned.id, "failed", 1.0, Some(detail));
''',
    '''            let _ = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_DOWNLOADED,
                Some(detail),
            );
''',
    "container identity failure progress",
)
source = replace_once(
    source,
    '''                database.update_job(&transitioned.id, "container_checked", 1.0, None)?;
''',
    '''                database.update_job(
                    &transitioned.id,
                    "container_checked",
                    PROGRESS_CONTAINER_CHECKED,
                    None,
                )?;
''',
    "container success progress",
)
source = replace_once(
    source,
    '''                    database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))
''',
    '''                    database.update_job(
                        &transitioned.id,
                        "failed",
                        PROGRESS_DOWNLOADED,
                        Some(&detail),
                    )
''',
    "container failure progress",
)

# FFprobe progress; this is the next remaining matching identity/failure block.
source = replace_once(
    source,
    '''            let _ = database.update_job(&transitioned.id, "failed", 1.0, Some(detail));
''',
    '''            let _ = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_CONTAINER_CHECKED,
                Some(detail),
            );
''',
    "FFprobe identity failure progress",
)
source = replace_once(
    source,
    '''                database.update_job(&transitioned.id, "probed", 1.0, None)?;
''',
    '''                database.update_job(
                    &transitioned.id,
                    "probed",
                    PROGRESS_PROBED,
                    None,
                )?;
''',
    "FFprobe success progress",
)
source = replace_once(
    source,
    '''                    database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))
''',
    '''                    database.update_job(
                        &transitioned.id,
                        "failed",
                        PROGRESS_CONTAINER_CHECKED,
                        Some(&detail),
                    )
''',
    "FFprobe failure progress",
)

# Recovery uses named zero progress.
source = source.replace("progress = 0.0, error = NULL", "progress = 0.0, error = NULL", 1)

# Public trusted manager lookup.
manager_anchor = '''    pub fn list_variants(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Vec<VideoVariant>> {
'''
manager_method = '''    pub fn get_job(
        &self,
        database: Option<&str>,
        job_id: &str,
    ) -> anyhow::Result<Option<VideoJob>> {
        let (_, database) = self.resolve_database(database)?;
        database.get_job(job_id)
    }

'''
if manager_anchor not in source:
    raise SystemExit("VideoManager list variants anchor missing")
source = source.replace(manager_anchor, manager_method + manager_anchor, 1)

# Test named stage progress through DB transitions without network/ffmpeg.
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("Video Manager tests tail missing")
source = source[:tests_end] + r'''

    #[test]
    fn job_lookup_and_progress_constants_form_a_monotonic_pipeline() {
        assert_eq!(PROGRESS_QUEUED, 0.0);
        assert!(PROGRESS_QUEUED < PROGRESS_DOWNLOADING);
        assert!(PROGRESS_DOWNLOADING < PROGRESS_DOWNLOADED);
        assert!(PROGRESS_DOWNLOADED < PROGRESS_CONTAINER_CHECKED);
        assert!(PROGRESS_CONTAINER_CHECKED < PROGRESS_PROBED);
        assert!(PROGRESS_PROBED < PROGRESS_NORMALIZING);
        assert!(PROGRESS_NORMALIZING < 1.0);

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
        assert!(manager.get_job(None, &Uuid::new_v4().to_string()).unwrap().is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
''' + source[tests_end:]
path.write_text(source)

# ---------------------------------------------------------------------------
# Normalization stage progress.
# ---------------------------------------------------------------------------
path = Path("engine/crates/video-manager/src/normalization.rs")
source = path.read_text()
source = replace_once(
    source,
    '''use crate::{FfmpegPolicy, QueuedDownload, VideoManager, VideoVariant};
''',
    '''use crate::{
    FfmpegPolicy, QueuedDownload, VideoManager, VideoVariant, PROGRESS_NORMALIZING,
    PROGRESS_PROBED,
};
''',
    "normalization progress imports",
)
source = replace_once(
    source,
    '''            let _ = database.update_job(&transitioned.id, "failed", 1.0, Some(detail));
''',
    '''            let _ = database.update_job(
                &transitioned.id,
                "failed",
                PROGRESS_PROBED,
                Some(detail),
            );
''',
    "normalization identity failure progress",
)
transitioned_anchor = '''        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
'''
transitioned_new = '''        database.update_job(
            &transitioned.id,
            "normalizing",
            PROGRESS_NORMALIZING,
            None,
        )?;
        let quarantine = self.quarantine_path(&transitioned.asset_id, &transitioned.id)?;
'''
source = replace_once(source, transitioned_anchor, transitioned_new, "normalization start progress")
# Three normalization failure updates should retain NORMALIZING progress.
source = source.replace(
    'database.update_job(&transitioned.id, "failed", 1.0, Some(&detail))',
    '''database.update_job(
                    &transitioned.id,
                    "failed",
                    PROGRESS_NORMALIZING,
                    Some(&detail),
                )''',
)
path.write_text(source)

# ---------------------------------------------------------------------------
# Module language: ownership-scoped sanitized vm.job().
# ---------------------------------------------------------------------------
path = Path("engine/crates/core/src/video_language.rs")
source = path.read_text()
get_block = '''            "get" => {
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
'''
job_block = get_block + '''            "job" => {
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
'''
source = replace_once(source, get_block, job_block, "VideoLanguage job dispatch")

helper_anchor = '''fn public_variant_value(variant: VideoVariant) -> Value {
'''
job_helper = r'''fn public_job_value(job: video_manager::VideoJob) -> Value {
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

'''
if helper_anchor not in source:
    raise SystemExit("public variant helper anchor missing")
source = source.replace(helper_anchor, job_helper + helper_anchor, 1)

tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("VideoLanguage tests tail missing")
source = source[:tests_end] + r'''

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
''' + source[tests_end:]
path.write_text(source)

# ---------------------------------------------------------------------------
# Boot-time registry.
# ---------------------------------------------------------------------------
path = Path("engine/crates/route-engine/src/modules.rs")
source = path.read_text()
source = replace_once(
    source,
    '''                | "get"
                | "variants"
''',
    '''                | "get"
                | "job"
                | "variants"
''',
    "Video Manager job builtin registry",
)
source = replace_once(
    source,
    '''        assert!(builtin_function_exists("vm", "variants"));
''',
    '''        assert!(builtin_function_exists("vm", "variants"));
        assert!(builtin_function_exists("video-manager", "job"));
''',
    "Video Manager job registry test",
)
path.write_text(source)
