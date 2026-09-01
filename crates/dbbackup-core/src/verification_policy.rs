use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestedVerificationLevel {
    L1,
    L2,
    L3,
}

impl RequestedVerificationLevel {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::L1 => 1,
            Self::L2 => 2,
            Self::L3 => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationSchedule {
    EveryRun,
    Weekly,
    Cron(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationPolicy {
    requested_level: RequestedVerificationLevel,
    schedule: VerificationSchedule,
}

impl VerificationPolicy {
    pub fn from_json(input: &str) -> Result<Self, VerificationPolicyError> {
        let value: Value =
            serde_json::from_str(input).map_err(|_| VerificationPolicyError::InvalidJson)?;
        let object = value
            .as_object()
            .ok_or(VerificationPolicyError::InvalidShape)?;
        reject_unknown_fields(object)?;

        let level = match (object.get("level"), object.get("requestedLevel")) {
            (Some(_), Some(_)) => return Err(VerificationPolicyError::AmbiguousLevel),
            (Some(value), None) | (None, Some(value)) => parse_level(value)?,
            (None, None) => RequestedVerificationLevel::L2,
        };
        let schedule = object
            .get("schedule")
            .map(parse_schedule)
            .transpose()?
            .unwrap_or(VerificationSchedule::EveryRun);

        Ok(Self {
            requested_level: level,
            schedule,
        })
    }

    pub const fn requested_level(&self) -> RequestedVerificationLevel {
        self.requested_level
    }

    pub fn schedule(&self) -> &VerificationSchedule {
        &self.schedule
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VerificationPolicyError {
    #[error("VERIFICATION_POLICY_JSON_INVALID")]
    InvalidJson,
    #[error("VERIFICATION_POLICY_SHAPE_INVALID")]
    InvalidShape,
    #[error("VERIFICATION_POLICY_FIELD_UNKNOWN")]
    UnknownField,
    #[error("VERIFICATION_POLICY_LEVEL_AMBIGUOUS")]
    AmbiguousLevel,
    #[error("VERIFICATION_POLICY_LEVEL_INVALID")]
    InvalidLevel,
    #[error("VERIFICATION_POLICY_SCHEDULE_INVALID")]
    InvalidSchedule,
}

impl VerificationPolicyError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson => "VERIFICATION_POLICY_JSON_INVALID",
            Self::InvalidShape => "VERIFICATION_POLICY_SHAPE_INVALID",
            Self::UnknownField => "VERIFICATION_POLICY_FIELD_UNKNOWN",
            Self::AmbiguousLevel => "VERIFICATION_POLICY_LEVEL_AMBIGUOUS",
            Self::InvalidLevel => "VERIFICATION_POLICY_LEVEL_INVALID",
            Self::InvalidSchedule => "VERIFICATION_POLICY_SCHEDULE_INVALID",
        }
    }
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), VerificationPolicyError> {
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "level" | "requestedLevel" | "schedule" | "executionAuthority"
        )
    }) {
        return Err(VerificationPolicyError::UnknownField);
    }
    if object
        .get("executionAuthority")
        .is_some_and(|value| !value.is_string())
    {
        return Err(VerificationPolicyError::InvalidShape);
    }
    Ok(())
}

fn parse_level(value: &Value) -> Result<RequestedVerificationLevel, VerificationPolicyError> {
    match value {
        Value::Number(number) => match number.as_u64() {
            Some(1) => Ok(RequestedVerificationLevel::L1),
            Some(2) => Ok(RequestedVerificationLevel::L2),
            Some(3) => Ok(RequestedVerificationLevel::L3),
            _ => Err(VerificationPolicyError::InvalidLevel),
        },
        Value::String(level) => match level.to_ascii_lowercase().as_str() {
            "l1" | "1" => Ok(RequestedVerificationLevel::L1),
            "l2" | "2" => Ok(RequestedVerificationLevel::L2),
            "l3" | "3" => Ok(RequestedVerificationLevel::L3),
            _ => Err(VerificationPolicyError::InvalidLevel),
        },
        _ => Err(VerificationPolicyError::InvalidLevel),
    }
}

fn parse_schedule(value: &Value) -> Result<VerificationSchedule, VerificationPolicyError> {
    let schedule = value
        .as_str()
        .ok_or(VerificationPolicyError::InvalidSchedule)?;
    match schedule {
        "every_run" => Ok(VerificationSchedule::EveryRun),
        "weekly" => Ok(VerificationSchedule::Weekly),
        value if !value.is_empty() && value.len() <= 256 && !value.contains(['\0', '\r', '\n']) => {
            Ok(VerificationSchedule::Cron(value.to_owned()))
        }
        _ => Err(VerificationPolicyError::InvalidSchedule),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_current_and_shadow_policy_shapes_without_downgrade() {
        let current = VerificationPolicy::from_json(r#"{"level":"l3"}"#).expect("current");
        let shadow = VerificationPolicy::from_json(
            r#"{"requestedLevel":3,"schedule":"weekly","executionAuthority":"legacy_direct"}"#,
        )
        .expect("shadow");

        assert_eq!(current.requested_level(), RequestedVerificationLevel::L3);
        assert_eq!(shadow.requested_level(), RequestedVerificationLevel::L3);
        assert_eq!(shadow.schedule(), &VerificationSchedule::Weekly);
    }

    #[test]
    fn empty_policy_preserves_the_product_default() {
        let policy = VerificationPolicy::from_json("{}").expect("default policy");
        assert_eq!(policy.requested_level(), RequestedVerificationLevel::L2);
        assert_eq!(policy.schedule(), &VerificationSchedule::EveryRun);
    }

    #[test]
    fn rejects_ambiguous_invalid_and_unknown_policy_instead_of_guessing() {
        for (input, expected) in [
            (
                r#"{"level":"l1","requestedLevel":2}"#,
                VerificationPolicyError::AmbiguousLevel,
            ),
            (r#"{"level":"l4"}"#, VerificationPolicyError::InvalidLevel),
            (
                r#"{"level":"l2","silentFallback":"l1"}"#,
                VerificationPolicyError::UnknownField,
            ),
        ] {
            assert_eq!(
                VerificationPolicy::from_json(input).expect_err("must reject"),
                expected
            );
        }
    }
}
