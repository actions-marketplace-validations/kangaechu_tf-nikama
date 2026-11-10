//! PR locks as objects in a Google Cloud Storage bucket.
//!
//! Compatible with Digger's `gcp` lock provider: the object is named after the
//! resource (`<owner>/<repo>#<project>`) and carries the holder in the
//! `LockId` custom metadata key. Unlike Digger, the object is created with
//! `ifGenerationMatch=0`, so two concurrent runs cannot both believe they won.

use super::Lock;
use crate::gcs::{GcsClient, Upload};
use async_trait::async_trait;
use std::collections::BTreeMap;

pub const LOCK_ID_KEY: &str = "LockId";

pub struct GcsLock {
    client: GcsClient,
    bucket: String,
}

impl GcsLock {
    pub fn new(client: GcsClient, bucket: &str) -> Self {
        Self {
            client,
            bucket: bucket.to_string(),
        }
    }
}

#[async_trait]
impl Lock for GcsLock {
    async fn lock(&self, pr: u64, resource: &str) -> anyhow::Result<bool> {
        let mut metadata = BTreeMap::new();
        metadata.insert(LOCK_ID_KEY.to_string(), pr.to_string());
        metadata.insert(
            "CreatedAt".to_string(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        let outcome = self
            .client
            .upload(&self.bucket, resource, "text/plain", &metadata, b"", true)
            .await?;
        Ok(outcome == Upload::Written)
    }

    async fn unlock(&self, resource: &str) -> anyhow::Result<bool> {
        self.client.delete(&self.bucket, resource).await
    }

    async fn holder(&self, resource: &str) -> anyhow::Result<Option<u64>> {
        let Some(info) = self.client.info(&self.bucket, resource).await? else {
            return Ok(None);
        };
        let raw = info.metadata.get(LOCK_ID_KEY).ok_or_else(|| {
            anyhow::anyhow!(
                "lock object gs://{}/{} has no {} metadata",
                self.bucket,
                resource,
                LOCK_ID_KEY
            )
        })?;
        raw.trim().parse::<u64>().map(Some).map_err(|_| {
            anyhow::anyhow!(
                "lock object gs://{}/{} has an invalid {} '{}'",
                self.bucket,
                resource,
                LOCK_ID_KEY,
                raw
            )
        })
    }

    async fn held_by(&self, pr: u64, prefix: &str) -> anyhow::Result<Vec<String>> {
        let wanted = pr.to_string();
        Ok(self
            .client
            .list(&self.bucket, prefix)
            .await?
            .into_iter()
            .filter(|o| {
                o.metadata
                    .get(LOCK_ID_KEY)
                    .is_some_and(|v| v.trim() == wanted)
            })
            .map(|o| o.name)
            .collect())
    }

    fn name(&self) -> &'static str {
        "gcs"
    }
}
