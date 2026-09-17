use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    ETAG, HOST, IF_MATCH, IF_NONE_MATCH, RANGE,
};
use reqwest::redirect::Policy;
use reqwest::{Client, Method, RequestBuilder, StatusCode, Url};
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;

use crate::config::{ProviderAuthMode, ProviderKind, ProviderSettings};

const AWS_SERVICE: &str = "s3";
const AMAZON_ACCESS_KEY_ENV: &str = "RBE_CN_PROV_AMAZON_ACCESS_KEY";
const AMAZON_SECRET_KEY_ENV: &str = "RBE_CN_PROV_AMAZON_SECRET_KEY";
const AMAZON_SESSION_TOKEN_ENV: &str = "RBE_CN_PROV_AMAZON_SESSION_TOKEN";
const SUPABASE_API_KEY_ENV: &str = "RBE_CN_PROV_SUPABASE_API_KEY";
const AZURE_SAS_TOKEN_ENV: &str = "RBE_CN_PROV_AZURE_SAS_TOKEN";
const GOOGLE_OAUTH_TOKEN_ENV: &str = "RBE_CN_PROV_GOOGLE_OAUTH_TOKEN";
const HTTP_API_KEY_ENV: &str = "RBE_CN_PROV_HTTP_API_KEY";
const HTTP_BEARER_TOKEN_ENV: &str = "RBE_CN_PROV_HTTP_BEARER_TOKEN";
const HTTP_USERNAME_ENV: &str = "RBE_CN_PROV_HTTP_USERNAME";
const HTTP_PASSWORD_ENV: &str = "RBE_CN_PROV_HTTP_PASSWORD";
const HTTP_HEADER_VALUE_ENV: &str = "RBE_CN_PROV_HTTP_HEADER_VALUE";
const MAX_PROVIDER_ERROR_BYTES: usize = 1024;
const MAX_PROVIDER_METADATA_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROVIDER_CONTROL_METADATA_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_SECRET_FILE_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct ProviderClient {
    client: Client,
    settings: ProviderSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderObjectVersion {
    Etag(String),
    Generation(u64),
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderMetadataObject {
    pub bytes: Vec<u8>,
    pub version: Option<ProviderObjectVersion>,
}

impl ProviderClient {
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        let client = Client::builder()
            .https_only(false)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_millis(settings.connect_timeout_ms))
            .read_timeout(Duration::from_millis(settings.read_timeout_ms))
            .build()
            .map_err(|error| {
                anyhow::anyhow!("failed to build Cloud Node provider client: {error}")
            })?;
        Ok(Self {
            client,
            settings: settings.clone(),
        })
    }

    pub fn kind(&self) -> ProviderKind {
        self.settings.kind
    }

    pub fn namespace(&self) -> &str {
        &self.settings.namespace
    }

    pub fn object_key(&self, relative: &str) -> anyhow::Result<String> {
        if relative.is_empty()
            || relative.starts_with('/')
            || relative.contains("..")
            || relative.contains('\\')
        {
            anyhow::bail!("invalid Cloud Node provider object key {relative:?}");
        }
        let mut parts = Vec::new();
        let prefix = self.settings.prefix.trim_matches('/');
        if !prefix.is_empty() {
            parts.push(prefix);
        }
        parts.push("rbe-cn");
        parts.push(&self.settings.namespace);
        parts.push(relative);
        Ok(parts.join("/"))
    }

    pub fn target_description(&self) -> String {
        match self.settings.kind {
            ProviderKind::AmazonS3 => {
                format!("s3://{}/{}", self.settings.bucket, self.settings.namespace)
            }
            ProviderKind::Supabase => format!(
                "supabase://{}/{}",
                self.settings.bucket, self.settings.namespace
            ),
            ProviderKind::AzureBlob => format!(
                "azure://{}/{}",
                self.settings.bucket, self.settings.namespace
            ),
            ProviderKind::GoogleCloudStorage => {
                format!("gs://{}/{}", self.settings.bucket, self.settings.namespace)
            }
            ProviderKind::Http => format!(
                "http-provider://{}/{}",
                self.settings.bucket, self.settings.namespace
            ),
        }
    }

    pub async fn get(&self, relative: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.get_limited(relative, MAX_PROVIDER_METADATA_BYTES)
            .await
    }

    pub(crate) async fn get_limited(
        &self,
        relative: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .get_versioned_limited(relative, max_bytes)
            .await?
            .map(|object| object.bytes))
    }

    pub(crate) async fn get_versioned_limited(
        &self,
        relative: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Option<ProviderMetadataObject>> {
        let key = self.object_key(relative)?;
        let response = match self.settings.kind {
            ProviderKind::AmazonS3 => {
                self.aws_request(Method::GET, &key, Vec::new(), None, None)
                    .await?
            }
            ProviderKind::Supabase => {
                let url = self.supabase_url(&key, true)?;
                self.apply_supabase_auth(self.client.get(url))?
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                self.client
                    .get(url)
                    .send()
                    .await
                    .map_err(provider_transport_error)?
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
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                self.apply_http_auth(self.client.get(url))?
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
        };
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(provider_http_error(response, "download").await);
        }
        let version = provider_object_version(self.settings.kind, response.headers())?;
        let bytes = response_bytes_limited(response, "download", false, max_bytes)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Cloud Node provider successful metadata response disappeared")
            })?;
        Ok(Some(ProviderMetadataObject { bytes, version }))
    }

    pub(crate) async fn open_download(
        &self,
        relative: &str,
        start: u64,
    ) -> anyhow::Result<Option<reqwest::Response>> {
        let key = self.object_key(relative)?;
        let range = (start > 0).then(|| format!("bytes={start}-"));
        let response = match self.settings.kind {
            ProviderKind::AmazonS3 => {
                self.aws_request(Method::GET, &key, Vec::new(), None, range.as_deref())
                    .await?
            }
            ProviderKind::Supabase => {
                let url = self.supabase_url(&key, true)?;
                apply_range(
                    self.apply_supabase_auth(self.client.get(url))?,
                    range.as_deref(),
                )
                .send()
                .await
                .map_err(provider_transport_error)?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                apply_range(self.client.get(url), range.as_deref())
                    .send()
                    .await
                    .map_err(provider_transport_error)?
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
                apply_range(self.client.get(url).bearer_auth(token), range.as_deref())
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                apply_range(
                    self.apply_http_auth(self.client.get(url))?,
                    range.as_deref(),
                )
                .send()
                .await
                .map_err(provider_transport_error)?
            }
        };
        checked_download_response(response, start).await
    }

    pub async fn put(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> anyhow::Result<()> {
        let key = self.object_key(relative)?;
        let response = match self.settings.kind {
            ProviderKind::AmazonS3 => {
                self.aws_request(Method::PUT, &key, bytes, Some(content_type), None)
                    .await?
            }
            ProviderKind::Supabase => {
                let url = self.supabase_url(&key, false)?;
                self.apply_supabase_auth(
                    self.client
                        .request(Self::supabase_upload_method(), url)
                        .header("x-upsert", "true")
                        .header(CONTENT_TYPE, content_type)
                        .body(bytes),
                )?
                .send()
                .await
                .map_err(provider_transport_error)?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                self.client
                    .put(url)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header(CONTENT_TYPE, content_type)
                    .body(bytes)
                    .send()
                    .await
                    .map_err(provider_transport_error)?
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
                    .body(bytes)
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                self.apply_http_auth(
                    self.client
                        .put(url)
                        .header(CONTENT_TYPE, content_type)
                        .body(bytes),
                )?
                .send()
                .await
                .map_err(provider_transport_error)?
            }
        };
        response_bytes(response, "upload", false).await?;
        Ok(())
    }

    pub(crate) async fn put_if_unchanged(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        content_type: &str,
        expected_version: Option<&ProviderObjectVersion>,
        expected_exists: bool,
    ) -> anyhow::Result<bool> {
        if !expected_exists && expected_version.is_some() {
            anyhow::bail!("Cloud Node provider conditional write cannot expect a version for a missing object");
        }
        let key = self.object_key(relative)?;
        let response = match self.settings.kind {
            ProviderKind::AmazonS3 => {
                if expected_exists
                    && !matches!(expected_version, Some(ProviderObjectVersion::Etag(_)))
                {
                    anyhow::bail!("Cloud Node S3 provider omitted the ETag required for atomic HEAD publication");
                }
                let payload_sha256: [u8; 32] = Sha256::digest(&bytes).into();
                let content_length = u64::try_from(bytes.len())
                    .map_err(|_| anyhow::anyhow!("Cloud Node S3 request body exceeds u64"))?;
                self.aws_signed_request(
                    Method::PUT,
                    &key,
                    Some(reqwest::Body::from(bytes)),
                    payload_sha256,
                    Some(content_type),
                    None,
                    Some(content_length),
                    expected_version,
                    !expected_exists,
                )
                .await?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                let request = self
                    .client
                    .put(url)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header(CONTENT_TYPE, content_type)
                    .body(bytes);
                apply_etag_precondition(request, expected_version, expected_exists)?
                    .send()
                    .await
                    .map_err(provider_transport_error)?
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
                let generation = gcs_generation_precondition(expected_version, expected_exists)?;
                self.client
                    .put(url)
                    .bearer_auth(token)
                    .header(CONTENT_TYPE, content_type)
                    .header("x-goog-if-generation-match", generation)
                    .body(bytes)
                    .send()
                    .await
                    .map_err(provider_transport_error)?
            }
            ProviderKind::Supabase | ProviderKind::Http => {
                self.put(relative, bytes, content_type).await?;
                return Ok(false);
            }
        };
        conditional_write_response(response, expected_exists).await?;
        Ok(true)
    }

    pub(crate) async fn put_file(
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
                        .request(Self::supabase_upload_method(), url)
                        .header("x-upsert", "true")
                        .header(CONTENT_TYPE, content_type)
                        .header(CONTENT_LENGTH, size)
                        .body(file_body(path).await?),
                )?
                .send()
                .await
                .map_err(provider_transport_error)?
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
                    .await
                    .map_err(provider_transport_error)?
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
                    .await
                    .map_err(provider_transport_error)?
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
                .await
                .map_err(provider_transport_error)?
            }
        };
        response_bytes(response, "upload", false).await?;
        Ok(size)
    }

    pub async fn probe(&self) -> anyhow::Result<()> {
        let probe_key = "provider/probe.json";
        let payload = format!(
            "{{\"formatVersion\":1,\"namespace\":{}}}",
            serde_json::to_string(&self.settings.namespace)?
        );
        self.put(probe_key, payload.into_bytes(), "application/json")
            .await?;
        self.probe_read_only().await
    }

    pub async fn probe_read_only(&self) -> anyhow::Result<()> {
        let downloaded = self
            .get_limited("provider/probe.json", MAX_PROVIDER_CONTROL_METADATA_BYTES)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Cloud Node provider probe object is missing"))?;
        if downloaded.is_empty() {
            anyhow::bail!("Cloud Node provider probe returned an empty object");
        }
        Ok(())
    }

    fn supabase_upload_method() -> Method {
        Method::POST
    }

    fn apply_supabase_auth(&self, request: RequestBuilder) -> anyhow::Result<RequestBuilder> {
        match self.settings.auth.mode {
            ProviderAuthMode::Auto | ProviderAuthMode::ApiKey => {
                let token = required_env(
                    self.settings
                        .auth
                        .api_key_env
                        .as_deref()
                        .or(self.settings.credential_env.as_deref()),
                    SUPABASE_API_KEY_ENV,
                )?;
                Ok(request.header("apikey", token.as_str()).bearer_auth(token))
            }
            ProviderAuthMode::Bearer => {
                let token = required_env(
                    self.settings
                        .auth
                        .bearer_token_env
                        .as_deref()
                        .or(self.settings.credential_env.as_deref()),
                    SUPABASE_API_KEY_ENV,
                )?;
                Ok(request.bearer_auth(token))
            }
            ProviderAuthMode::Header => self.apply_custom_header(request),
            other => anyhow::bail!("unsupported Supabase provider auth mode {other:?}"),
        }
    }

    fn apply_http_auth(&self, request: RequestBuilder) -> anyhow::Result<RequestBuilder> {
        match self.settings.auth.mode {
            ProviderAuthMode::Auto => {
                let token = optional_env(
                    self.settings
                        .auth
                        .bearer_token_env
                        .as_deref()
                        .or(self.settings.credential_env.as_deref()),
                    HTTP_BEARER_TOKEN_ENV,
                )?;
                Ok(match token {
                    Some(token) => request.bearer_auth(token),
                    None => request,
                })
            }
            ProviderAuthMode::None => Ok(request),
            ProviderAuthMode::ApiKey => {
                let token =
                    required_env(self.settings.auth.api_key_env.as_deref(), HTTP_API_KEY_ENV)?;
                let header_name = self
                    .settings
                    .auth
                    .header_name
                    .as_deref()
                    .unwrap_or("x-api-key");
                add_header(request, header_name, &token)
            }
            ProviderAuthMode::Bearer | ProviderAuthMode::OAuthBearer => {
                let token = required_env(
                    self.settings.auth.bearer_token_env.as_deref().or(self
                        .settings
                        .auth
                        .oauth_token_env
                        .as_deref()),
                    HTTP_BEARER_TOKEN_ENV,
                )?;
                Ok(request.bearer_auth(token))
            }
            ProviderAuthMode::Basic => {
                let username = required_env(
                    self.settings.auth.username_env.as_deref(),
                    HTTP_USERNAME_ENV,
                )?;
                let password = required_env(
                    self.settings.auth.password_env.as_deref(),
                    HTTP_PASSWORD_ENV,
                )?;
                Ok(request.basic_auth(username, Some(password)))
            }
            ProviderAuthMode::Header => self.apply_custom_header(request),
            ProviderAuthMode::AwsSigV4 | ProviderAuthMode::AzureSas => {
                anyhow::bail!("generic HTTP provider cannot use cloud-specific auth mode")
            }
        }
    }

    fn apply_custom_header(&self, request: RequestBuilder) -> anyhow::Result<RequestBuilder> {
        let header_name = self.settings.auth.header_name.as_deref().ok_or_else(|| {
            anyhow::anyhow!("Cloud Node provider header auth requires headerName")
        })?;
        let value = required_env(
            self.settings
                .auth
                .header_value_env
                .as_deref()
                .or(self.settings.credential_env.as_deref()),
            HTTP_HEADER_VALUE_ENV,
        )?;
        add_header(request, header_name, &value)
    }

    fn supabase_url(&self, key: &str, authenticated_read: bool) -> anyhow::Result<Url> {
        let endpoint = self
            .settings
            .endpoint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("supabase provider endpoint is missing"))?;
        let route = if authenticated_read {
            "storage/v1/object/authenticated"
        } else {
            "storage/v1/object"
        };
        object_url(
            endpoint,
            &format!(
                "{route}/{}/{}",
                encode_path_segment(&self.settings.bucket),
                encode_object_key(key)
            ),
        )
    }

    fn azure_url(&self, key: &str) -> anyhow::Result<Url> {
        let endpoint = if let Some(value) = &self.settings.endpoint {
            value.clone()
        } else {
            let account = self
                .settings
                .account
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("azure provider account is missing"))?;
            format!("https://{account}.blob.core.windows.net")
        };
        let mut url = object_url(
            &endpoint,
            &format!(
                "{}/{}",
                encode_path_segment(&self.settings.bucket),
                encode_object_key(key)
            ),
        )?;
        let sas = required_env(
            self.settings
                .auth
                .sas_token_env
                .as_deref()
                .or(self.settings.credential_env.as_deref()),
            AZURE_SAS_TOKEN_ENV,
        )?;
        let sas = sas.trim_start_matches('?');
        if sas.is_empty() {
            anyhow::bail!("Cloud Node Azure SAS token is empty");
        }
        url.set_query(Some(sas));
        Ok(url)
    }

    fn gcs_url(&self, key: &str) -> anyhow::Result<Url> {
        let endpoint = self
            .settings
            .endpoint
            .as_deref()
            .unwrap_or("https://storage.googleapis.com");
        object_url(
            endpoint,
            &format!(
                "{}/{}",
                encode_path_segment(&self.settings.bucket),
                encode_object_key(key)
            ),
        )
    }

    fn http_url(&self, key: &str) -> anyhow::Result<Url> {
        let endpoint = self
            .settings
            .endpoint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("http provider endpoint is missing"))?;
        object_url(
            endpoint,
            &format!(
                "{}/{}",
                encode_path_segment(&self.settings.bucket),
                encode_object_key(key)
            ),
        )
    }

    fn aws_url(&self, key: &str, region: &str) -> anyhow::Result<Url> {
        if let Some(endpoint) = self.settings.endpoint.as_deref() {
            return object_url(
                endpoint,
                &format!(
                    "{}/{}",
                    encode_path_segment(&self.settings.bucket),
                    encode_object_key(key)
                ),
            );
        }
        let endpoint = format!(
            "https://{}.s3.{}.amazonaws.com",
            self.settings.bucket, region
        );
        object_url(&endpoint, &encode_object_key(key))
    }

    async fn aws_request(
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
            None,
            false,
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
            None,
            false,
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
        expected_version: Option<&ProviderObjectVersion>,
        require_absent: bool,
    ) -> anyhow::Result<reqwest::Response> {
        let region = self
            .settings
            .region
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("amazon-s3 provider region is missing"))?;
        let url = self.aws_url(key, region)?;
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
        if require_absent {
            if expected_version.is_some() {
                anyhow::bail!(
                    "Cloud Node S3 conditional write cannot require absent and match an ETag"
                );
            }
            request = request.header(IF_NONE_MATCH, "*");
        } else if let Some(version) = expected_version {
            let ProviderObjectVersion::Etag(etag) = version else {
                anyhow::bail!("Cloud Node S3 conditional write requires an ETag version");
            };
            request = request.header(IF_MATCH, etag);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().await.map_err(provider_transport_error)
    }
}

fn provider_transport_error(error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "Cloud Node provider request failed: {}",
        error.without_url()
    )
}

fn provider_object_version(
    kind: ProviderKind,
    headers: &HeaderMap,
) -> anyhow::Result<Option<ProviderObjectVersion>> {
    match kind {
        ProviderKind::AmazonS3 | ProviderKind::AzureBlob => headers
            .get(ETAG)
            .map(|value| {
                value
                    .to_str()
                    .map(|value| ProviderObjectVersion::Etag(value.to_owned()))
                    .map_err(|error| {
                        anyhow::anyhow!("Cloud Node provider returned invalid ETag: {error}")
                    })
            })
            .transpose(),
        ProviderKind::GoogleCloudStorage => headers
            .get("x-goog-generation")
            .map(|value| {
                let value = value.to_str().map_err(|error| {
                    anyhow::anyhow!(
                        "Cloud Node GCS provider returned invalid generation header: {error}"
                    )
                })?;
                let generation = value.parse::<u64>().map_err(|error| {
                    anyhow::anyhow!(
                        "Cloud Node GCS provider returned invalid generation {value:?}: {error}"
                    )
                })?;
                Ok(ProviderObjectVersion::Generation(generation))
            })
            .transpose(),
        ProviderKind::Supabase | ProviderKind::Http => Ok(None),
    }
}

fn apply_etag_precondition(
    request: RequestBuilder,
    expected_version: Option<&ProviderObjectVersion>,
    expected_exists: bool,
) -> anyhow::Result<RequestBuilder> {
    match (expected_exists, expected_version) {
        (false, None) => Ok(request.header(IF_NONE_MATCH, "*")),
        (true, Some(ProviderObjectVersion::Etag(etag))) => Ok(request.header(IF_MATCH, etag)),
        (true, None) => anyhow::bail!(
            "Cloud Node provider omitted the ETag required for atomic HEAD publication"
        ),
        (_, Some(ProviderObjectVersion::Generation(_))) => {
            anyhow::bail!("Cloud Node ETag conditional write received a generation version")
        }
        (false, Some(ProviderObjectVersion::Etag(_))) => anyhow::bail!(
            "Cloud Node provider conditional write cannot match an ETag for a missing object"
        ),
    }
}

fn gcs_generation_precondition(
    expected_version: Option<&ProviderObjectVersion>,
    expected_exists: bool,
) -> anyhow::Result<u64> {
    match (expected_exists, expected_version) {
        (false, None) => Ok(0),
        (true, Some(ProviderObjectVersion::Generation(generation))) => Ok(*generation),
        (true, None) => anyhow::bail!(
            "Cloud Node GCS provider omitted the generation required for atomic HEAD publication"
        ),
        (_, Some(ProviderObjectVersion::Etag(_))) => {
            anyhow::bail!("Cloud Node GCS conditional write received an ETag version")
        }
        (false, Some(ProviderObjectVersion::Generation(_))) => anyhow::bail!(
            "Cloud Node GCS conditional write cannot match a generation for a missing object"
        ),
    }
}

async fn conditional_write_response(
    response: reqwest::Response,
    expected_exists: bool,
) -> anyhow::Result<()> {
    let status = response.status();
    if status == StatusCode::PRECONDITION_FAILED
        || status == StatusCode::CONFLICT
        || (expected_exists && status == StatusCode::NOT_FOUND)
    {
        let error = provider_http_error(response, "conditional HEAD publish").await;
        anyhow::bail!("Cloud Node provider HEAD changed during atomic publish; retry synchronization: {error}");
    }
    response_bytes(response, "conditional HEAD publish", false).await?;
    Ok(())
}

async fn file_body(path: &Path) -> anyhow::Result<reqwest::Body> {
    let file = tokio::fs::File::open(path).await.map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node provider upload source {}: {error}",
            path.display()
        )
    })?;
    Ok(reqwest::Body::wrap_stream(ReaderStream::new(file)))
}

async fn checked_download_response(
    response: reqwest::Response,
    start: u64,
) -> anyhow::Result<Option<reqwest::Response>> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(provider_http_error(response, "download").await);
    }
    if start > 0 && status == StatusCode::PARTIAL_CONTENT {
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .ok_or_else(|| {
                anyhow::anyhow!("Cloud Node provider partial download omitted Content-Range")
            })?
            .to_str()
            .map_err(|error| {
                anyhow::anyhow!(
                    "Cloud Node provider returned invalid Content-Range header: {error}"
                )
            })?;
        let actual_start = content_range_start(content_range).ok_or_else(|| {
            anyhow::anyhow!(
                "Cloud Node provider returned malformed Content-Range {content_range:?}"
            )
        })?;
        if actual_start != start {
            anyhow::bail!(
                "Cloud Node provider range started at {actual_start} instead of requested {start}"
            );
        }
    }
    Ok(Some(response))
}

async fn response_bytes(
    response: reqwest::Response,
    operation: &str,
    allow_not_found: bool,
) -> anyhow::Result<Option<Vec<u8>>> {
    response_bytes_limited(
        response,
        operation,
        allow_not_found,
        MAX_PROVIDER_METADATA_BYTES,
    )
    .await
}

async fn response_bytes_limited(
    mut response: reqwest::Response,
    operation: &str,
    allow_not_found: bool,
    max_metadata_bytes: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let status = response.status();
    if allow_not_found && status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(provider_http_error(response, operation).await);
    }
    if operation != "download" {
        return Ok(None);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_metadata_bytes as u64)
    {
        anyhow::bail!(
            "Cloud Node provider metadata exceeds {} bytes",
            max_metadata_bytes
        );
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(max_metadata_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await? {
        append_bounded_bytes(&mut bytes, &chunk, max_metadata_bytes, "provider metadata")?;
    }
    Ok(Some(bytes))
}

fn append_bounded_bytes(
    target: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
    label: &str,
) -> anyhow::Result<()> {
    let next = target
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| anyhow::anyhow!("Cloud Node {label} size overflow"))?;
    if next > limit {
        anyhow::bail!("Cloud Node {label} exceeds {limit} bytes");
    }
    target.extend_from_slice(chunk);
    Ok(())
}

async fn provider_http_error(mut response: reqwest::Response, operation: &str) -> anyhow::Error {
    let status = response.status();
    let mut detail = Vec::new();
    while detail.len() < MAX_PROVIDER_ERROR_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = (MAX_PROVIDER_ERROR_BYTES - detail.len()).min(chunk.len());
                detail.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let detail = String::from_utf8_lossy(&detail);
    anyhow::anyhow!("Cloud Node provider {operation} failed with HTTP {status}: {detail}")
}

fn apply_range(request: RequestBuilder, range: Option<&str>) -> RequestBuilder {
    match range {
        Some(range) => request.header(RANGE, range),
        None => request,
    }
}

fn content_range_start(value: &str) -> Option<u64> {
    let value = value.strip_prefix("bytes ")?;
    let (range, _) = value.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    start.parse().ok()
}

fn add_header(request: RequestBuilder, name: &str, value: &str) -> anyhow::Result<RequestBuilder> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        anyhow::anyhow!("invalid Cloud Node provider auth header name: {error}")
    })?;
    let value = HeaderValue::from_str(value).map_err(|error| {
        anyhow::anyhow!("invalid Cloud Node provider auth header value: {error}")
    })?;
    Ok(request.header(name, value))
}

fn required_env(configured: Option<&str>, default_name: &str) -> anyhow::Result<String> {
    let name = configured.unwrap_or(default_name);
    let value = std::env::var(name).map_err(|_| {
        anyhow::anyhow!("Cloud Node provider credential environment {name} is not set")
    })?;
    resolve_secret_value(name, &value)?.ok_or_else(|| {
        anyhow::anyhow!("Cloud Node provider credential environment {name} is empty")
    })
}

fn optional_env(configured: Option<&str>, default_name: &str) -> anyhow::Result<Option<String>> {
    let name = configured.unwrap_or(default_name);
    match std::env::var(name) {
        Ok(value) => resolve_secret_value(name, &value),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "failed to read Cloud Node provider credential environment {name}: {error}"
        )),
    }
}

fn resolve_secret_value(name: &str, value: &str) -> anyhow::Result<Option<String>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let Some(file_name) = value.strip_prefix("file:") else {
        return Ok(Some(value.to_owned()));
    };
    if file_name.is_empty() {
        anyhow::bail!("Cloud Node provider credential environment {name} has an empty file: path");
    }
    let path = Path::new(file_name);
    if !path.is_absolute() {
        anyhow::bail!("Cloud Node provider credential file from {name} must use an absolute path");
    }
    let file = std::fs::File::open(path).map_err(|error| {
        anyhow::anyhow!(
            "failed to open Cloud Node provider credential file {} from {name}: {error}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PROVIDER_SECRET_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to read Cloud Node provider credential file {} from {name}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_PROVIDER_SECRET_FILE_BYTES {
        anyhow::bail!(
            "Cloud Node provider credential file {} exceeds {} bytes",
            path.display(),
            MAX_PROVIDER_SECRET_FILE_BYTES
        );
    }
    let value = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "Cloud Node provider credential file {} from {name} is not UTF-8",
            path.display()
        )
    })?;
    let value = value.trim_end_matches(&['\r', '\n'][..]);
    if value.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(value.to_owned()))
}

fn object_url(endpoint: &str, path: &str) -> anyhow::Result<Url> {
    let base = endpoint.trim_end_matches('/');
    Url::parse(&format!("{base}/{}", path.trim_start_matches('/')))
        .map_err(|error| anyhow::anyhow!("invalid Cloud Node provider URL: {error}"))
}

fn encode_object_key(value: &str) -> String {
    value
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + (value - 10)) as char,
    }
}

fn host_header(url: &Url) -> anyhow::Result<String> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Cloud Node provider URL has no host"))?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        normalized[..32].copy_from_slice(&digest);
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn aws_timestamp(now: SystemTime) -> anyhow::Result<(String, String)> {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is before UNIX epoch"))?
        .as_secs();
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| anyhow::anyhow!("system clock exceeds supported AWS timestamp range"))?;
    let seconds_of_day = seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    let short = format!("{year:04}{month:02}{day:02}");
    let full = format!("{short}T{hour:02}{minute:02}{second:02}Z");
    Ok((short, full))
}

// Howard Hinnant's civil-from-days transform, with day zero at 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_default_endpoint_uses_virtual_hosted_bucket() {
        let settings: ProviderSettings = serde_json::from_value(serde_json::json!({
            "kind":"amazon-s3",
            "namespace":"production",
            "bucket":"rbe-bucket",
            "region":"ap-south-1"
        }))
        .unwrap();
        let client = ProviderClient::new(&settings).unwrap();
        let url = client
            .aws_url("rbe-cn/production/history/HEAD.json", "ap-south-1")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://rbe-bucket.s3.ap-south-1.amazonaws.com/rbe-cn/production/history/HEAD.json"
        );
    }

    #[test]
    fn s3_custom_endpoint_uses_path_style_bucket() {
        let settings: ProviderSettings = serde_json::from_value(serde_json::json!({
            "kind":"amazon-s3",
            "namespace":"production",
            "bucket":"rbe-bucket",
            "region":"auto",
            "endpoint":"https://objects.example.invalid"
        }))
        .unwrap();
        let client = ProviderClient::new(&settings).unwrap();
        let url = client
            .aws_url("rbe-cn/production/history/HEAD.json", "auto")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://objects.example.invalid/rbe-bucket/rbe-cn/production/history/HEAD.json"
        );
    }

    #[test]
    fn supabase_upserts_use_upload_method() {
        assert_eq!(ProviderClient::supabase_upload_method(), Method::POST);
    }

    #[test]
    fn provider_object_keys_are_namespaced() {
        let settings: ProviderSettings = serde_json::from_value(serde_json::json!({
            "kind":"google-cloud-storage",
            "namespace":"production",
            "bucket":"rbe",
            "prefix":"tenant-a"
        }))
        .unwrap();
        let client = ProviderClient::new(&settings).unwrap();
        assert_eq!(
            client.object_key("history/head.json").unwrap(),
            "tenant-a/rbe-cn/production/history/head.json"
        );
    }

    #[test]
    fn supabase_uses_authenticated_read_route() {
        let settings: ProviderSettings = serde_json::from_value(serde_json::json!({
            "kind":"supabase",
            "namespace":"production",
            "bucket":"private-backups",
            "endpoint":"https://example.supabase.co"
        }))
        .unwrap();
        let client = ProviderClient::new(&settings).unwrap();
        assert_eq!(
            client
                .supabase_url("history/HEAD.json", true)
                .unwrap()
                .path(),
            "/storage/v1/object/authenticated/private-backups/history/HEAD.json"
        );
        assert_eq!(
            client
                .supabase_url("history/HEAD.json", false)
                .unwrap()
                .path(),
            "/storage/v1/object/private-backups/history/HEAD.json"
        );
    }

    #[test]
    fn credential_file_values_are_reloaded_and_keep_inner_whitespace() {
        let path = std::env::temp_dir().join(format!(
            "rbe-provider-secret-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"first secret\r\n").unwrap();
        let reference = format!("file:{}", path.display());
        assert_eq!(
            resolve_secret_value("RBE_TEST_SECRET", &reference)
                .unwrap()
                .as_deref(),
            Some("first secret")
        );
        std::fs::write(&path, b"second secret\n").unwrap();
        assert_eq!(
            resolve_secret_value("RBE_TEST_SECRET", &reference)
                .unwrap()
                .as_deref(),
            Some("second secret")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn credential_file_reference_requires_absolute_path() {
        assert!(resolve_secret_value("RBE_TEST_SECRET", "file:relative/token").is_err());
    }

    #[test]
    fn provider_native_versions_parse_etags_and_gcs_generations() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"etag-1\""));
        assert_eq!(
            provider_object_version(ProviderKind::AmazonS3, &headers).unwrap(),
            Some(ProviderObjectVersion::Etag("\"etag-1\"".to_owned()))
        );
        headers.remove(ETAG);
        headers.insert("x-goog-generation", HeaderValue::from_static("42"));
        assert_eq!(
            provider_object_version(ProviderKind::GoogleCloudStorage, &headers).unwrap(),
            Some(ProviderObjectVersion::Generation(42))
        );
    }

    #[test]
    fn provider_native_preconditions_cover_create_and_replace() {
        let request = apply_etag_precondition(
            Client::new().put("https://example.invalid/object"),
            None,
            false,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(request.headers().get(IF_NONE_MATCH).unwrap(), "*");

        let version = ProviderObjectVersion::Etag("\"etag-2\"".to_owned());
        let request = apply_etag_precondition(
            Client::new().put("https://example.invalid/object"),
            Some(&version),
            true,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(request.headers().get(IF_MATCH).unwrap(), "\"etag-2\"");
        assert_eq!(gcs_generation_precondition(None, false).unwrap(), 0);
        assert_eq!(
            gcs_generation_precondition(Some(&ProviderObjectVersion::Generation(99)), true)
                .unwrap(),
            99
        );
    }

    #[test]
    fn bounded_provider_metadata_rejects_oversize_chunks() {
        let mut bytes = Vec::new();
        append_bounded_bytes(&mut bytes, b"1234", 5, "test metadata").unwrap();
        assert_eq!(bytes, b"1234");
        assert!(append_bounded_bytes(&mut bytes, b"56", 5, "test metadata").is_err());
        assert_eq!(bytes, b"1234");
    }

    #[test]
    fn aws_epoch_timestamp_is_stable() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200);
        let (short, full) = aws_timestamp(now).unwrap();
        assert_eq!(short, "20240101");
        assert_eq!(full, "20240101T000000Z");
    }

    #[test]
    fn path_encoding_preserves_object_hierarchy() {
        assert_eq!(encode_object_key("a folder/x+y"), "a%20folder/x%2By");
    }
}
