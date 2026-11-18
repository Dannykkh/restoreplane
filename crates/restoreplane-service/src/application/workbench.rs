use super::ServiceApplication;
use restoreplane_application::{canonical_payload_bytes, ApplicationError, ApplicationResponse};
use restoreplane_catalog::RepositoryError;
use restoreplane_domain::workbench::{
    AuthorizePurpose, AuthorizeRequest, BindingReceipt, DecimalU64,
};
use serde_json::Value;

impl ServiceApplication {
    pub(super) fn workbench_authorize_owner(
        &self,
        payload: Value,
        owner_sid_hash: &str,
        accepted_at_ms: i64,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: AuthorizeRequest =
            serde_json::from_value(payload).map_err(|_| ApplicationError::InvalidPayload)?;
        if accepted_at_ms < 0 || input.app_nonce.is_nil() {
            return Err(ApplicationError::InvalidPayload);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let observed = store
            .workbench_binding()
            .snapshot(&input.scope(), owner_sid_hash)
            .map_err(|error| match error {
                RepositoryError::WorkbenchBindingUnproven => {
                    ApplicationError::WorkbenchBindingUnproven
                }
                RepositoryError::WorkspaceScopeMismatch
                | RepositoryError::NotFound
                | RepositoryError::SecurityInputInvalid => ApplicationError::Forbidden,
                _ => ApplicationError::Internal,
            })?;
        let revision = descriptor_revision(observed.revision_input)?;
        if input
            .expected_descriptor_revision
            .as_ref()
            .is_some_and(|expected| expected != &revision)
        {
            return Err(ApplicationError::WorkbenchBindingStale);
        }
        if input.purpose.is_mutating() && !observed.observed_conflicts.is_empty() {
            return Err(ApplicationError::WorkbenchTargetBusy);
        }
        let receipt = BindingReceipt {
            scope: input.scope(),
            descriptor: observed.descriptor,
            descriptor_revision: revision,
            authority_epoch: self.health.snapshot().boot_id().to_string(),
            authorized_at_ms: DecimalU64::from(accepted_at_ms as u64),
            read_lease_expires_at_ms: if input.purpose == AuthorizePurpose::Read {
                Some(DecimalU64::from(
                    (accepted_at_ms as u64).saturating_add(30_000),
                ))
            } else {
                None
            },
            observed_conflicts: observed.observed_conflicts,
        };
        Ok(ApplicationResponse {
            deduplicated: false,
            resource: None,
            body: serde_json::to_value(receipt).map_err(|_| ApplicationError::Internal)?,
        })
    }
}

pub(super) fn descriptor_revision(mut input: Value) -> Result<String, ApplicationError> {
    fn canonical_integers(value: &mut Value) {
        match value {
            Value::Number(n) => *value = Value::String(n.to_string()),
            Value::Object(map) => {
                for value in map.values_mut() {
                    canonical_integers(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    canonical_integers(value);
                }
            }
            _ => {}
        }
    }
    canonical_integers(&mut input);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"restoreplane.workbench.binding.v1\0");
    hasher.update(&canonical_payload_bytes(&input)?);
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_is_order_stable_and_binds_changes_to_credentials_and_tls() {
        let a = serde_json::json!({"scope":{"workspace":"one"},"secretVersion":1,"tls":{"root":"system","mode":"verify-full"}});
        let b = serde_json::json!({"tls":{"mode":"verify-full","root":"system"},"secretVersion":1,"scope":{"workspace":"one"}});
        assert_eq!(
            descriptor_revision(a.clone()).unwrap(),
            descriptor_revision(b).unwrap()
        );
        for modified in [
            serde_json::json!({"scope":{"workspace":"one"},"secretVersion":2,"tls":{"root":"system","mode":"verify-full"}}),
            serde_json::json!({"scope":{"workspace":"two"},"secretVersion":1,"tls":{"root":"system","mode":"verify-full"}}),
        ] {
            assert_ne!(
                descriptor_revision(a.clone()).unwrap(),
                descriptor_revision(modified).unwrap()
            );
        }
    }
}
