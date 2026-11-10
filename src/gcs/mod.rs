//! Minimal Google Cloud Storage client over the JSON API, shared by the GCS
//! plan storage and the GCS lock backend.
//!
//! Credentials come from Application Default Credentials: the file named by
//! `GOOGLE_APPLICATION_CREDENTIALS` (a service account key or the workload
//! identity federation file written by `google-github-actions/auth`), the
//! gcloud well-known file, or the metadata server.

#[cfg(test)]
mod tests;

use google_cloud_auth::credentials::{AccessTokenCredentials, Builder};
use reqwest::StatusCode;
use std::collections::BTreeMap;

pub const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";

const MULTIPART_BOUNDARY: &str = "nikama-gcs-7f3a9c1e5b2d4e8f";

enum Auth {
    /// Application Default Credentials.
    Adc(AccessTokenCredentials),
    /// A fixed bearer token (tests).
    #[allow(dead_code)]
    Static(String),
    /// No `Authorization` header (`STORAGE_EMULATOR_HOST`).
    Anonymous,
}

pub struct GcsClient {
    http: reqwest::Client,
    endpoint: String,
    auth: Auth,
}

/// Object attributes used by the lock backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectInfo {
    pub name: String,
    pub generation: Option<i64>,
    pub metadata: BTreeMap<String, String>,
}

impl ObjectInfo {
    fn from_resource(v: &serde_json::Value) -> Self {
        let metadata = v["metadata"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: v["name"].as_str().unwrap_or_default().to_string(),
            generation: v["generation"].as_str().and_then(|g| g.parse().ok()),
            metadata,
        }
    }
}

/// Outcome of an upload with `ifGenerationMatch=0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upload {
    Written,
    /// The precondition failed: the object already exists.
    AlreadyExists,
}

impl GcsClient {
    /// Client for the real service (or the emulator named by
    /// `STORAGE_EMULATOR_HOST`, which needs no credentials).
    pub fn from_env() -> anyhow::Result<Self> {
        if let Some(host) = crate::util::env_opt("STORAGE_EMULATOR_HOST") {
            let endpoint = if host.contains("://") {
                host
            } else {
                format!("http://{}", host)
            };
            tracing::info!(endpoint, "using the Cloud Storage emulator");
            return Ok(Self::new(&endpoint, Auth::Anonymous));
        }
        let creds = Builder::default()
            .build_access_token_credentials()
            .map_err(|e| {
                anyhow::anyhow!(
                    "could not load Google Cloud credentials (set GOOGLE_APPLICATION_CREDENTIALS, \
                     e.g. with google-github-actions/auth): {}",
                    e
                )
            })?;
        Ok(Self::new(DEFAULT_ENDPOINT, Auth::Adc(creds)))
    }

    /// Client that sends a fixed bearer token to `endpoint`.
    #[allow(dead_code)]
    pub fn with_token(endpoint: &str, token: &str) -> Self {
        Self::new(endpoint, Auth::Static(token.to_string()))
    }

    fn new(endpoint: &str, auth: Auth) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            auth,
        }
    }

    async fn authorize(
        &self,
        req: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(match &self.auth {
            Auth::Adc(creds) => {
                let token = creds.access_token().await.map_err(|e| {
                    anyhow::anyhow!("could not get a Google Cloud access token: {}", e)
                })?;
                req.bearer_auth(token.token)
            }
            Auth::Static(token) => req.bearer_auth(token),
            Auth::Anonymous => req,
        })
    }

    fn object_url(&self, bucket: &str, object: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint,
            urlencoding::encode(bucket),
            urlencoding::encode(object)
        )
    }

    /// Upload `body` as `object` (multipart upload so that custom metadata can
    /// be set in the same request). With `create_only` the request carries
    /// `ifGenerationMatch=0` and an existing object is reported instead of
    /// being overwritten.
    pub async fn upload(
        &self,
        bucket: &str,
        object: &str,
        content_type: &str,
        metadata: &BTreeMap<String, String>,
        body: &[u8],
        create_only: bool,
    ) -> anyhow::Result<Upload> {
        let mut url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=multipart",
            self.endpoint,
            urlencoding::encode(bucket)
        );
        if create_only {
            url.push_str("&ifGenerationMatch=0");
        }
        let mut meta = serde_json::json!({ "name": object, "contentType": content_type });
        if !metadata.is_empty() {
            meta["metadata"] = serde_json::to_value(metadata)?;
        }
        let mut payload = Vec::with_capacity(body.len() + 512);
        payload.extend_from_slice(
            format!(
                "--{b}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{m}\r\n--{b}\r\nContent-Type: {c}\r\n\r\n",
                b = MULTIPART_BOUNDARY,
                m = meta,
                c = content_type
            )
            .as_bytes(),
        );
        payload.extend_from_slice(body);
        payload.extend_from_slice(format!("\r\n--{}--\r\n", MULTIPART_BOUNDARY).as_bytes());

        let req = self
            .http
            .post(&url)
            .header(
                "Content-Type",
                format!("multipart/related; boundary={}", MULTIPART_BOUNDARY),
            )
            .body(payload);
        let resp = self.authorize(req).await?.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(Upload::Written);
        }
        if create_only && status == StatusCode::PRECONDITION_FAILED {
            return Ok(Upload::AlreadyExists);
        }
        Err(api_error("upload", bucket, object, status, resp).await)
    }

    /// Object content, or `None` when it does not exist.
    pub async fn download(&self, bucket: &str, object: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let url = format!("{}?alt=media", self.object_url(bucket, object));
        let resp = self.authorize(self.http.get(&url)).await?.send().await?;
        match resp.status() {
            s if s.is_success() => Ok(Some(resp.bytes().await?.to_vec())),
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(api_error("download", bucket, object, s, resp).await),
        }
    }

    /// Object attributes, or `None` when it does not exist.
    pub async fn info(&self, bucket: &str, object: &str) -> anyhow::Result<Option<ObjectInfo>> {
        let url = self.object_url(bucket, object);
        let resp = self.authorize(self.http.get(&url)).await?.send().await?;
        match resp.status() {
            s if s.is_success() => {
                let v: serde_json::Value = resp.json().await?;
                Ok(Some(ObjectInfo::from_resource(&v)))
            }
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(api_error("get", bucket, object, s, resp).await),
        }
    }

    /// Objects whose name starts with `prefix`, with their metadata.
    pub async fn list(&self, bucket: &str, prefix: &str) -> anyhow::Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/storage/v1/b/{}/o?prefix={}",
                self.endpoint,
                urlencoding::encode(bucket),
                urlencoding::encode(prefix)
            );
            if let Some(t) = &page_token {
                url.push_str(&format!("&pageToken={}", urlencoding::encode(t)));
            }
            let resp = self.authorize(self.http.get(&url)).await?.send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(api_error("list", bucket, prefix, status, resp).await);
            }
            let v: serde_json::Value = resp.json().await?;
            if let Some(items) = v["items"].as_array() {
                out.extend(items.iter().map(ObjectInfo::from_resource));
            }
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => page_token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Delete `object`. Returns whether it existed.
    pub async fn delete(&self, bucket: &str, object: &str) -> anyhow::Result<bool> {
        let url = self.object_url(bucket, object);
        let resp = self.authorize(self.http.delete(&url)).await?.send().await?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => Err(api_error("delete", bucket, object, s, resp).await),
        }
    }
}

async fn api_error(
    op: &str,
    bucket: &str,
    object: &str,
    status: StatusCode,
    resp: reqwest::Response,
) -> anyhow::Error {
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(512).collect();
    anyhow::anyhow!(
        "gcs {} gs://{}/{} failed: HTTP {}: {}",
        op,
        bucket,
        object,
        status,
        body.trim()
    )
}
