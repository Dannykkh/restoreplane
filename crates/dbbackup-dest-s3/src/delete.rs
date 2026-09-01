//! S3 retention delete authority.
//!
//! The backup writer never constructs this type. Production construction requires a separate
//! credential and exact object version identity. A versionless request is rejected before any S3
//! call so a delete marker can never be mistaken for permanent deletion of a protected version.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_pipeline::{DeleteErrorV2, DestinationDeleteV2};
use opendal::{ErrorKind, Operator};
use zeroize::Zeroize;

use crate::admin::{ObjectLockProbeError, ObjectLockStatus, S3Admin};
use crate::{object_key, S3Error};

const S3_DELETE_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Delete-only IAM policy template for a versioned backup bucket.
///
/// It permits permanent deletion of an explicitly selected version and the two read probes needed
/// to distinguish Object Lock/legal hold from a generic IAM denial. It cannot write backup data,
/// mutate retention, or bypass Governance retention.
pub const DELETE_ONLY_IAM_POLICY: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "DeleteExactExpiredVersion",
      "Effect": "Allow",
      "Action": [
        "s3:DeleteObjectVersion",
        "s3:GetObjectRetention",
        "s3:GetObjectLegalHold"
      ],
      "Resource": "arn:aws:s3:::BACKUP-BUCKET/*"
    },
    {
      "Sid": "DenyWriterAndLockMutation",
      "Effect": "Deny",
      "Action": [
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:PutObjectRetention",
        "s3:PutObjectLegalHold",
        "s3:BypassGovernanceRetention",
        "s3:PutLifecycleConfiguration",
        "s3:PutBucketVersioning",
        "s3:PutBucketObjectLockConfiguration"
      ],
      "Resource": [
        "arn:aws:s3:::BACKUP-BUCKET",
        "arn:aws:s3:::BACKUP-BUCKET/*"
      ]
    }
  ]
}"#;

/// Credentials and endpoint used only by the destructive retention worker.
#[derive(Clone, Debug)]
pub struct S3DeleteCfg {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl Drop for S3DeleteCfg {
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
    }
}

pub struct S3DeleteOperatorV2 {
    op: Operator,
    prefix: String,
    lock_probe: Option<S3Admin>,
}

impl S3DeleteOperatorV2 {
    /// Builds the production delete path with its own credential and trusted lock probes.
    pub fn new(cfg: S3DeleteCfg) -> Result<Self, S3Error> {
        validate_config(&cfg)?;
        opendal::install_default();

        let mut builder = opendal::services::S3::default()
            .bucket(&cfg.bucket)
            .region(&cfg.region)
            .access_key_id(&cfg.access_key_id)
            .secret_access_key(&cfg.secret_access_key);
        if !cfg.endpoint.is_empty() {
            builder = builder.endpoint(&cfg.endpoint);
        }
        let op = Operator::new(builder)?;
        let lock_probe = S3Admin::from_credentials(
            &cfg.endpoint,
            &cfg.region,
            &cfg.bucket,
            &cfg.access_key_id,
            &cfg.secret_access_key,
            0,
        );
        Ok(Self {
            op,
            prefix: cfg.prefix.trim_matches('/').to_owned(),
            lock_probe: Some(lock_probe),
        })
    }

    /// Injects a compatible operator for deterministic contract tests.
    ///
    /// This constructor omits AWS management probes and must not be used for a production S3
    /// endpoint. Exact version identity is still mandatory.
    #[doc(hidden)]
    pub fn from_operator(op: Operator, prefix: impl Into<String>) -> Self {
        Self {
            op,
            prefix: prefix.into().trim_matches('/').to_owned(),
            lock_probe: None,
        }
    }

    async fn inspect_lock(
        &self,
        key: &str,
        version: &str,
    ) -> Result<ObjectLockStatus, DeleteErrorV2> {
        let Some(probe) = &self.lock_probe else {
            return Ok(ObjectLockStatus::Unlocked);
        };
        probe
            .inspect_object_lock(key, version)
            .await
            .map_err(|error| probe_error(key, error))
    }

    async fn permission_denied_after_probe(
        &self,
        relative_key: &str,
        full_key: &str,
        version: &str,
    ) -> DeleteErrorV2 {
        match self.inspect_lock(full_key, version).await {
            Ok(ObjectLockStatus::LegalHold) => DeleteErrorV2::RetentionLocked {
                key: relative_key.to_owned(),
                retention_until_ms: None,
            },
            Ok(ObjectLockStatus::RetainedUntil(retention_until_ms)) => {
                DeleteErrorV2::RetentionLocked {
                    key: relative_key.to_owned(),
                    retention_until_ms: Some(retention_until_ms),
                }
            }
            Ok(ObjectLockStatus::Unlocked) => DeleteErrorV2::Backend {
                key: relative_key.to_owned(),
                message:
                    "S3 delete permission denied while Object Lock and legal hold probes report unlocked"
                        .to_owned(),
                retryable: false,
            },
            Err(error) => error,
        }
    }
}

#[async_trait]
impl DestinationDeleteV2 for S3DeleteOperatorV2 {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        if !valid_relative_key(key) {
            return Err(terminal_input_error(key, "invalid relative S3 object key"));
        }
        let Some(version) = version else {
            return Err(terminal_input_error(
                key,
                "exact S3 object version is required; a versionless delete would only create a delete marker",
            ));
        };
        if !valid_version(version) {
            return Err(terminal_input_error(key, "invalid S3 object version"));
        }

        let full_key = object_key(&self.prefix, key);
        match self.inspect_lock(&full_key, version).await? {
            ObjectLockStatus::LegalHold => {
                return Err(DeleteErrorV2::RetentionLocked {
                    key: key.to_owned(),
                    retention_until_ms: None,
                });
            }
            ObjectLockStatus::RetainedUntil(retention_until_ms) => {
                return Err(DeleteErrorV2::RetentionLocked {
                    key: key.to_owned(),
                    retention_until_ms: Some(retention_until_ms),
                });
            }
            ObjectLockStatus::Unlocked => {}
        }

        let delete_result = bounded_s3_delete_request(key, S3_DELETE_REQUEST_TIMEOUT, async {
            self.op.delete_with(&full_key).version(version).await
        })
        .await?;
        match delete_result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => Err(self
                .permission_denied_after_probe(key, &full_key, version)
                .await),
            Err(error) => {
                let retryable = error.kind() == ErrorKind::RateLimited || error.is_temporary();
                Err(DeleteErrorV2::Backend {
                    key: key.to_owned(),
                    message: bounded_diagnostic(&error),
                    retryable,
                })
            }
        }
    }
}

async fn bounded_s3_delete_request<T>(
    key: &str,
    timeout: Duration,
    future: impl Future<Output = T>,
) -> Result<T, DeleteErrorV2> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| DeleteErrorV2::Backend {
            key: key.to_owned(),
            message: "S3_DELETE_REQUEST_TIMEOUT".to_owned(),
            retryable: true,
        })
}

fn validate_config(cfg: &S3DeleteCfg) -> Result<(), S3Error> {
    for (field, value) in [
        ("region", cfg.region.as_str()),
        ("bucket", cfg.bucket.as_str()),
        ("access_key_id", cfg.access_key_id.as_str()),
        ("secret_access_key", cfg.secret_access_key.as_str()),
    ] {
        if value.trim().is_empty() || value.contains(['\0', '\r', '\n']) {
            return Err(S3Error::DeleteConfigInvalid(format!(
                "{field} is empty or contains a control character"
            )));
        }
    }
    if !cfg.prefix.is_empty()
        && (!valid_relative_key(cfg.prefix.trim_matches('/'))
            || cfg.prefix.starts_with("//")
            || cfg.prefix.ends_with("//"))
    {
        return Err(S3Error::DeleteConfigInvalid(
            "prefix is not a safe relative object prefix".to_owned(),
        ));
    }
    Ok(())
}

fn terminal_input_error(key: &str, message: &str) -> DeleteErrorV2 {
    DeleteErrorV2::Backend {
        key: key.to_owned(),
        message: message.to_owned(),
        retryable: false,
    }
}

fn probe_error(key: &str, error: ObjectLockProbeError) -> DeleteErrorV2 {
    DeleteErrorV2::Backend {
        key: key.to_owned(),
        message: bounded_text(&error.message),
        retryable: error.retryable,
    }
}

fn bounded_diagnostic(error: &opendal::Error) -> String {
    bounded_text(&format!(
        "S3 delete failed with {}: {}",
        error.kind(),
        error.message()
    ))
}

fn bounded_text(value: &str) -> String {
    value.chars().take(2_048).collect()
}

fn valid_relative_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !key.chars().any(|value| matches!(value, '\0' | '\r' | '\n'))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 1_024
        && !version
            .chars()
            .any(|value| matches!(value, '\0' | '\r' | '\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_policy_is_version_specific_and_cannot_bypass_retention() {
        let policy: serde_json::Value =
            serde_json::from_str(DELETE_ONLY_IAM_POLICY).expect("valid IAM policy JSON");
        let statements = policy["Statement"].as_array().expect("statements");
        let allow = statements
            .iter()
            .find(|statement| statement["Effect"] == "Allow")
            .expect("allow statement");
        let allowed = allow["Action"].as_array().expect("allowed actions");
        assert!(allowed
            .iter()
            .any(|action| action == "s3:DeleteObjectVersion"));
        assert!(!allowed.iter().any(|action| action == "s3:PutObject"));
        assert!(!allowed.iter().any(|action| action == "s3:DeleteObject"));

        let deny = statements
            .iter()
            .find(|statement| statement["Effect"] == "Deny")
            .expect("deny statement");
        let denied = deny["Action"].as_array().expect("denied actions");
        assert!(denied
            .iter()
            .any(|action| action == "s3:BypassGovernanceRetention"));
    }

    #[test]
    fn backend_error_retryability_uses_temporary_status_not_permission_guessing() {
        let temporary = opendal::Error::new(ErrorKind::Unexpected, "network").set_temporary();
        assert!(matches!(
            DeleteErrorV2::Backend {
                key: "key".to_owned(),
                message: bounded_diagnostic(&temporary),
                retryable: temporary.is_temporary(),
            },
            DeleteErrorV2::Backend {
                retryable: true,
                ..
            }
        ));

        let denied = opendal::Error::new(ErrorKind::PermissionDenied, "denied");
        assert!(!denied.is_temporary());
        assert_ne!(denied.kind(), ErrorKind::RateLimited);
    }

    #[tokio::test]
    async fn pending_s3_delete_request_is_bounded_with_a_retryable_stable_code() {
        let error = bounded_s3_delete_request(
            "set/manifest.json",
            Duration::from_millis(10),
            std::future::pending::<()>(),
        )
        .await
        .expect_err("pending S3 request must time out");

        assert!(matches!(
            error,
            DeleteErrorV2::Backend {
                message,
                retryable: true,
                ..
            } if message == "S3_DELETE_REQUEST_TIMEOUT"
        ));
    }
}
