use std::str::FromStr;

use restoreplane_domain::SecretRefId;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::OperationalStore;

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct SecretRefSpec {
    id: SecretRefId,
    provider_locator: String,
    purpose: String,
    version: i64,
    fingerprint: String,
    rotated_from_id: Option<SecretRefId>,
    created_at_ms: i64,
}

impl SecretRefSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn windows_dpapi(
        id: SecretRefId,
        provider_locator: impl Into<String>,
        purpose: impl Into<String>,
        version: i64,
        fingerprint: impl Into<String>,
        rotated_from_id: Option<SecretRefId>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            provider_locator: provider_locator.into(),
            purpose: purpose.into(),
            version,
            fingerprint: fingerprint.into(),
            rotated_from_id,
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.provider_locator.is_empty()
            || self.provider_locator.len() > 512
            || self.purpose.is_empty()
            || self.purpose.len() > 128
            || self.version <= 0
            || !is_digest(&self.fingerprint)
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretRefRecord {
    id: SecretRefId,
    provider: String,
    provider_locator: String,
    purpose: String,
    version: i64,
    state: String,
    fingerprint: String,
    rotated_from_id: Option<SecretRefId>,
    created_at_ms: i64,
    retired_at_ms: Option<i64>,
}

impl SecretRefRecord {
    pub const fn id(&self) -> &SecretRefId {
        &self.id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn provider_locator(&self) -> &str {
        &self.provider_locator
    }

    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    pub const fn version(&self) -> i64 {
        self.version
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub const fn rotated_from_id(&self) -> Option<&SecretRefId> {
        self.rotated_from_id.as_ref()
    }

    pub const fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    pub const fn retired_at_ms(&self) -> Option<i64> {
        self.retired_at_ms
    }
}

#[derive(Clone)]
pub struct SecretRefRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn secret_refs(&self) -> SecretRefRepository {
        SecretRefRepository {
            store: self.clone(),
        }
    }
}

impl SecretRefRepository {
    pub fn create(&self, spec: SecretRefSpec) -> Result<SecretRefRecord, RepositoryError> {
        spec.validate()?;
        if spec.version != 1 || spec.rotated_from_id.is_some() {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let id = spec.id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            insert_secret_ref(&transaction, &spec)?;
            let record = read_secret_ref(&transaction, &id)?;
            transaction.commit()?;
            Ok(record)
        })
    }

    pub fn rotate(
        &self,
        previous_id: &SecretRefId,
        next: SecretRefSpec,
        rotated_at_ms: i64,
    ) -> Result<SecretRefRecord, RepositoryError> {
        next.validate()?;
        if rotated_at_ms < 0 || next.rotated_from_id.as_ref() != Some(previous_id) {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let previous_id = previous_id.clone();
        let next_id = next.id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let previous = read_secret_ref(&transaction, &previous_id)?;
            if previous.provider != "windows_dpapi"
                || previous.state != "active"
                || previous.provider_locator != next.provider_locator
                || previous.purpose != next.purpose
                || previous.version.checked_add(1) != Some(next.version)
                || rotated_at_ms < previous.created_at_ms
                || next.created_at_ms < rotated_at_ms
            {
                return Err(RepositoryError::SecurityInputInvalid);
            }
            insert_secret_ref(&transaction, &next)?;
            let retired = transaction.execute(
                "UPDATE secret_refs
                 SET state = 'retired', retired_at_ms = ?2
                 WHERE id = ?1 AND state = 'active'",
                params![previous_id.as_str(), rotated_at_ms],
            )?;
            if retired != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let record = read_secret_ref(&transaction, &next_id)?;
            transaction.commit()?;
            Ok(record)
        })
    }

    pub fn get(&self, id: &SecretRefId) -> Result<SecretRefRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        read_secret_ref(reader.connection(), id)
    }

    pub fn active_for_purpose(
        &self,
        purpose: &str,
    ) -> Result<Option<SecretRefRecord>, RepositoryError> {
        if !matches!(purpose, "signing_key" | "encryption_key") {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let mut statement = connection.prepare(
            "SELECT id
             FROM secret_refs
             WHERE purpose = ?1 AND state = 'active'
             ORDER BY created_at_ms DESC, id DESC
             LIMIT 2",
        )?;
        let ids = statement
            .query_map([purpose], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > 1 {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        ids.first()
            .map(|id| {
                SecretRefId::from_str(id)
                    .map_err(RepositoryError::from)
                    .and_then(|id| read_secret_ref(connection, &id))
            })
            .transpose()
    }

    pub fn revoke(&self, id: &SecretRefId, revoked_at_ms: i64) -> Result<(), RepositoryError> {
        if revoked_at_ms < 0 {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let id = id.clone();
        self.write(move |connection| {
            let updated = connection.execute(
                "UPDATE secret_refs
                 SET state = 'revoked', retired_at_ms = COALESCE(retired_at_ms, ?2)
                 WHERE id = ?1 AND state IN ('active', 'retired')",
                params![id.as_str(), revoked_at_ms],
            )?;
            if updated != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn insert_secret_ref(
    transaction: &Transaction<'_>,
    spec: &SecretRefSpec,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "INSERT INTO secret_refs(
             id, provider, provider_locator, purpose, version, state,
             fingerprint, rotated_from_id, created_at_ms
         ) VALUES (?1, 'windows_dpapi', ?2, ?3, ?4, 'active', ?5, ?6, ?7)",
        params![
            spec.id.as_str(),
            spec.provider_locator,
            spec.purpose,
            spec.version,
            spec.fingerprint,
            spec.rotated_from_id.as_ref().map(SecretRefId::as_str),
            spec.created_at_ms
        ],
    )?;
    Ok(())
}

fn read_secret_ref(
    connection: &rusqlite::Connection,
    id: &SecretRefId,
) -> Result<SecretRefRecord, RepositoryError> {
    type SecretRefRow = (
        String,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
        i64,
        Option<i64>,
    );

    let row: SecretRefRow = connection
        .query_row(
            "SELECT provider, provider_locator, purpose, version, state, fingerprint,
                    rotated_from_id, created_at_ms, retired_at_ms
             FROM secret_refs
             WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(RepositoryError::NotFound)?;
    let rotated_from_id = row
        .6
        .map(|value| SecretRefId::from_str(&value))
        .transpose()?;
    Ok(SecretRefRecord {
        id: id.clone(),
        provider: row.0,
        provider_locator: row.1,
        purpose: row.2,
        version: row.3,
        state: row.4,
        fingerprint: row.5,
        rotated_from_id,
        created_at_ms: row.7,
        retired_at_ms: row.8,
    })
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
