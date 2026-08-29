from pathlib import Path


def replace_once(source: str, old: str, new: str, label: str) -> str:
    if old not in source:
        raise SystemExit(f"{label} missing")
    return source.replace(old, new, 1)


# ---------------------------------------------------------------------------
# Video Manager storage/read API.
# ---------------------------------------------------------------------------
path = Path("engine/crates/video-manager/src/lib.rs")
source = path.read_text()
trait_anchor = '''    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
trait_new = '''    fn commit_ready_variant(
        &self,
        job_id: &str,
        variant: &VideoVariant,
    ) -> anyhow::Result<Option<VideoJob>>;
    fn list_variants(&self, _asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
        Ok(Vec::new())
    }
    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>>;
'''
source = replace_once(source, trait_anchor, trait_new, "VideoDatabase variant read hook")

impl_anchor = '''    fn get_asset(&self, database: &str, asset_id: &str) -> anyhow::Result<Option<VideoAsset>> {
'''
impl_method = r'''    fn list_variants(&self, asset_id: &str) -> anyhow::Result<Vec<VideoVariant>> {
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

'''
if impl_anchor not in source:
    raise SystemExit("SQLite get_asset anchor missing")
source = source.replace(impl_anchor, impl_method + impl_anchor, 1)

manager_anchor = '''    pub fn database_health(&self, database: Option<&str>) -> anyhow::Result<DatabaseHealth> {
'''
manager_method = r'''    pub fn list_variants(
        &self,
        database: Option<&str>,
        asset_id: &str,
    ) -> anyhow::Result<Vec<VideoVariant>> {
        let (_, database) = self.resolve_database(database)?;
        database.list_variants(asset_id)
    }

'''
if manager_anchor not in source:
    raise SystemExit("VideoManager database health anchor missing")
source = source.replace(manager_anchor, manager_method + manager_anchor, 1)

# Verify SQLite can read a committed variant back with its trusted path intact.
test_anchor = '''    #[test]
    fn ready_variant_commit_atomically_promotes_asset_and_job() {
'''
test_new = '''    #[test]
    fn ready_variant_commit_atomically_promotes_asset_and_job() {
'''
# Existing commit test is extended after its first committed assertion through a precise snippet.
commit_assert = '''        assert_eq!(committed.state, "ready");
        assert_eq!(committed.progress, 1.0);
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
'''
commit_assert_new = '''        assert_eq!(committed.state, "ready");
        assert_eq!(committed.progress, 1.0);
        let variants = manager.list_variants(None, &queued.asset.id).unwrap();
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].id, variant.id);
        assert_eq!(variants[0].path, variant.path);
        assert_eq!(variants[0].size_bytes, variant.size_bytes);
        let asset = manager.get_asset(None, &queued.asset.id).unwrap().unwrap();
'''
source = replace_once(source, commit_assert, commit_assert_new, "variant read regression")
path.write_text(source)

# ---------------------------------------------------------------------------
# Language facade: ownership check + sanitized metadata (no path).
# ---------------------------------------------------------------------------
path = Path("engine/crates/core/src/video_language.rs")
source = path.read_text()
source = replace_once(
    source,
    '''use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, VideoAssetState, VideoManager, VideoSourceType,
};
''',
    '''use video_manager::{
    CreateAssetRequest, QueueDownloadRequest, VideoAssetState, VideoManager, VideoSourceType,
    VideoVariant,
};
''',
    "VideoLanguage variant import",
)
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
get_new = get_block + '''            "variants" => {
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
'''
source = replace_once(source, get_block, get_new, "VideoLanguage variants dispatch")

helper_anchor = '''fn operation_error(error: anyhow::Error) -> VideoLanguageError {
'''
helper = r'''fn public_variant_value(variant: VideoVariant) -> Value {
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

'''
if helper_anchor not in source:
    raise SystemExit("VideoLanguage operation error anchor missing")
source = source.replace(helper_anchor, helper + helper_anchor, 1)

# Core-level regression: no storage path in module-visible variant data.
tests_end = source.rfind("\n}")
if tests_end < 0:
    raise SystemExit("VideoLanguage tests tail missing")
source = source[:tests_end] + r'''

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
''' + source[tests_end:]
path.write_text(source)

# Boot-time capability registry knows the new direct/namespace function.
path = Path("engine/crates/route-engine/src/modules.rs")
source = path.read_text()
source = replace_once(
    source,
    '''                | "get"
                | "create"
''',
    '''                | "get"
                | "variants"
                | "create"
''',
    "Video Manager builtin variants registry",
)
source = replace_once(
    source,
    '''        assert!(builtin_function_exists("video-manager", "queueDownload"));
''',
    '''        assert!(builtin_function_exists("video-manager", "queueDownload"));
        assert!(builtin_function_exists("vm", "variants"));
''',
    "Video Manager variants registry test",
)
path.write_text(source)
