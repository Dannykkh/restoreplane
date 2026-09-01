use uuid::Uuid;

use crate::{
    BackupPlanId, DatabaseTargetId, DestinationId, DestinationRequirement, DomainError,
    OccurrenceOrigin, PlanOccurrenceId, PlanRevisionId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionDestination {
    destination_id: DestinationId,
    requirement: DestinationRequirement,
}

impl RevisionDestination {
    pub fn destination_id(&self) -> &DestinationId {
        &self.destination_id
    }

    pub const fn requirement(&self) -> DestinationRequirement {
        self.requirement
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRevisionDraft {
    id: PlanRevisionId,
    plan_id: BackupPlanId,
    targets: Vec<DatabaseTargetId>,
    destinations: Vec<RevisionDestination>,
}

impl PlanRevisionDraft {
    pub fn new(id: PlanRevisionId, plan_id: BackupPlanId) -> Self {
        Self {
            id,
            plan_id,
            targets: Vec::new(),
            destinations: Vec::new(),
        }
    }

    pub fn with_target(mut self, target_id: DatabaseTargetId) -> Result<Self, DomainError> {
        if self.targets.contains(&target_id) {
            return Err(DomainError::PlanRevisionTargetDuplicate);
        }
        self.targets.push(target_id);
        Ok(self)
    }

    pub fn with_destination(
        mut self,
        destination_id: DestinationId,
        requirement: DestinationRequirement,
    ) -> Result<Self, DomainError> {
        if self
            .destinations
            .iter()
            .any(|item| item.destination_id == destination_id)
        {
            return Err(DomainError::PlanRevisionDestinationDuplicate);
        }
        self.destinations.push(RevisionDestination {
            destination_id,
            requirement,
        });
        Ok(self)
    }

    pub fn targets(&self) -> &[DatabaseTargetId] {
        &self.targets
    }

    pub fn destinations(&self) -> &[RevisionDestination] {
        &self.destinations
    }

    pub fn publish(self, published_at_ms: i64) -> Result<PublishedPlanRevision, DomainError> {
        if published_at_ms < 0 {
            return Err(DomainError::PlanRevisionPublishedAtInvalid);
        }
        if self.targets.is_empty() {
            return Err(DomainError::PlanRevisionTargetRequired);
        }
        if self.destinations.is_empty() {
            return Err(DomainError::PlanRevisionDestinationRequired);
        }
        if !self
            .destinations
            .iter()
            .any(|item| item.requirement == DestinationRequirement::Required)
        {
            return Err(DomainError::PlanRevisionRequiredDestinationRequired);
        }
        Ok(PublishedPlanRevision {
            id: self.id,
            plan_id: self.plan_id,
            targets: self.targets,
            destinations: self.destinations,
            published_at_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedPlanRevision {
    id: PlanRevisionId,
    plan_id: BackupPlanId,
    targets: Vec<DatabaseTargetId>,
    destinations: Vec<RevisionDestination>,
    published_at_ms: i64,
}

impl PublishedPlanRevision {
    pub fn id(&self) -> &PlanRevisionId {
        &self.id
    }

    pub fn plan_id(&self) -> &BackupPlanId {
        &self.plan_id
    }

    pub fn targets(&self) -> &[DatabaseTargetId] {
        &self.targets
    }

    pub fn destinations(&self) -> &[RevisionDestination] {
        &self.destinations
    }

    pub const fn published_at_ms(&self) -> i64 {
        self.published_at_ms
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OccurrenceIdentity {
    Scheduled {
        plan_revision_id: PlanRevisionId,
        scheduled_for_ms: i64,
    },
    Manual {
        request_idempotency_key: String,
    },
}

impl OccurrenceIdentity {
    pub fn scheduled(
        plan_revision_id: PlanRevisionId,
        origin: OccurrenceOrigin,
        scheduled_for_ms: i64,
    ) -> Result<Self, DomainError> {
        if !matches!(
            origin,
            OccurrenceOrigin::Scheduled | OccurrenceOrigin::CatchUp
        ) {
            return Err(DomainError::OccurrenceOriginInvalid);
        }
        if scheduled_for_ms < 0 {
            return Err(DomainError::OccurrenceScheduledTimeInvalid);
        }
        Ok(Self::Scheduled {
            plan_revision_id,
            scheduled_for_ms,
        })
    }

    pub fn manual(request_idempotency_key: impl Into<String>) -> Result<Self, DomainError> {
        let request_idempotency_key = request_idempotency_key.into();
        if request_idempotency_key.is_empty()
            || request_idempotency_key.len() > 256
            || request_idempotency_key.trim() != request_idempotency_key
        {
            return Err(DomainError::OccurrenceIdempotencyKeyInvalid);
        }
        Ok(Self::Manual {
            request_idempotency_key,
        })
    }

    pub fn deterministic_id(&self, namespace: Uuid) -> PlanOccurrenceId {
        let canonical_name = match self {
            Self::Scheduled {
                plan_revision_id,
                scheduled_for_ms,
            } => format!("scheduled:{plan_revision_id}:{scheduled_for_ms}"),
            Self::Manual {
                request_idempotency_key,
            } => format!("manual:{request_idempotency_key}"),
        };
        PlanOccurrenceId::from_stable_name(namespace, canonical_name.as_bytes())
    }
}
