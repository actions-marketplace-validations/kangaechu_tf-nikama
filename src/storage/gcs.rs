//! Google Cloud Storage plan storage.

use super::PlanStorage;
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
    async fn store(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.client
            .upload(
                &self.bucket,
                key,
                "application/octet-stream",
                &BTreeMap::new(),
                bytes,
                false,
            )
            .await?;
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.client.download(&self.bucket, key).await
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
