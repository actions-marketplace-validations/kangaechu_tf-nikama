//! Google Cloud Storage plan storage.

use super::{PlanStorage, StoredPlan, HEAD_SHA_METADATA_KEY};
use crate::gcs::GcsClient;
use async_trait::async_trait;
use std::collections::BTreeMap;

pub struct GcsPlanStorage {
    client: GcsClient,
    bucket: String,
}

impl GcsPlanStorage {
    pub fn new(client: GcsClient, bucket: &str) -> Self {
        Self {
            client,
            bucket: bucket.to_string(),
        }
    }
}

#[async_trait]
impl PlanStorage for GcsPlanStorage {
    async fn store(&self, key: &str, bytes: &[u8], head_sha: Option<&str>) -> anyhow::Result<()> {
        let mut metadata = BTreeMap::new();
        if let Some(sha) = head_sha {
            metadata.insert(HEAD_SHA_METADATA_KEY.to_string(), sha.to_string());
        }
        self.client
            .upload(
                &self.bucket,
                key,
                "application/octet-stream",
                &metadata,
                bytes,
                false,
            )
            .await?;
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<StoredPlan>> {
        let Some(bytes) = self.client.download(&self.bucket, key).await? else {
            return Ok(None);
        };
        // Custom metadata is not part of the media download; fetch the
        // object resource for it.
        let head_sha = self
            .client
            .info(&self.bucket, key)
            .await?
            .and_then(|i| i.metadata.get(HEAD_SHA_METADATA_KEY).cloned())
            .filter(|s| !s.is_empty());
        Ok(Some(StoredPlan { bytes, head_sha }))
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.client.delete(&self.bucket, key).await?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.client.info(&self.bucket, key).await?.is_some())
    }

    fn name(&self) -> &'static str {
        "gcs"
    }
}
