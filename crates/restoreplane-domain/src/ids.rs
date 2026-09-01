use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::DomainError;

macro_rules! typed_uuid {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct $name(String);

            impl Default for $name {
                fn default() -> Self {
                    Self::new()
                }
            }

            impl $name {
                pub fn new() -> Self {
                    Self(Uuid::new_v4().to_string())
                }

                pub fn from_stable_name(namespace: Uuid, canonical_name: &[u8]) -> Self {
                    Self(Uuid::new_v5(&namespace, canonical_name).to_string())
                }

                pub fn as_str(&self) -> &str {
                    &self.0
                }

                pub fn as_uuid(&self) -> Uuid {
                    Uuid::parse_str(&self.0).expect("typed UUID invariant")
                }
            }

            impl FromStr for $name {
                type Err = DomainError;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    let parsed = Uuid::parse_str(value).map_err(|_| DomainError::InvalidId {
                        type_name: stringify!($name),
                    })?;
                    if parsed.to_string() != value {
                        return Err(DomainError::NonCanonicalId {
                            type_name: stringify!($name),
                        });
                    }
                    Ok(Self(value.to_owned()))
                }
            }

            impl TryFrom<String> for $name {
                type Error = DomainError;

                fn try_from(value: String) -> Result<Self, Self::Error> {
                    Self::from_str(&value)
                }
            }

            impl AsRef<str> for $name {
                fn as_ref(&self) -> &str {
                    self.as_str()
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str(&self.0)
                }
            }

            impl Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: Serializer,
                {
                    serializer.serialize_str(self.as_str())
                }
            }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: Deserializer<'de>,
                {
                    let value = String::deserialize(deserializer)?;
                    Self::from_str(&value).map_err(serde::de::Error::custom)
                }
            }
        )+
    };
}

typed_uuid!(
    WorkspaceId,
    ConnectionProfileId,
    DatabaseTargetId,
    BackupScopeId,
    DestinationId,
    BackupPlanId,
    PlanRevisionId,
    PlanOccurrenceId,
    BackupRunId,
    RestoreRunId,
    VerificationRunId,
    CatalogRebuildRunId,
    OperationRunId,
    WorkItemId,
    WorkAttemptId,
    RecoveryPointId,
    ArtifactPartId,
    ArtifactReplicaId,
    StoredObjectId,
    SecretRefId,
    ConfirmationTokenId,
    IpcRequestId,
);
