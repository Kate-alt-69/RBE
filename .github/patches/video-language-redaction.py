from pathlib import Path

path = Path("crates/core/src/video_language.rs")
source = path.read_text()

source = source.replace(
'''                    Some(asset) => {
                        ensure_owned(module_owner, &asset.namespace)?;
                        json_result(serde_json::to_value(asset))
                    }
''',
'''                    Some(asset) => {
                        ensure_owned(module_owner, &asset.namespace)?;
                        Ok(public_asset_value(asset))
                    }
''',
1,
)
source = source.replace(
'''                let asset = manager
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
''',
'''                let asset = manager
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
''',
1,
)
source = source.replace(
'''                let queued = manager
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
''',
'''                let queued = manager
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
''',
1,
)

asset_anchor = '''fn public_job_value(job: video_manager::VideoJob) -> Value {
'''
asset_view = r'''fn public_asset_value(asset: video_manager::VideoAsset) -> Value {
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

'''
if asset_anchor not in source:
    raise SystemExit("public asset view anchor missing")
source = source.replace(asset_anchor, asset_view + asset_anchor, 1)

old_error = '''fn operation_error(error: anyhow::Error) -> VideoLanguageError {
    VideoLanguageError::new("VID3002", error.to_string())
}
'''
new_error = '''fn operation_error(error: anyhow::Error) -> VideoLanguageError {
    tracing::warn!(error = %error, "Video Manager language operation failed");
    VideoLanguageError::new("VID3002", "Video Manager operation failed")
}
'''
if old_error not in source:
    raise SystemExit("Video Manager language error mapper anchor missing")
source = source.replace(old_error, new_error, 1)

last = source.rfind("\n}")
if last < 0:
    raise SystemExit("video language tests tail missing")
tests = r'''

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
'''
source = source[:last] + tests + source[last:]
path.write_text(source)
