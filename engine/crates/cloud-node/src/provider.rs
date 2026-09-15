use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{
    HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HOST,
    RANGE,
};
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

#[derive(Clone)]
pub struct ProviderClient {
    client: Client,
    settings: ProviderSettings,
}

impl ProviderClient {
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        let client = Client::builder()
            .https_only(false)
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
                    .await?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                self.client.get(url).send().await?
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
                self.client.get(url).bearer_auth(token).send().await?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                self.apply_http_auth(self.client.get(url))?.send().await?
            }
        };
        response_bytes(response, "download", true).await
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
                .await?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                apply_range(self.client.get(url), range.as_deref())
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
                apply_range(self.client.get(url).bearer_auth(token), range.as_deref())
                    .send()
                    .await?
            }
            ProviderKind::Http => {
                let url = self.http_url(&key)?;
                apply_range(
                    self.apply_http_auth(self.client.get(url))?,
                    range.as_deref(),
                )
                .send()
                .await?
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
                .await?
            }
            ProviderKind::AzureBlob => {
                let url = self.azure_url(&key)?;
                self.client
                    .put(url)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header(CONTENT_TYPE, content_type)
                    .body(bytes)
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
                    .body(bytes)
                    .send()
                    .await?
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
                .await?
            }
        };
        response_bytes(response, "upload", false).await?;
        Ok(())
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

    pub async fn probe(&self) -> anyhow::Result<()> {
        let probe_key = "provider/probe.json";
        let payload = format!(
            "{{\"formatVersion\":1,\"namespace\":{}}}",
            serde_json::to_string(&self.settings.namespace)?
        );
        self.put(probe_key, payload.into_bytes(), "application/json")
            .await?;
        let downloaded = self.get(probe_key).await?.ok_or_else(|| {
            anyhow::anyhow!("Cloud Node provider probe object disappeared after upload")
        })?;
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
        if let Some(body) = body {
            request = request.body(body);
        }
        request
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("Cloud Node amazon-s3 request failed: {error}"))
    }
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
    let status = response.status();
    if allow_not_found && status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(provider_http_error(response, operation).await);
    }
    if operation == "download" {
        Ok(Some(response.bytes().await?.to_vec()))
    } else {
        Ok(None)
    }
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
    if value.trim().is_empty() {
        anyhow::bail!("Cloud Node provider credential environment {name} is empty");
    }
    Ok(value)
}

fn optional_env(configured: Option<&str>, default_name: &str) -> anyhow::Result<Option<String>> {
    let name = configured.unwrap_or(default_name);
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "failed to read Cloud Node provider credential environment {name}: {error}"
        )),
    }
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
