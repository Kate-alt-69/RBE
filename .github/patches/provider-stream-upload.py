from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    return text.replace(old, new, 1)


# Enable request-body streaming only for the client feature.
cargo = Path("engine/crates/cloud-node/Cargo.toml")
text = cargo.read_text()
text = replace_once(
    text,
    'client = ["dep:reqwest"]',
    'client = ["dep:reqwest", "dep:tokio-util"]',
    "client feature dependencies",
)
text = replace_once(
    text,
    'reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "http2"], optional = true }',
    'reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "http2", "stream"], optional = true }\ntokio-util = { version = "0.7", features = ["io"], optional = true }',
    "reqwest streaming dependency",
)
cargo.write_text(text)


# Share the already-proven immutable outbound freezer with provider sync.
client = Path("engine/crates/cloud-node/src/client.rs")
text = client.read_text()
text = replace_once(
    text,
    "async fn prepare_outbound_cache(\n",
    "pub(crate) async fn prepare_outbound_cache(\n",
    "prepare_outbound_cache visibility",
)
text = replace_once(
    text,
    "fn cached_resource_path(\n",
    "pub(crate) fn cached_resource_path(\n",
    "cached_resource_path visibility",
)
text = replace_once(
    text,
    "async fn cleanup_outbound_cache(store: &CloudNodeStore, peer_node_id: &str) -> anyhow::Result<()> {",
    "pub(crate) async fn cleanup_outbound_cache(\n    store: &CloudNodeStore,\n    peer_node_id: &str,\n) -> anyhow::Result<()> {",
    "cleanup_outbound_cache visibility",
)
client.write_text(text)


# Add a file-backed request body and a streaming PUT path for every provider.
provider = Path("engine/crates/cloud-node/src/provider.rs")
p = provider.read_text()
p = replace_once(
    p,
    "use std::time::{SystemTime, UNIX_EPOCH};",
    "use std::path::Path;\nuse std::time::{SystemTime, UNIX_EPOCH};",
    "provider path import",
)
p = replace_once(
    p,
    "    HeaderName, HeaderValue, AUTHORIZATION, CONTENT_RANGE, CONTENT_TYPE, HOST, RANGE,\n};",
    "    HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HOST,\n    RANGE,\n};",
    "provider content length import",
)
p = replace_once(
    p,
    "use sha2::{Digest, Sha256};",
    "use sha2::{Digest, Sha256};\nuse tokio_util::io::ReaderStream;",
    "provider reader stream import",
)

probe_anchor = "    pub async fn probe(&self) -> anyhow::Result<()> {\n"
put_file = r'''    pub(crate) async fn put_file(
        &self,
        relative: &str,
        path: &Path,
        content_type: &str,
        expected_sha256: [u8; 32],
    ) -> anyhow::Result<u64> {
        let metadata = tokio::fs::metadata(path).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to stat Cloud Node provider upload source {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "Cloud Node provider upload source is not a file: {}",
                path.display()
            );
        }
        let size = metadata.len();
        let key = self.object_key(relative)?;
        let response = match self.settings.kind {
            ProviderKind::AmazonS3 => {
                self.aws_file_request(&key, path, content_type, expected_sha256, size)
                    .await?
            }
            ProviderKind::Supabase => {
                let url = self.supabase_url(&key, false)?;
                self.apply_supabase_auth(
                    self.client
                        .put(url)
                        .header("x-upsert", "true")
                        .header(CONTENT_TYPE, content_type)
                        .header(CONTENT_LENGTH, size)
                        .body(file_body(path).await?),
                )?
                .send()
                .await?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                self.client
                    .put(url)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header(CONTENT_TYPE, content_type)
                    .header(CONTENT_LENGTH, size)
                    .body(file_body(path).await?)
                    .send()
                    .await?
            }
            ProviderKind::GoogleCloudStorage => {
                let url = self.gcs_url(&key)?;
                let token = required_env(
                    self.settings
                        .auth
                        .oauth_token_env
                        .as_deref()
                        .or(self.settings.auth.bearer_token_env.as_deref())
                        .or(self.settings.credential_env.as_deref()),
                    GOOGLE_OAUTH_TOKEN_ENV,
                )?;
                self.client
                    .put(url)
                    .bearer_auth(token)
                    .header(CONTENT_TYPE, content_type)
                    .header(CONTENT_LENGTH, size)
                    .body(file_body(path).await?)
                    .send()
                    .await?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                self.apply_http_auth(
                    self.client
                        .put(url)
                        .header(CONTENT_TYPE, content_type)
                        .header(CONTENT_LENGTH, size)
                        .body(file_body(path).await?),
                )?
                .send()
                .await?
            }
        };
        response_bytes(response, "upload", false).await?;
        Ok(size)
    }

'''
p = replace_once(p, probe_anchor, put_file + probe_anchor, "streaming put_file insertion")

start = p.index("    async fn aws_request(\n")
end = p.index("}\n\nasync fn checked_download_response", start)
new_aws = r'''    async fn aws_request(
        &self,
        method: Method,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
        range: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        let payload_sha256: [u8; 32] = Sha256::digest(&body).into();
        let content_length = if body.is_empty() {
            None
        } else {
            Some(
                u64::try_from(body.len())
                    .map_err(|_| anyhow::anyhow!("Cloud Node S3 request body exceeds u64"))?,
            )
        };
        let body = if body.is_empty() {
            None
        } else {
            Some(reqwest::Body::from(body))
        };
        self.aws_signed_request(
            method,
            key,
            body,
            payload_sha256,
            content_type,
            range,
            content_length,
        )
        .await
    }

    async fn aws_file_request(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        payload_sha256: [u8; 32],
        content_length: u64,
    ) -> anyhow::Result<reqwest::Response> {
        self.aws_signed_request(
            Method::PUT,
            key,
            Some(file_body(path).await?),
            payload_sha256,
            Some(content_type),
            None,
            Some(content_length),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn aws_signed_request(
        &self,
        method: Method,
        key: &str,
        body: Option<reqwest::Body>,
        payload_sha256: [u8; 32],
        content_type: Option<&str>,
        range: Option<&str>,
        content_length: Option<u64>,
    ) -> anyhow::Result<reqwest::Response> {
        let region = self
            .settings
            .region
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("amazon-s3 provider region is missing"))?;
        let endpoint = self.settings.endpoint.clone().unwrap_or_else(|| {
            format!(
                "https://{}.s3.{}.amazonaws.com",
                self.settings.bucket, region
            )
        });
        let url = object_url(&endpoint, &encode_object_key(key))?;
        if url.query().is_some() {
            anyhow::bail!("Cloud Node amazon-s3 endpoint cannot contain a query string");
        }
        let access_key = required_env(
            self.settings
                .auth
                .access_key_env
                .as_deref()
                .or(self.settings.access_key_env.as_deref()),
            AMAZON_ACCESS_KEY_ENV,
        )?;
        let secret_key = required_env(
            self.settings
                .auth
                .secret_key_env
                .as_deref()
                .or(self.settings.secret_key_env.as_deref()),
            AMAZON_SECRET_KEY_ENV,
        )?;
        let session_token = optional_env(
            self.settings
                .auth
                .session_token_env
                .as_deref()
                .or(self.settings.session_token_env.as_deref()),
            AMAZON_SESSION_TOKEN_ENV,
        )?;
        let (short_date, amz_date) = aws_timestamp(SystemTime::now())?;
        let payload_hash = hex::encode(payload_sha256);
        let host = host_header(&url)?;
        let canonical_uri = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };

        let mut canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let mut signed_headers = "host;x-amz-content-sha256;x-amz-date".to_owned();
        if let Some(token) = &session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{}\n", token.trim()));
            signed_headers.push_str(";x-amz-security-token");
        }
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method.as_str(),
            canonical_uri,
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let scope = format!("{short_date}/{region}/{AWS_SERVICE}/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let k_date = hmac_sha256(
            format!("AWS4{secret_key}").as_bytes(),
            short_date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, region.as_bytes());
        let k_service = hmac_sha256(&k_region, AWS_SERVICE.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
        );

        let mut request = self
            .client
            .request(method, url)
            .header(HOST, host)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header(AUTHORIZATION, authorization);
        if let Some(token) = session_token {
            request = request.header("x-amz-security-token", token);
        }
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        if let Some(content_length) = content_length {
            request = request.header(CONTENT_LENGTH, content_length);
        }
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("Cloud Node amazon-s3 request failed: {error}"))
    }
'''
p = p[:start] + new_aws + p[end:]

helper_anchor = "async fn checked_download_response(\n"
file_body_helper = r'''async fn file_body(path: &Path) -> anyhow::Result<reqwest::Body> {
    let file = tokio::fs::File::open(path).await.map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node provider upload source {}: {error}",
            path.display()
        )
    })?;
    Ok(reqwest::Body::wrap_stream(ReaderStream::new(file)))
}

'''
p = replace_once(p, helper_anchor, file_body_helper + helper_anchor, "file body helper")
provider.write_text(p)


# Provider pushes now freeze one exact root, stream from it, and retain the
# cache until HEAD proves the provider accepted the state.
sync = Path("engine/crates/cloud-node/src/provider_sync.rs")
s = sync.read_text()
s = replace_once(
    s,
    "use sha2::{Digest, Sha256};",
    "use sha2::{Digest, Sha256};\nuse tokio::io::AsyncReadExt;",
    "provider sync async read import",
)
s = replace_once(
    s,
    "use crate::config::{CloudNodeSettings, ProviderConflictPolicy};",
    "use crate::client::{cached_resource_path, cleanup_outbound_cache, prepare_outbound_cache};\nuse crate::config::{CloudNodeSettings, ProviderConflictPolicy};",
    "provider sync outbound cache import",
)

s = s.replace(
    "push_provider_state(&client, &history, &plan, &local_head, remote.as_ref()).await?;",
    "push_provider_state(\n                &client,\n                &history,\n                store,\n                &plan,\n                &local_head,\n                remote.as_ref(),\n            )\n            .await?;",
)
if s.count("push_provider_state(") != 3:
    raise SystemExit(f"push_provider_state call rewrite count unexpected: {s.count('push_provider_state(')}")
# The second call has deeper indentation but replacement content is valid after rustfmt.

old_push = '''async fn push_provider_state(
    client: &ProviderClient,
    history: &LocalHistory,
    plan: &SyncPlan,
    local_head: &HistoryCommit,
    expected_remote: Option<&HistoryCommit>,
) -> anyhow::Result<()> {
    upload_snapshot(client, plan).await?;
'''
new_push = '''async fn push_provider_state(
    client: &ProviderClient,
    history: &LocalHistory,
    store: &CloudNodeStore,
    plan: &SyncPlan,
    local_head: &HistoryCommit,
    expected_remote: Option<&HistoryCommit>,
) -> anyhow::Result<()> {
    let cache_owner = provider_cache_owner(client.namespace());
    upload_snapshot(client, store, plan, &cache_owner).await?;
'''
s = replace_once(s, old_push, new_push, "push provider cache arguments")
s = replace_once(
    s,
    "    upload_head(client, local_head).await\n}\n\nasync fn pull_provider_state(",
    "    upload_head(client, local_head).await?;\n    cleanup_outbound_cache(store, &cache_owner).await\n}\n\nasync fn pull_provider_state(",
    "provider cache cleanup after HEAD",
)

upload_start = s.index("async fn upload_snapshot(client: &ProviderClient, plan: &SyncPlan) -> anyhow::Result<()> {")
upload_end = s.index("\nasync fn restore_snapshot(", upload_start)
new_upload = r'''async fn upload_snapshot(
    client: &ProviderClient,
    store: &CloudNodeStore,
    plan: &SyncPlan,
    cache_owner: &str,
) -> anyhow::Result<()> {
    let root = plan.root_hex();
    let index_key = format!("snapshots/{root}/index.json");
    if let Some(existing) = client.get(&index_key).await? {
        let snapshot: ProviderSnapshot = serde_json::from_slice(&existing)?;
        validate_snapshot(&snapshot)?;
        if snapshot.root_sha256 == root {
            return Ok(());
        }
        anyhow::bail!("Cloud Node provider snapshot index collision for root {root}");
    }

    let cache_root = prepare_outbound_cache(store, cache_owner, plan).await?;
    let header = plan.header()?;
    let mut resources = Vec::new();
    for object in plan.ordered() {
        let object_hex = hex::encode(object.object_key);
        let content_hex = hex::encode(object.content_sha256);
        let base = format!("snapshots/{root}/objects/{object_hex}/{content_hex}");

        let manifest_path = cached_resource_path(
            &cache_root,
            object,
            TransferResource::Manifest,
            &object.manifest_path,
        )?;
        let (manifest_sha, manifest_size) = hash_and_size(&manifest_path).await?;
        let manifest_key = format!("{base}/{}", object.kind.manifest_name());
        client
            .put_file(
                &manifest_key,
                &manifest_path,
                "application/octet-stream",
                manifest_sha,
            )
            .await?;
        resources.push(resource_record(
            object.kind,
            TransferResource::Manifest,
            object.object_key,
            object.content_sha256,
            manifest_sha,
            manifest_size,
            manifest_key,
        )?);

        match object.kind {
            BlobKind::Folder => {}
            BlobKind::File => {
                let payload_path = object
                    .payload_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Cloud Node file sync object has no payload"))?;
                let cached_payload = cached_resource_path(
                    &cache_root,
                    object,
                    TransferResource::FilePayload,
                    payload_path,
                )?;
                let payload_size = tokio::fs::metadata(&cached_payload).await?.len();
                let payload_key = format!("{base}/payload");
                client
                    .put_file(
                        &payload_key,
                        &cached_payload,
                        "application/octet-stream",
                        object.content_sha256,
                    )
                    .await?;
                resources.push(resource_record(
                    object.kind,
                    TransferResource::FilePayload,
                    object.object_key,
                    object.content_sha256,
                    object.content_sha256,
                    payload_size,
                    payload_key,
                )?);
            }
            BlobKind::Video => {
                for chunk_path in &object.chunk_paths {
                    let cached_chunk = cached_resource_path(
                        &cache_root,
                        object,
                        TransferResource::VideoChunk,
                        chunk_path,
                    )?;
                    let (chunk_sha, chunk_size) = hash_and_size(&cached_chunk).await?;
                    let chunk_hex = hex::encode(chunk_sha);
                    let chunk_key = format!("{base}/chunks/{chunk_hex}.chunk");
                    client
                        .put_file(
                            &chunk_key,
                            &cached_chunk,
                            "application/octet-stream",
                            chunk_sha,
                        )
                        .await?;
                    resources.push(resource_record(
                        object.kind,
                        TransferResource::VideoChunk,
                        object.object_key,
                        object.content_sha256,
                        chunk_sha,
                        chunk_size,
                        chunk_key,
                    )?);
                }
            }
        }
    }

    let snapshot = ProviderSnapshot {
        format_version: SNAPSHOT_VERSION,
        root_sha256: root,
        folder_count: header.folder_count,
        video_count: header.video_count,
        file_count: header.file_count,
        resources,
    };
    validate_snapshot(&snapshot)?;
    client
        .put(
            &index_key,
            serde_json::to_vec(&snapshot)?,
            "application/json",
        )
        .await
}

async fn hash_and_size(path: &Path) -> anyhow::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        size = size
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| anyhow::anyhow!("provider upload read size exceeds u64"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("provider upload size overflow"))?;
    }
    Ok((digest.finalize().into(), size))
}
'''
s = s[:upload_start] + new_upload + s[upload_end:]

s = replace_once(
    s,
    "    size: usize,\n    key: String,",
    "    size: u64,\n    key: String,",
    "provider resource size type",
)
s = replace_once(
    s,
    "        size: u64::try_from(size)\n            .map_err(|_| anyhow::anyhow!(\"Cloud Node provider resource size exceeds u64\"))?,",
    "        size,",
    "provider resource direct size",
)

owner_anchor = "fn provider_recovery_owner(namespace: &str) -> String {\n"
owner_helper = r'''fn provider_cache_owner(namespace: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"RBE-CN-PROVIDER-CACHE/1\0");
    digest.update(namespace.as_bytes());
    format!("provider-{}", hex::encode(digest.finalize()))
}

'''
s = replace_once(s, owner_anchor, owner_helper + owner_anchor, "provider cache owner helper")

test_anchor = "    #[test]\n    fn local_history_tracks_snapshot_ancestry() {\n"
cache_test = r'''    #[test]
    fn provider_cache_owner_is_stable_and_path_safe() {
        let first = provider_cache_owner("prod-primary");
        let second = provider_cache_owner("prod-primary");
        let other = provider_cache_owner("prod-secondary");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.starts_with("provider-"));
        assert!(!first.contains('/'));
        assert!(!first.contains('\\'));
        assert!(!first.contains(".."));
    }

'''
s = replace_once(s, test_anchor, cache_test + test_anchor, "provider cache owner test")
sync.write_text(s)
