//! Plan file storage between the plan and apply runs.

mod gcs;
mod s3;

pub use gcs::GcsPlanStorage;
pub use s3::S3PlanStorage;

use async_trait::async_trait;
use std::sync::Arc;

/// Name of the object metadata entry holding the PR head SHA the plan was
/// produced from (`x-amz-meta-head-sha` on S3).
pub const HEAD_SHA_METADATA_KEY: &str = "head-sha";

/// A stored plan file together with the commit it was produced from.
/// `head_sha` is `None` for plans written without metadata (older versions
/// of this tool, or Digger).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPlan {
    pub bytes: Vec<u8>,
    pub head_sha: Option<String>,
}

#[async_trait]
pub trait PlanStorage: Send + Sync {
    /// Store `bytes` under `key`, recording `head_sha` when given.
    async fn store(&self, key: &str, bytes: &[u8], head_sha: Option<&str>) -> anyhow::Result<()>;
    async fn retrieve(&self, key: &str) -> anyhow::Result<Option<StoredPlan>>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
    fn name(&self) -> &'static str;
    fn is_noop(&self) -> bool {
        false
    }
}

/// Storage that keeps nothing: `apply` plans immediately before applying.
pub struct NoopPlanStorage;

#[async_trait]
impl PlanStorage for NoopPlanStorage {
    async fn store(
        &self,
        _key: &str,
        _bytes: &[u8],
        _head_sha: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn retrieve(&self, _key: &str) -> anyhow::Result<Option<StoredPlan>> {
        Ok(None)
    }
    async fn delete(&self, _key: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn name(&self) -> &'static str {
        "none"
    }
    fn is_noop(&self) -> bool {
        true
    }
}

/// Create the plan storage from `PLAN_UPLOAD_DESTINATION` (`aws`, `gcp` or
/// empty).
pub async fn from_env() -> anyhow::Result<Arc<dyn PlanStorage>> {
    let dest = crate::util::env_opt("PLAN_UPLOAD_DESTINATION")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match dest.as_str() {
        "" | "none" => Ok(Arc::new(NoopPlanStorage)),
        "aws" | "s3" => {
            let bucket = crate::util::env_opt("AWS_S3_BUCKET").ok_or_else(|| {
                anyhow::anyhow!("AWS_S3_BUCKET is required when PLAN_UPLOAD_DESTINATION=aws")
            })?;
            let encryption = if crate::util::env_is_true("PLAN_UPLOAD_S3_ENCRYPTION_ENABLED") {
                let kind = crate::util::env_opt("PLAN_UPLOAD_S3_ENCRYPTION_TYPE")
                    .unwrap_or_else(|| "AES256".to_string())
                    .to_ascii_uppercase();
                match kind.as_str() {
                    "AES256" => Some(s3::Encryption::Aes256),
                    "KMS" => Some(s3::Encryption::Kms(
                        crate::util::env_opt("PLAN_UPLOAD_S3_ENCRYPTION_KMS_ID").ok_or_else(|| {
                            anyhow::anyhow!("PLAN_UPLOAD_S3_ENCRYPTION_KMS_ID is required for KMS encryption")
                        })?,
                    )),
                    other => anyhow::bail!("unknown PLAN_UPLOAD_S3_ENCRYPTION_TYPE '{}'", other),
                }
            } else {
                None
            };
            Ok(Arc::new(S3PlanStorage::new(&bucket, encryption).await?))
        }
        "gcp" | "gcs" => {
            let bucket = crate::util::env_opt("GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET")
                .map(|b| b.trim().to_ascii_lowercase())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET is required when PLAN_UPLOAD_DESTINATION=gcp"
                    )
                })?;
            let client = crate::gcs::GcsClient::from_env()?;
            Ok(Arc::new(GcsPlanStorage::new(client, &bucket)))
        }
        "github" | "azure" | "gitlab" => anyhow::bail!(
            "PLAN_UPLOAD_DESTINATION={} is not supported by this tool (only aws, gcp or empty)",
            dest
        ),
        other => anyhow::bail!("unknown PLAN_UPLOAD_DESTINATION '{}'", other),
    }
}
