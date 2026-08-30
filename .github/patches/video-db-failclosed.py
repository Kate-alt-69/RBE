from pathlib import Path

path = Path("crates/video-manager/src/lib.rs")
source = path.read_text()

old = '''    fn parse(value: &str) -> Self {
        match value {
            "quarantined" => Self::Quarantined,
            "processing" => Self::Processing,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "deleted" => Self::Deleted,
            _ => Self::Reserved,
        }
    }
'''
new = '''    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "quarantined" => Ok(Self::Quarantined),
            "processing" => Ok(Self::Processing),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            "deleted" => Ok(Self::Deleted),
            other => anyhow::bail!("Video Manager stored invalid asset state {other:?}"),
        }
    }
'''
if old not in source:
    raise SystemExit("VideoAssetState::parse anchor missing")
source = source.replace(old, new, 1)

old = '''            .optional()?
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
'''
new = '''            .optional()?
            .map(|row| -> anyhow::Result<VideoAsset> {
                let source_type = parse_source_type(&row.3)?;
                let metadata = serde_json::from_str(&row.5).with_context(|| {
                    format!("parse Video Manager asset {} metadata JSON", row.0)
                })?;
                Ok(VideoAsset {
                    uri: format!("vm://{}/{}/{}", row.8, row.9, row.0),
                    id: row.0,
                    database: database.to_string(),
                    namespace: row.8,
                    group: row.9,
                    title: row.1,
                    state: VideoAssetState::parse(&row.2)?,
                    source_type,
                    source_uri: row.4,
                    metadata,
                    created_at_ms: row.6,
                    updated_at_ms: row.7,
                })
            })
            .transpose()
'''
if old not in source:
    raise SystemExit("get_asset fail-open row anchor missing")
source = source.replace(old, new, 1)

old = '''fn parse_source_type(value: &str) -> VideoSourceType {
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
'''
new = '''fn parse_source_type(value: &str) -> anyhow::Result<VideoSourceType> {
    match value {
        "upload" => Ok(VideoSourceType::Upload),
        "download" => Ok(VideoSourceType::Download),
        "local" => Ok(VideoSourceType::Local),
        "generated" => Ok(VideoSourceType::Generated),
        "live" => Ok(VideoSourceType::Live),
        "recorded_live" => Ok(VideoSourceType::RecordedLive),
        other => anyhow::bail!("Video Manager stored invalid source type {other:?}"),
    }
}
'''
if old not in source:
    raise SystemExit("parse_source_type anchor missing")
source = source.replace(old, new, 1)

pipe_trait = '''trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

'''
if ".pipe(" not in source:
    if pipe_trait not in source:
        raise SystemExit("unused Pipe trait anchor missing")
    source = source.replace(pipe_trait, "", 1)

insert = r'''

    #[test]
    fn corrupted_stored_asset_rows_fail_closed() {
        let path = temp_db("corrupted-asset-row");
        let manager = VideoManager::open_default(&path, 7200).unwrap();
        let asset = manager
            .create_asset(CreateAssetRequest {
                database: None,
                namespace_kind: "module".into(),
                namespace_owner: "corruption-test".into(),
                group: "rows".into(),
                title: "Corrupt me".into(),
                source_type: VideoSourceType::Generated,
                source_uri: None,
                metadata: serde_json::json!({"valid": true}),
                initial_state: VideoAssetState::Reserved,
            })
            .unwrap();
        let connection = Connection::open(&path).unwrap();

        connection
            .execute(
                "UPDATE video_assets SET state = 'mystery' WHERE id = ?1",
                params![asset.id],
            )
            .unwrap();
        let error = manager.get_asset(None, &asset.id).unwrap_err();
        assert!(error.to_string().contains("invalid asset state"));

        connection
            .execute(
                "UPDATE video_assets SET state = 'reserved', source_type = 'mystery' WHERE id = ?1",
                params![asset.id],
            )
            .unwrap();
        let error = manager.get_asset(None, &asset.id).unwrap_err();
        assert!(error.to_string().contains("invalid source type"));

        connection
            .execute(
                "UPDATE video_assets SET source_type = 'generated', metadata_json = '{broken' WHERE id = ?1",
                params![asset.id],
            )
            .unwrap();
        let error = manager.get_asset(None, &asset.id).unwrap_err();
        assert!(error.to_string().contains("metadata JSON"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
'''
end = source.rfind("\n}")
if end < 0:
    raise SystemExit("video-manager tests tail missing")
source = source[:end] + insert + source[end:]

path.write_text(source)
